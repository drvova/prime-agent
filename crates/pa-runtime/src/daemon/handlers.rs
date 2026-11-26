//! The runtime daemon's queries and calls ([`RuntimeHandlers`]).
//!
//! Sessions are addressed by id (`{"session": "<id>"}`): an open session by
//! the transcript it was opened on, any other by `<sessions dir>/<id>.jsonl`.
//! Reads (`sessions.list`, `session.snapshot`, `transcript.page`) read the
//! transcript store, which every row an open session's engine writes reaches
//! as it is written, so they never wait on a running turn. A change to an
//! open session goes through its engine's session manager, on the session's
//! own runtime, once its engine's first build is done; a change to a saved
//! one takes the session's lease for the change, so a session another
//! process holds is refused, never written. `session.create` of a session
//! the daemon holds open already answers it (`"reused": true`);
//! `session.create {"ephemeral": true}` opens one that keeps nothing and
//! ends with its client (`handlers/owned.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session::lease::acquire_runtime_session_lease;
use pa_core::session::manager::{get_session_file_path, load_entries_from_file, SessionManager};
use pa_core::session::store::store;
use pa_types::session::FileEntry;
use serde_json::{json, Value};

use super::held::{OpenSession, OpenSessions};
use super::schedules::Schedules;
use crate::runtime::{AfterAbort, Runtime};
use crate::session::{SessionBuilder, SessionSpec};
use crate::wire::{Attached, Call, ConnectionId, Handlers, Query};
pub(super) use bash::UserBashes;

mod attach;
mod bash;
mod bus;
mod children;
mod input;
mod jobs;
mod mcp;
mod messaging;
mod model;
mod owned;
mod queue;
mod state;
mod summary;

/// The most entries one `transcript.page` returns, and how many it returns
/// when the client names no `limit`.
const PAGE_MAX: usize = 500;
const PAGE_DEFAULT: usize = 100;

/// Answers the runtime daemon's queries and calls.
pub(super) struct RuntimeHandlers<B: SessionBuilder> {
    runtime: Arc<Runtime<B>>,
    open: Arc<OpenSessions>,
    /// The sessions' scheduled jobs: a session stopped or deleted on
    /// purpose is disarmed, so nothing wakes it.
    schedules: Arc<Schedules<B>>,
    /// Which connections follow which sessions: a session's attached
    /// clients.
    attached: Arc<Attached>,
    /// The input holds clients' connections keep on open sessions.
    holds: input::InputHolds,
    agent_dir: PathBuf,
    session_dir: PathBuf,
    cwd: PathBuf,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) fn new(
        runtime: Arc<Runtime<B>>,
        open: Arc<OpenSessions>,
        schedules: Arc<Schedules<B>>,
        attached: Arc<Attached>,
        agent_dir: PathBuf,
        session_dir: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        RuntimeHandlers {
            runtime,
            open,
            schedules,
            attached,
            holds: input::InputHolds::default(),
            agent_dir,
            session_dir,
            cwd,
        }
    }

    /// The transcript of the session `args.session` names, and whether the
    /// daemon holds it open.
    fn session(&self, args: &Value) -> anyhow::Result<(String, PathBuf, bool)> {
        let id = string_arg(args, "session")?;
        if let Some(path) = self.open.path(&id) {
            return Ok((id, path, true));
        }
        let path = get_session_file_path(&self.session_dir, &id);
        Ok((id, path, false))
    }

    /// The saved session `args.session` names: its id and transcript, refused
    /// while the daemon holds it open.
    fn saved_session(&self, args: &Value, call: &str) -> anyhow::Result<(String, PathBuf)> {
        let (id, path, open) = self.session(args)?;
        anyhow::ensure!(
            !open,
            "session {id} is open in the runtime daemon: stop it before {call}"
        );
        Ok((id, path))
    }

    /// Runs `change` on the saved session at `path` under its lease, taken for
    /// the change.
    async fn change_saved<T: Send + 'static>(
        &self,
        path: PathBuf,
        change: impl FnOnce(&mut SessionManager) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            anyhow::ensure!(
                store().exists(&path),
                "no saved session at {}",
                path.display()
            );
            let _lease = acquire_runtime_session_lease(&path, &agent_dir)?;
            let session_dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            let mut manager = SessionManager::open(&cwd, &session_dir, &path);
            change(&mut manager)
        })
        .await
        .context("the change's thread panicked")?
    }

    /// Runs `change` on the session `args.session` names: an open one's on
    /// its engine's session manager, on the session's own runtime (inside its
    /// scope); a saved one's under its lease.
    async fn change(
        &self,
        args: &Value,
        change: impl FnOnce(&mut SessionManager) -> anyhow::Result<Value> + Send + 'static,
    ) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        if !open {
            return self.change_saved(path, change).await;
        }
        let live = self.runtime.built_engine(&id).await?;
        let persistence = live.engine.session.shared_persistence();
        live.runtime
            .spawn(async move { change(&mut *persistence.lock().await) })
            .await
            .context("the session's runtime ended during the change")?
    }

    #[expect(
        clippy::too_many_lines,
        reason = "a create's steps read in the order they run: the session named, its lease or \
                  owner, its options, its build"
    )]
    async fn create(&self, args: Value, connection: ConnectionId) -> anyhow::Result<Value> {
        let named = optional_string_arg(&args, "session")?;
        let ephemeral = match args.get("ephemeral") {
            None | Some(Value::Null | Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => anyhow::bail!("the `ephemeral` argument must be a boolean"),
        };
        let given_path = optional_string_arg(&args, "path")?.map(PathBuf::from);
        let given_cwd = optional_string_arg(&args, "cwd")?.map(PathBuf::from);
        let options = match args.get("options") {
            None | Some(Value::Null) => None,
            Some(options) => Some(crate::SessionOptions::from_json(options)?),
        };
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
                let path = get_session_file_path(&self.session_dir, &id);
                (id, path)
            }
            (None, None) => {
                let id = uuid::Uuid::now_v7().to_string();
                let path = get_session_file_path(&self.session_dir, &id);
                (id, path)
            }
        };
        // Open already (the boot revived it, or another client opened it):
        // answered as it runs, the old daemon's create-open reuse.
        if let Some(path) = self.open.path(&id) {
            let cwd = self.open.cwd(&id);
            return Ok(json!({ "session": id, "path": path, "cwd": cwd, "reused": true }));
        }
        let cwd = match given_cwd {
            Some(cwd) => cwd,
            None => pa_core::session::manager::read_session_header(&path)
                .map_or_else(|| self.cwd.clone(), |header| PathBuf::from(header.cwd)),
        };
        if ephemeral {
            anyhow::ensure!(
                !store().exists(&path),
                "session {id} has a transcript: an ephemeral session is a new one"
            );
        }
        // An ephemeral session has no transcript to lease, and is owned by
        // the client that created it.
        let (lease, owner) = if ephemeral {
            (None, Some(self.attached.client_of(connection)))
        } else {
            let (path, agent_dir) = (path.clone(), self.agent_dir.clone());
            let lease = tokio::task::spawn_blocking(move || {
                acquire_runtime_session_lease(&path, &agent_dir)
            })
            .await
            .context("the lease's thread panicked")??;
            (Some(lease), None)
        };
        // Given options are kept for the session's later builds (its
        // revival, a scheduled wake); a session created again without any
        // keeps the ones it was created with. An ephemeral session is never
        // built again: its options are not kept.
        let options = match options {
            Some(options) if ephemeral => options,
            Some(options) => {
                self.runtime
                    .log()
                    .put_value(
                        super::OPTIONS_SCOPE,
                        &format!("options:{id}"),
                        options.to_json().to_string(),
                        pa_log::Origin::Written,
                    )
                    .wait()
                    .await?;
                options
            }
            None => super::session_options(self.runtime.log(), &id),
        };
        self.runtime
            .create_session(SessionSpec {
                id: Some(id.clone()),
                name: None,
                cwd: cwd.clone(),
                env: super::session_env(&self.agent_dir, &id),
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
                    _lease: lease,
                    owner,
                },
            )
            .await;
        Ok(json!({ "session": id, "path": path, "cwd": cwd, "reused": false }))
    }

    async fn stop(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        anyhow::ensure!(
            self.open.contains(&id),
            "session {id} is not open in the runtime daemon"
        );
        // Stopped on purpose: its jobs wake nothing until it is opened again.
        self.schedules.disarm(&id);
        // Its `!` command is killed first, and none starts while it stops.
        let _closing = self.open.bashes.close(&id).await;
        self.runtime.close_session(&id).await?;
        // Its input holds end with it: opened again, it starts unheld.
        self.release_session_holds(&id);
        // Closed: its lease goes.
        self.open.remove(&id).await;
        Ok(json!({ "session": id, "stopped": true }))
    }

    fn snapshot(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let entries = transcript_rows(&path, open)?;
        let header = entries.first().filter(|row| row["type"] == "session");
        // The last of each kind of row says what the session runs now.
        let last = |kind: &str| entries.iter().rev().find(|row| row["type"] == kind);
        Ok(json!({
            "session": id,
            "path": path,
            "open": open,
            "cwd": header.and_then(|header| header.get("cwd")).cloned(),
            "name": last("session_info").and_then(|row| row.get("name")).cloned(),
            "model": last("model_change").map(|row| json!({
                "provider": row.get("provider"),
                "modelId": row.get("modelId"),
            })),
            "thinkingLevel": last("thinking_level_change")
                .and_then(|row| row.get("thinkingLevel"))
                .cloned(),
            "entries": entries.len().saturating_sub(usize::from(header.is_some())),
            "lastCrash": last_crash(&entries),
            "leafId": entries.last().filter(|row| row["type"] != "session").and_then(|row| row.get("id")).cloned(),
            "lastEventSequence": self.runtime.last_event_sequence(&id),
        }))
    }

    fn page(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let before = optional_string_arg(args, "before")?;
        let limit = match args.get("limit") {
            None | Some(Value::Null) => PAGE_DEFAULT,
            Some(limit) => limit
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| (1..=PAGE_MAX).contains(limit))
                .with_context(|| format!("limit must be a whole number from 1 to {PAGE_MAX}"))?,
        };
        let mut rows = transcript_rows(&path, open)?;
        rows.retain(|row| row["type"] != "session");
        let end = match &before {
            Some(before) => rows
                .iter()
                .position(|row| row.get("id").and_then(Value::as_str) == Some(before))
                .with_context(|| format!("session {id} has no entry {before}"))?,
            None => rows.len(),
        };
        let start = end.saturating_sub(limit);
        Ok(json!({
            "session": id,
            "entries": rows[start..end],
            "more": start > 0,
        }))
    }

    async fn delete(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path) = self.saved_session(args, "deleting it")?;
        // A dormant session deleted is not woken.
        self.schedules.disarm(&id);
        self.open.remove(&id).await;
        let agent_dir = self.agent_dir.clone();
        let removal = tokio::task::spawn_blocking(move || {
            let _lease = acquire_runtime_session_lease(&path, &agent_dir)?;
            store()
                .remove(&path)
                .with_context(|| format!("remove {}", path.display()))
        })
        .await
        .context("the delete's thread panicked")??;
        Ok(json!({
            "session": id,
            "deleted": true,
            "trash": removal == pa_core::session::store::TranscriptRemoval::Trash,
        }))
    }

    async fn fork(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, _) = self.session(args)?;
        let (session_dir, cwd) = (self.session_dir.clone(), self.cwd.clone());
        let fork = tokio::task::spawn_blocking(move || {
            let cwd = pa_core::session::manager::read_session_header(&path)
                .map_or(cwd, |header| PathBuf::from(header.cwd));
            SessionManager::fork_from(&path, &cwd, &session_dir).map_err(anyhow::Error::msg)
        })
        .await
        .context("the fork's thread panicked")??;
        Ok(json!({
            "from": id,
            "session": fork.get_session_id(),
            "path": fork.get_session_file(),
        }))
    }

    async fn rename(&self, args: &Value) -> anyhow::Result<Value> {
        let name = string_arg(args, "name")?;
        self.change(args, move |manager| {
            manager.append_session_info(&name)?;
            Ok(json!({ "session": manager.get_session_id(), "name": name }))
        })
        .await
    }

    async fn label(&self, args: &Value) -> anyhow::Result<Value> {
        let entry = string_arg(args, "entry")?;
        let label = optional_string_arg(args, "label")?;
        self.change(args, move |manager| {
            anyhow::ensure!(
                manager.get_entry_by_id(&entry).is_some(),
                "the session has no entry {entry}"
            );
            manager.append_label_change(&entry, label.as_deref())?;
            Ok(json!({ "session": manager.get_session_id(), "entry": entry, "label": label }))
        })
        .await
    }

    async fn export(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, _, _) = self.session(args)?;
        let out = PathBuf::from(string_arg(args, "out")?);
        let (roots, base) = (vec![self.session_dir.clone()], self.agent_dir.clone());
        let selected = id.clone();
        let report = tokio::task::spawn_blocking(move || {
            pa_core::session::store::export_jsonl(
                &roots,
                &base,
                pa_core::session::store::ExportSelection::Session(&selected),
                &out,
            )
        })
        .await
        .context("the export's thread panicked")??;
        if let Some((path, reason)) = report.failed.first() {
            anyhow::bail!("export {}: {reason}", path.display());
        }
        Ok(json!({
            "session": id,
            "written": report.written,
            "unchanged": report.unchanged,
        }))
    }
}

impl<B: SessionBuilder> Handlers for RuntimeHandlers<B> {
    async fn query(&self, query: Query, args: Value) -> anyhow::Result<Value> {
        match query {
            Query::SessionsList => self.list(&args).await,
            Query::SessionSnapshot => match optional_string_arg(&args, "shape")? {
                Some(shape) => self.shaped_snapshot(&args, &shape).await,
                None => self.snapshot(&args),
            },
            Query::TranscriptPage => match optional_string_arg(&args, "shape")?.as_deref() {
                Some("messages") => self.messages_page(&args).await,
                Some(other) => anyhow::bail!("transcript.page has no shape {other:?} (messages)"),
                None => self.page(&args),
            },
            Query::Tasks => self.tasks(&args),
            Query::SessionContext => self.context(&args).await,
            Query::SessionUsage => self.usage(&args).await,
            Query::SessionTree => self.tree(&args).await,
            Query::SessionContextTree => self.context_tree(&args).await,
            Query::SessionBreakdown => self.breakdown(&args).await,
            Query::Schedules => self.schedules(&args),
            Query::Settings => self.settings(&args).await,
            Query::Models => self.models().await,
            Query::Sources => self.sources().await,
            Query::Subscriptions => Ok(self.subscriptions()),
            Query::SessionChildren => self.children(&args).await,
            Query::SessionModel => self.model(&args).await,
            Query::SessionSystemPrompt => self.system_prompt(&args).await,
        }
    }

    async fn call(
        &self,
        call: Call,
        args: Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        match call {
            Call::SessionCreate => self.create(args, connection).await,
            Call::SessionStop => self.stop(&args).await,
            Call::SessionRename => self.rename(&args).await,
            Call::SessionDelete => self.delete(&args).await,
            Call::SessionSetModel => self.set_model(&args).await,
            Call::SessionSetEffort => self.set_effort(&args).await,
            Call::SessionFork => self.fork(&args).await,
            Call::SessionLabel => self.label(&args).await,
            Call::SessionExport => self.export(&args).await,
            Call::SessionAbort => self.abort(&args, AfterAbort::Park).await,
            Call::SessionAbortAndSendQueued => self.abort(&args, AfterAbort::SendQueued).await,
            Call::SessionResumeQueue => self.resume_queue(&args),
            Call::SessionWithdraw => self.withdraw(&args).await,
            Call::SessionRestore => self.restore(&args).await,
            Call::SessionCompact => self.compact(&args).await,
            Call::SessionWaitIdle => self.wait_idle(&args).await,
            Call::SessionSend => self.send(&args, connection).await,
            Call::SessionSetMcpServers => self.set_mcp_servers(&args).await,
            Call::SessionBash => self.bash(&args).await,
            Call::SessionAbortBash => self.abort_bash(&args),
            Call::ScheduleAdd => self.schedule_add(&args).await,
            Call::ScheduleCancel => self.schedule_cancel(&args),
            Call::SessionPauseInput => self.pause_input(&args, connection),
            Call::SessionReleaseInput => self.release_input(&args),
            Call::SessionCancelTask => self.cancel_task(&args).await,
            Call::SessionCancelChild => self.cancel_child(&args).await,
        }
    }

    async fn disconnected(&self, connection: ConnectionId) {
        self.release_connection_holds(connection);
        self.end_owned_sessions(connection).await;
    }
}

/// The transcript's rows as stored (the header first), read without
/// repairing anything; an open session's transcript before its first row is
/// written has none.
fn transcript_rows(path: &Path, open: bool) -> anyhow::Result<Vec<Value>> {
    if !store().exists(path) {
        anyhow::ensure!(open, "no session at {}", path.display());
        return Ok(Vec::new());
    }
    load_entries_from_file(path, false)
        .iter()
        .map(|entry: &FileEntry| serde_json::to_value(entry).map_err(anyhow::Error::from))
        .collect()
}

/// The session's last fault, as its transcript kept it (the fault rows its
/// rebuilds and give-up wrote): the crash's fields and what became of the
/// session since (`recovered`, `failed`); `None` when it never crashed.
fn last_crash(rows: &[Value]) -> Option<Value> {
    let faults: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            row["type"] == "custom_message"
                && row["customType"] == pa_types::daemon::SESSION_FAULT_CUSTOM_TYPE
        })
        .map(|row| &row["details"])
        .collect();
    let crashed = faults
        .iter()
        .rposition(|details| details["type"] == "session_crashed")?;
    let mut crash = faults[crashed].clone();
    let state = faults[crashed + 1..]
        .iter()
        .find_map(|details| match details["type"].as_str() {
            Some("session_recovered") => Some("recovered"),
            Some("session_failed") => Some("failed"),
            _ => None,
        })
        .unwrap_or("rebuilding");
    if let Some(crash) = crash.as_object_mut() {
        crash.remove("type");
        crash.insert("state".to_string(), json!(state));
    }
    Some(crash)
}

/// The string argument `key`.
fn string_arg(args: &Value, key: &str) -> anyhow::Result<String> {
    optional_string_arg(args, key)?.with_context(|| format!("missing the `{key}` argument"))
}

/// The string argument `key`, when it is given.
fn optional_string_arg(args: &Value, key: &str) -> anyhow::Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(anyhow::anyhow!("the `{key}` argument must be a string")),
    }
}
