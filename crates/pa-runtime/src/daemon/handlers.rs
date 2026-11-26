//! The runtime daemon's queries and calls ([`RuntimeHandlers`]).
//!
//! Sessions are addressed by id (`{"session": "<id>"}`): an open session by
//! the transcript it was opened on, any other by `<sessions dir>/<id>.jsonl`.
//! Reads (`sessions.list`, `session.snapshot`, `transcript.page`) read the
//! transcript store, which every row an open session's engine writes reaches
//! as it is written, so they never wait on a running turn. A change to an
//! open session goes through its engine's session manager, on the session's
//! own runtime; a change to a saved one takes the session's lease for the
//! change, so a session another process holds is refused, never written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session::lease::acquire_runtime_session_lease;
use pa_core::session::manager::{get_session_file_path, load_entries_from_file, SessionManager};
use pa_core::session::store::store;
use pa_types::session::FileEntry;
use serde_json::{json, Value};

use super::held::{OpenSession, OpenSessions};
use crate::runtime::{AfterAbort, Runtime};
use crate::session::{SessionBuilder, SessionSpec};
use crate::wire::{Call, Handlers, Query};

mod queue;
mod state;

/// The most entries one `transcript.page` returns, and how many it returns
/// when the client names no `limit`.
const PAGE_MAX: usize = 500;
const PAGE_DEFAULT: usize = 100;

/// Answers the runtime daemon's queries and calls.
pub(super) struct RuntimeHandlers<B: SessionBuilder> {
    runtime: Arc<Runtime<B>>,
    open: Arc<OpenSessions>,
    agent_dir: PathBuf,
    session_dir: PathBuf,
    cwd: PathBuf,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) fn new(
        runtime: Arc<Runtime<B>>,
        open: Arc<OpenSessions>,
        agent_dir: PathBuf,
        session_dir: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        RuntimeHandlers {
            runtime,
            open,
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
        let live = self
            .runtime
            .session_engine(&id)
            .with_context(|| format!("session {id}'s engine is not built yet"))?;
        let persistence = live.engine.session.shared_persistence();
        live.runtime
            .spawn(async move { change(&mut *persistence.lock().await) })
            .await
            .context("the session's runtime ended during the change")?
    }

    async fn create(&self, args: Value) -> anyhow::Result<Value> {
        let named = optional_string_arg(&args, "session")?;
        let given_path = optional_string_arg(&args, "path")?.map(PathBuf::from);
        let given_cwd = optional_string_arg(&args, "cwd")?.map(PathBuf::from);
        let (id, path) = match (named, given_path) {
            (Some(_), Some(_)) => {
                anyhow::bail!("give session.create a `session` or a `path`, not both")
            }
            (None, Some(path)) => {
                let header = pa_core::session::manager::read_session_header(&path)
                    .with_context(|| format!("no session header in {}", path.display()))?;
                (header.id, path)
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
        anyhow::ensure!(
            !self.open.contains(&id),
            "session {id} is open in the runtime daemon already"
        );
        let cwd = match given_cwd {
            Some(cwd) => cwd,
            None => pa_core::session::manager::read_session_header(&path)
                .map_or_else(|| self.cwd.clone(), |header| PathBuf::from(header.cwd)),
        };
        let lease = {
            let (path, agent_dir) = (path.clone(), self.agent_dir.clone());
            tokio::task::spawn_blocking(move || acquire_runtime_session_lease(&path, &agent_dir))
                .await
                .context("the lease's thread panicked")??
        };
        self.runtime
            .create_session(SessionSpec {
                id: Some(id.clone()),
                name: None,
                cwd: cwd.clone(),
                env: super::session_env(&self.agent_dir, &id),
                subagent: None,
                transcript: Some(path.clone()),
            })
            .await?;
        self.open
            .insert(
                id.clone(),
                OpenSession {
                    path: path.clone(),
                    cwd: cwd.clone(),
                    _lease: lease,
                },
            )
            .await;
        Ok(json!({ "session": id, "path": path, "cwd": cwd }))
    }

    async fn stop(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        anyhow::ensure!(
            self.open.contains(&id),
            "session {id} is not open in the runtime daemon"
        );
        self.runtime.close_session(&id).await?;
        // Closed: its lease goes.
        self.open.remove(&id).await;
        Ok(json!({ "session": id, "stopped": true }))
    }

    fn list(&self) -> Value {
        let open: HashMap<String, PathBuf> = self.open.paths();
        let mut sessions: Vec<Value> =
            pa_core::session::discovery::scan_session_headers(&self.session_dir)
                .into_iter()
                .filter(|header| !open.contains_key(&header.id))
                .map(|header| {
                    json!({ "session": header.id, "path": header.path, "cwd": header.cwd, "open": false })
                })
                .collect();
        sessions.extend(open.into_iter().map(|(id, path)| {
            let cwd =
                pa_core::session::manager::read_session_header(&path).map(|header| header.cwd);
            json!({ "session": id, "path": path, "cwd": cwd, "open": true })
        }));
        json!({ "sessions": sessions })
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

    async fn set_model(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path) = self.saved_session(
            args,
            "changing its model (a running session's model changes in phase 7b)",
        )?;
        let selector = string_arg(args, "model")?;
        let auth = pa_core::auth::AuthStorage::create(&self.agent_dir);
        let registry =
            pa_core::models::ModelRegistry::create(auth, self.agent_dir.join("models.json"));
        let resolved = pa_core::models::resolve_cli_model(
            /*cli_provider*/ None,
            &selector,
            registry.get_all(),
        );
        if let Some(error) = resolved.error {
            anyhow::bail!(error);
        }
        let model = resolved
            .model
            .with_context(|| format!("no model matches {selector:?}"))?;
        let (provider, model_id) = (model.provider.clone(), model.id.clone());
        self.change_saved(path, move |manager| {
            manager.append_model_change(&provider, &model_id)?;
            Ok(json!({ "session": id, "provider": provider, "modelId": model_id }))
        })
        .await
    }

    async fn set_effort(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let name = string_arg(args, "level")?;
        let level: pa_agent::types::ThinkingLevel =
            serde_json::from_value(json!(name)).map_err(|_| {
                anyhow::anyhow!(
                    "unknown thinking level {name:?} (off, minimal, low, medium, high, xhigh, max)"
                )
            })?;
        if open {
            let live = self
                .runtime
                .session_engine(&id)
                .with_context(|| format!("session {id}'s engine is not built yet"))?;
            live.runtime
                .spawn(async move { live.engine.session.set_thinking_level(level).await })
                .await
                .context("the session's runtime ended during the change")??;
        } else {
            self.change_saved(path, move |manager| {
                manager.append_thinking_level_change(&name)?;
                Ok(())
            })
            .await?;
        }
        Ok(json!({ "session": id, "level": level }))
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
            Query::SessionsList => Ok(self.list()),
            Query::SessionSnapshot => self.snapshot(&args),
            Query::TranscriptPage => self.page(&args),
            Query::Tasks => self.tasks(&args),
            Query::SessionContext => self.context(&args).await,
            Query::SessionUsage => self.usage(&args).await,
            Query::SessionTree => self.tree(&args).await,
            Query::SessionContextTree => self.context_tree(&args).await,
            Query::SessionBreakdown => self.breakdown(&args).await,
            Query::Settings | Query::Models | Query::Sources | Query::Subscriptions => Err(
                anyhow::anyhow!("{query:?} is not yet in the runtime daemon (phase 7c+)"),
            ),
        }
    }

    async fn call(&self, call: Call, args: Value) -> anyhow::Result<Value> {
        match call {
            Call::SessionCreate => self.create(args).await,
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
        }
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
