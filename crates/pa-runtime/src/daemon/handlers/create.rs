//! `session.create` ([`pa_types::wire2::SessionCreate`]): the session named
//! (by id, by transcript, or a new one), its lease or its owner, its
//! options, and its build. Beside the build's own choices it carries what the
//! old supervisor's create carried:
//!
//! - `sessionDir`: a new session's transcript is made there instead of in the
//!   daemon's session dir.
//! - `launchEnv`: the client's environment, which the session runs in over
//!   the daemon's own, minus the daemon-internal variables and `RLM_DEPTH`
//!   (a client running inside an agent carries its depth, which a root
//!   session must not take). It is kept in memory for the session's
//!   lifetimes in this process, never stored, as the old supervisor kept it
//!   for its worker.
//! - the options' `apiKey`: held in memory for the daemon's life, never
//!   written (the stored options keep only that one was given), so a session
//!   opened again without one in this process runs on the same key, and one
//!   a later daemon builds from its stored options fails rather than run on
//!   another key.
//! - the options' `models` (else the settings' `enabledModels`): the
//!   session's model scope, resolved at its open (`daemon::model_scope`); a
//!   new session that chose no model starts on the scope's pick.
//! - the options' `credential`, `profile`, `parent` and `telemetryDisabled`,
//!   which the session's build reads (the embedder's `SessionBuilder`).
//!
//! A create of an open session answers it as it runs (`"reused": true`),
//! except one the daemon is letting go of (its supervision gave it up, and
//! the daemon releases it as it hears its `agent/<id>/failed`, which the
//! client may hear first; or its close is under way): the create waits for
//! the release and opens it afresh (`"reused": false`).

use std::path::PathBuf;

use anyhow::Context as _;
use pa_core::session::hold_refusal::HoldRefusal;
use pa_core::session::lease::acquire_runtime_session_lease;
use pa_core::session::manager::get_session_file_path;
use pa_core::session::store::store;
use pa_types::wire2::SessionCreate;
use serde_json::{json, Value};

use super::super::held::OpenSession;
use super::RuntimeHandlers;
use crate::session::{SessionBuilder, SessionSpec};
use crate::wire::{ConnectionId, ErrorWithInfo};

/// How long a create of a session being let go of waits for the daemon to
/// release it before it is refused.
const GIVEN_UP_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl<B: SessionBuilder> RuntimeHandlers<B> {
    #[expect(
        clippy::too_many_lines,
        reason = "a create's steps read in the order they run: the session named, its lease or \
                  owner, its options and scope, its build"
    )]
    pub(super) async fn create(
        &self,
        args: Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let SessionCreate {
            session: named,
            path: given_path,
            cwd: given_cwd,
            ephemeral,
            session_dir,
            name,
            launch_env: given_env,
            options,
        } = serde_json::from_value(args).context("the session.create arguments")?;
        let given_path = given_path.map(PathBuf::from);
        let options = match options {
            None | Some(Value::Null) => None,
            Some(options) => Some(crate::SessionOptions::from_json(&options)?),
        };
        let session_dir = session_dir.map_or_else(|| self.session_dir.clone(), PathBuf::from);
        let (id, path) = match (named, given_path) {
            (Some(_), Some(_)) => {
                anyhow::bail!("give session.create a `session` or a `path`, not both")
            }
            (_, Some(_)) if ephemeral => {
                anyhow::bail!("an ephemeral session has no transcript: give it no `path`")
            }
            // Open already: its transcript may not have its header yet.
            (None, Some(path)) => {
                if let Some(id) = self.open.id_of(&path) {
                    (id, path)
                } else {
                    let header = pa_core::session::manager::read_session_header(&path)
                        .with_context(|| format!("no session header in {}", path.display()))?;
                    (header.id, path)
                }
            }
            (Some(id), None) => {
                let path = get_session_file_path(&session_dir, &id);
                (id, path)
            }
            (None, None) => {
                let id = uuid::Uuid::now_v7().to_string();
                let path = get_session_file_path(&session_dir, &id);
                (id, path)
            }
        };
        // Held but no longer run (given up on, which the daemon releases as
        // it hears its `agent/<id>/failed` and a client may have heard
        // first, or a close under way): the create waits for the release
        // and opens it as a session of its own, never answering the one
        // being let go of as reused.
        if self.open.contains(&id) && !self.runtime.runs(&id) {
            tokio::time::timeout(GIVEN_UP_RELEASE_WAIT, self.open.until_released(&id))
                .await
                .with_context(|| format!("session {id}, being let go of, was not released"))?;
        }
        // Open already (the boot revived it, or another client opened it):
        // answered as it runs, the old daemon's create-open reuse.
        if let Some(path) = self.open.path(&id) {
            let cwd = self.open.cwd(&id);
            return Ok(json!({ "session": id, "path": path, "cwd": cwd, "reused": true }));
        }
        // A settled child its parent's family opened again is passive, as the
        // old daemon's ledger child: opened by its transcript, it runs as its
        // own session.
        if self.runtime.subagents().release_dormant(&id).await {
            self.feeds.due(&id);
        }
        let cwd = match given_cwd {
            Some(cwd) => PathBuf::from(cwd),
            None => pa_core::session::manager::read_session_header(&path)
                .map_or_else(|| self.cwd.clone(), |header| PathBuf::from(header.cwd)),
        };
        let resumed = store().exists(&path);
        // A profile or credential the build could not honour refuses the create before anything
        // is opened, as the old worker's does: the profile must exist and the pin its choices
        // select must resolve.
        if let Some(options) = &options {
            pa_core::auth::AuthAccounts::create(&self.agent_dir)
                .new_session_pin(
                    options.profile.as_deref(),
                    options.provider.as_deref(),
                    options.credential.as_deref(),
                    None,
                )
                .map_err(anyhow::Error::msg)?;
        }
        if ephemeral {
            anyhow::ensure!(
                !resumed,
                "session {id} has a transcript: an ephemeral session is a new one"
            );
        }
        // An ephemeral session has no transcript to lease, and is owned by
        // the client that created it.
        let (lease, owner) = if ephemeral {
            (None, Some(self.attached.client_of(connection)))
        } else {
            let (path, agent_dir) = (path.clone(), self.agent_dir.clone());
            // A live holder's refusal answers with the hold refusal and its
            // `errorInfo`, as the old worker's create does.
            let lease = tokio::task::spawn_blocking(move || {
                acquire_runtime_session_lease(&path, &agent_dir)
            })
            .await
            .context("the lease's thread panicked")?
            .map_err(|error| match HoldRefusal::of(&error) {
                Some(refusal) => anyhow::Error::new(ErrorWithInfo {
                    message: refusal.message,
                    info: serde_json::to_value(refusal.info).unwrap_or(Value::Null),
                }),
                None => error,
            })?;
            (Some(lease), None)
        };
        // Given options are kept for the session's later builds (its
        // revival, a scheduled wake); a session created again without any
        // keeps the ones it was created with. An ephemeral session is never
        // built again: its options are not kept. A given API key is held in
        // memory only.
        let mut options = match options {
            Some(options) if ephemeral => options,
            Some(options) => {
                self.runtime
                    .log()
                    .put_value(
                        super::super::OPTIONS_SCOPE,
                        &format!("options:{id}"),
                        options.to_json().to_string(),
                        pa_log::Origin::Written,
                    )
                    .wait()
                    .await?;
                options
            }
            None => super::super::session_options(self.runtime.log(), &id),
        };
        self.open.keep_api_key(&id, &mut options);
        let scope =
            super::super::model_scope::resolve(&self.agent_dir, &cwd, &options, !resumed).await;
        if let Some(startup) = scope.startup {
            startup.choose(&mut options);
        }
        // A new session's transcript is written at its create, header first (then its name,
        // when the create gives one), as the old worker's create wrote it: a saved-session list, `/resume` or a store read right after
        // the create finds it, and every row it takes from then on is kept as it is appended
        // (a model or level switch before any prompt included).
        if !resumed && !ephemeral {
            let (path, cwd, agent_dir) = (path.clone(), cwd.clone(), self.agent_dir.clone());
            let (id, options) = (id.clone(), options.clone());
            let name = name.filter(|name| !name.trim().is_empty());
            tokio::task::spawn_blocking(move || {
                write_new_transcript(&id, &path, &cwd, &agent_dir, &options, name.as_deref())
            })
            .await
            .context("the new transcript's thread panicked")??;
        }
        // The client's environment, re-filtered (the socket peer is
        // untrusted), under the session's own internal variables.
        let mut env =
            pa_types::daemon::launch_env::collect_launch_env(given_env.unwrap_or_default());
        env.remove("RLM_DEPTH");
        env.extend(super::super::session_env(&self.agent_dir, &id));
        self.runtime
            .create_session(SessionSpec {
                id: Some(id.clone()),
                name: None,
                cwd: cwd.clone(),
                env,
                subagent: None,
                transcript: (!ephemeral).then(|| path.clone()),
                options,
                ephemeral,
            })
            .await?;
        self.open
            .insert(
                id.clone(),
                OpenSession {
                    path: path.clone(),
                    cwd: cwd.clone(),
                    lease,
                    owner,
                    scoped_models: scope.entries,
                    model_fallback_message: scope.fallback_message,
                },
            )
            .await;
        // Its roster entry, for the clients that follow the roster.
        self.feeds.due(&id);
        Ok(json!({ "session": id, "path": path, "cwd": cwd, "reused": false }))
    }
}

/// Writes new session `id`'s transcript at `path`: its header, with the parent, depth,
/// credential pin and profile its build would give it, then its `name`'s `session_info` row.
fn write_new_transcript(
    id: &str,
    path: &std::path::Path,
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    options: &crate::SessionOptions,
    name: Option<&str>,
) -> anyhow::Result<()> {
    let credential_pin = pa_core::auth::AuthAccounts::create(agent_dir)
        .new_session_pin(
            options.profile.as_deref(),
            options.provider.as_deref(),
            options.credential.as_deref(),
            options
                .parent
                .as_ref()
                .map(|parent| std::path::Path::new(&parent.path)),
        )
        .map_err(anyhow::Error::msg)?;
    let session_dir = path.parent().context("a transcript path has a directory")?;
    let mut manager = pa_core::session::manager::SessionManager::persisted(cwd, session_dir);
    manager.new_session(&pa_core::session::manager::NewSessionOptions {
        id: Some(id.to_string()),
        parent_session: options.parent.as_ref().map(|parent| parent.path.clone()),
        // A root session starts at depth 0, never at the `RLM_DEPTH` the daemon was started
        // with (a daemon launched from an agent's command carries it), as the old supervisor's
        // create wrote it.
        rlm_depth: Some(
            options
                .parent
                .as_ref()
                .map_or(0, |parent| u64::from(parent.rlm_depth)),
        ),
        credential_pin,
        profile: options.profile.clone(),
    });
    anyhow::ensure!(
        manager.get_session_file() == Some(path),
        "session {id}'s transcript is not at {}",
        path.display()
    );
    if let Some(name) = name {
        manager
            .append_session_info(name)
            .with_context(|| format!("name session {id}"))?;
    }
    manager
        .flush_now()
        .with_context(|| format!("write the transcript of session {id}"))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use pa_types::session_scope::SessionScope;

    use super::write_new_transcript;

    /// A new root session's transcript is at depth 0 even where `RLM_DEPTH`
    /// is set (here in the scope the write runs in, as a daemon started from
    /// an agent's command has it in its own environment).
    #[test]
    fn a_new_root_transcript_takes_no_depth_from_the_environment() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let _scope = pa_types::session_scope::enter(Some(SessionScope::new(
            "root".to_string(),
            dir.path().to_path_buf(),
            BTreeMap::from([("RLM_DEPTH".to_string(), "3".to_string())]),
            BTreeSet::new(),
        )));
        let path = dir.path().join("sessions").join("root.jsonl");
        write_new_transcript(
            "root",
            &path,
            dir.path(),
            dir.path(),
            &crate::SessionOptions::default(),
            None,
        )
        .expect("the transcript is written");
        let header =
            pa_core::session::manager::read_session_header(&path).expect("the header reads");
        assert_eq!(header.rlm_depth, Some(0));
    }
}
