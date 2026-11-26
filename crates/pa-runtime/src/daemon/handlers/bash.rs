//! `session.bash` and `session.abort_bash`: the user's `!` commands on an
//! open session, the old daemon's `execute_bash` and `abort_bash` over
//! pa-core's user bash (`pa_core::user_bash`).
//!
//! One command runs at a time per session. `session.bash` answers as the
//! command starts; the run streams `bash_start`, a `bash_output` per chunk
//! and `bash_end` on `agent/<id>/stream`, and, unless `transient`, its
//! `bashExecution` row joins the session's transcript and the model's
//! context (`excludeFromContext` keeps it out of the context). The command
//! runs on the session's runtime, inside its scope. Stopping the session or
//! the daemon kills the running command's process tree first and waits
//! (bounded) for it to settle; a command asked for while its session stops
//! is refused.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use pa_core::user_bash::{
    bash_identity, record_in_engine, run_bash, start_event, RunBash, UserBash,
};
use serde_json::{json, Value};

use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::lock;
use crate::session::SessionBuilder;

/// The open sessions' user-bash slots, and the sessions being stopped,
/// which take no new command.
#[derive(Default)]
pub(in crate::daemon) struct UserBashes {
    slots: Mutex<Slots>,
}

#[derive(Default)]
struct Slots {
    slots: HashMap<String, Arc<UserBash>>,
    closing: HashSet<String>,
}

impl UserBashes {
    /// Stops session `id`'s commands: the running one is killed and this
    /// waits (bounded) until it settles; no new one is admitted while the
    /// answer is held (a session that stopped is refused as not open).
    pub(in crate::daemon) async fn close(&self, id: &str) -> Closing<'_> {
        let slot = {
            let mut slots = lock(&self.slots);
            slots.closing.insert(id.to_string());
            slots.slots.remove(id)
        };
        if let Some(slot) = slot {
            slot.abort_and_settle().await;
        }
        Closing {
            bashes: self,
            id: id.to_string(),
        }
    }

    /// Whether session `id`'s `!` command is running.
    pub(in crate::daemon) fn running(&self, id: &str) -> bool {
        lock(&self.slots)
            .slots
            .get(id)
            .is_some_and(|slot| slot.is_running())
    }

    /// [`Self::close`] for every session: the daemon is stopping.
    pub(in crate::daemon) async fn close_all(&self) {
        let ids: Vec<String> = lock(&self.slots).slots.keys().cloned().collect();
        for id in ids {
            drop(self.close(&id).await);
        }
    }
}

/// A session's commands held closed while it stops ([`UserBashes::close`]).
pub(in crate::daemon) struct Closing<'a> {
    bashes: &'a UserBashes,
    id: String,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        lock(&self.bashes.slots).closing.remove(&self.id);
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.bash {session, command, excludeFromContext?, transient?,
    /// runId?}`: starts the command and answers `{session, started}`.
    pub(super) async fn bash(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let command = string_arg(args, "command")?;
        let exclude_from_context = flag_arg(args, "excludeFromContext")?;
        let transient = flag_arg(args, "transient")?;
        let run_id = optional_string_arg(args, "runId")?;
        let cwd = self
            .open
            .cwd(&id)
            .with_context(|| format!("session {id} is not open in the runtime daemon"))?;
        let live = self.runtime.built_engine(&id).await?;
        let stream = self.runtime.stream_sender(&id)?;
        let settings = pa_core::settings::SettingsManager::create(&cwd, &self.agent_dir);
        let prefix = settings.settings().shell_command_prefix.clone();
        let shell_path = settings.settings().shell_path.clone();
        // Refused while the session stops or is no longer open; the check
        // and the claim are one critical section with a stop's removal of
        // the slot, so the stop's abort always sees the claimed run.
        let (slot, cancellation) = {
            let mut slots = lock(&self.open.bashes.slots);
            anyhow::ensure!(!slots.closing.contains(&id), "Session is shutting down");
            anyhow::ensure!(
                self.open.contains(&id),
                "session {id} is not open in the runtime daemon"
            );
            let slot = Arc::clone(slots.slots.entry(id.clone()).or_default());
            let cancellation = slot.claim().context("A bash command is already running")?;
            (slot, cancellation)
        };
        let identity = bash_identity(transient, run_id.as_deref());
        stream(start_event(&command, exclude_from_context, &identity));
        let cwd = cwd.to_string_lossy().into_owned();
        let engine = Arc::clone(&live.engine);
        // The run outlives the answer (a command can outlast the client's
        // request timeout); its end streams as `bash_end`.
        live.runtime.spawn(async move {
            let end = run_bash(RunBash {
                command: &command,
                cwd: &cwd,
                prefix: prefix.as_deref(),
                shell_path: shell_path.as_deref(),
                cancellation: &cancellation,
                on_chunk: Some(Arc::clone(&stream)),
            })
            .await;
            // A transient run lives only in its pane: nothing is recorded.
            if !transient {
                if let Err(error) =
                    record_in_engine(&engine, &command, &end.result(), exclude_from_context).await
                {
                    tracing::warn!("a bash command's row was not saved: {error}");
                }
            }
            slot.release();
            stream(end.end_event(&identity));
        });
        Ok(json!({ "session": id, "started": true }))
    }

    /// `session.abort_bash {session}`: kills the session's running command
    /// (its run reports cancelled); answers whether one was running.
    pub(super) fn abort_bash(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let slot = lock(&self.open.bashes.slots).slots.get(&id).cloned();
        let running = slot.as_ref().is_some_and(|slot| slot.is_running());
        if let Some(slot) = slot {
            slot.abort();
        }
        Ok(json!({ "session": id, "aborted": running }))
    }
}

/// The boolean argument `key`, false when it is not given.
fn flag_arg(args: &Value, key: &str) -> anyhow::Result<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(anyhow::anyhow!("the `{key}` argument must be a boolean")),
    }
}
