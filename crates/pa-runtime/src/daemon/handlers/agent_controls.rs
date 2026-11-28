//! The agents view's controls of a session on the runtime daemon (8d-4b),
//! the old worker's commands of the same names, each answering the old
//! response's `data` or its refusal text (pa-core's
//! `session_engine::agent_controls`, applied on the session's node by
//! `subagents/controls.rs`):
//!
//! - `agent_messages {session?}` (`agent_messages_status`): the safety status
//!   `{paused, maxMessageChars, maxPendingPerSession, rateLimitCapacity,
//!   rateLimitRefillMs}`; the calls `agent_messages.pause {session?}` and
//!   `agent_messages.resume {session?}` set the pause (a pause also takes the
//!   agent messages waiting on the session's agent out) and answer the
//!   status; `agent_messages.clear {session}` takes them out and answers
//!   `{steering, followUp}`, their prompts. A session is any the runtime
//!   runs, a subagent's included. Without `session` the status is the first
//!   running session's by id (`{paused: false, limits: {}}` while none runs),
//!   and a pause or resume reaches every running session, answering the
//!   first one's status (`null` while none runs), as the old supervisor's
//!   broadcast did.
//! - `session.rlm_max_depth {session}` (`get_rlm_max_depth_status`):
//!   `{maxDepth, source}`; the call `session.set_rlm_max_depth {session,
//!   maxDepth, global?}` (`set_rlm_max_depth`) fixes the bound as a chat
//!   override, appends the session's `rlm_max_depth_state` row on its
//!   runtime, with `global` writes the settings default on a blocking thread,
//!   and answers `{maxDepth, source: "chat", globalSaved, globalError?}`.
//! - The call `session.delete_child {session, child}` (`delete_rlm_subagent`,
//!   `child` the subagent's handle): `{deleted}`, or `{deleted: false,
//!   reason: "running"}` for a subagent with live work.
//! - `session.stop {session, rlmCloseReason?}` closes a session as the old
//!   `kill` did (pa-core's `session::close`): `killed` without the marker
//!   cancels every job of the session and records its `archived` state,
//!   `replaced` cancels a subagent's RLM heartbeats and archives, `shutdown`
//!   keeps both; its clients are told the reason (`agent/<id>/closed`), and
//!   any other marker is refused.
//! - `session.stop {session, rlmLedgerDelete?, rlmChildId?}` of a subagent
//!   (the old `kill` of a child's session, `subagents/controls.rs`): a plain
//!   stop ends it and keeps its ledger edge; with `rlmLedgerDelete` (the
//!   delete's reason, `user`) it is deleted, its deletion boundary written
//!   before its teardown, and a delete aimed at a child no session runs (a
//!   retry included) is the ledger's alone. Both answer `{session, stopped}`;
//!   a boundary that is not written is `Failed to delete RLM subagent: ...`.

use anyhow::Context as _;
use pa_core::session::close::SessionCloseReason;
use pa_core::session_engine::agent_controls::{
    unscoped_agent_message_status, SetRlmMaxDepth, RLM_MAX_DEPTH_STATE_CUSTOM_TYPE,
};
use pa_core::session_engine::rlm_ledger::RlmLedgerDeleteReason;
use pa_types::event_log::{NewEvent, Topic, Trust};
use serde_json::{json, Value};

use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;
use crate::subagents::{ChildStop, ChildStopped};

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `agent_messages` (see the module documentation).
    pub(super) fn agent_messages(&self, args: &Value) -> anyhow::Result<Value> {
        let subagents = self.runtime.subagents();
        match optional_string_arg(args, "session")? {
            Some(id) => subagents.agent_message_status(&id),
            None => match subagents.ingesting_sessions().first() {
                Some(id) => subagents.agent_message_status(id),
                None => Ok(unscoped_agent_message_status()),
            },
        }
    }

    /// `agent_messages.pause` and `agent_messages.resume` (see the module
    /// documentation).
    pub(super) fn pause_agent_messages(&self, args: &Value, paused: bool) -> anyhow::Result<Value> {
        let subagents = self.runtime.subagents();
        if let Some(id) = optional_string_arg(args, "session")? {
            return subagents.pause_agent_messages(&id, paused);
        }
        let mut first = Value::Null;
        for id in subagents.ingesting_sessions() {
            let status = subagents.pause_agent_messages(&id, paused)?;
            if first.is_null() {
                first = status;
            }
        }
        Ok(first)
    }

    /// `agent_messages.clear` (see the module documentation).
    pub(super) fn clear_agent_messages(&self, args: &Value) -> anyhow::Result<Value> {
        self.runtime
            .subagents()
            .clear_agent_messages(&string_arg(args, "session")?)
    }

    /// `session.rlm_max_depth` (see the module documentation).
    pub(super) async fn rlm_max_depth(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        // The bound and its source are the built engine's.
        self.runtime.built_engine(&id).await?;
        self.runtime.subagents().rlm_max_depth_status(&id)
    }

    /// `session.set_rlm_max_depth` (see the module documentation).
    pub(super) async fn set_rlm_max_depth(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let set = SetRlmMaxDepth::parse(args).map_err(anyhow::Error::msg)?;
        let live = self.runtime.built_engine(&id).await?;
        self.runtime
            .subagents()
            .fix_rlm_max_depth(&id, set.bound())?;
        // The durable row a session opened again resolves its bound from; a
        // failed write is logged, never the command's failure, as the old
        // worker's was.
        let persistence = live.engine.session.shared_persistence();
        let row = set.state_row();
        let written = live
            .runtime
            .spawn(async move {
                persistence
                    .lock()
                    .await
                    .append_custom_entry(RLM_MAX_DEPTH_STATE_CUSTOM_TYPE, Some(row))
            })
            .await
            .context("the session's runtime ended during the write")?;
        if let Err(error) = written {
            tracing::warn!("session {id}'s rlm_max_depth_state row was not written: {error:#}");
        }
        let cwd = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
        let agent_dir = self.agent_dir.clone();
        let global = tokio::task::spawn_blocking(move || set.write_global(&cwd, &agent_dir))
            .await
            .context("the settings write panicked")?;
        Ok(set.reply(global))
    }

    /// `session.delete_child` (see the module documentation).
    pub(super) async fn delete_child(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let child = string_arg(args, "child")?;
        let outcome = self.runtime.subagents().delete_child(&id, &child).await?;
        Ok(outcome.reply())
    }

    /// What `session.stop`'s `args` ask of a subagent (see the module
    /// documentation).
    pub(super) fn child_stop(args: &Value) -> anyhow::Result<ChildStop> {
        let child_id = optional_string_arg(args, "rlmChildId")?;
        match optional_string_arg(args, "rlmLedgerDelete")? {
            None => Ok(ChildStop::Stop),
            Some(wire) => Ok(ChildStop::Delete {
                reason: RlmLedgerDeleteReason::from_wire(&wire)
                    .with_context(|| format!("{wire:?} is no RLM ledger delete reason"))?,
                child_id,
            }),
        }
    }

    /// `session.stop` of session `id`, which is no open root (see the
    /// module documentation), its clients told `reason`.
    pub(super) async fn stop_subagent(
        &self,
        id: &str,
        stop: ChildStop,
        reason: SessionCloseReason,
    ) -> anyhow::Result<Value> {
        let deleting = matches!(stop, ChildStop::Delete { .. });
        // A running subagent's close records its `archived` state first, as
        // a root's does.
        self.archive_closed(id, reason).await?;
        let stopped = match self.runtime.subagents().stop_child(id, stop).await {
            Ok(stopped) => stopped,
            Err(error) if deleting => anyhow::bail!("Failed to delete RLM subagent: {error:#}"),
            Err(error) => return Err(error),
        };
        match stopped {
            ChildStopped::Unknown => {
                anyhow::bail!("session {id} is not open in the runtime daemon")
            }
            ChildStopped::Ledger => {}
            ChildStopped::Session => {
                // Closed on purpose: its jobs are cancelled as the close
                // says, and those it keeps wake nothing.
                self.schedules.close(id, reason.jobs())?;
                // It leaves the roster, and its attached clients learn it
                // was stopped, as a root's do.
                self.feeds.due(id);
                self.runtime
                    .publish(NewEvent {
                        topic: Topic::new(format!("agent/{id}/closed"))?,
                        source: "runtime".to_string(),
                        session: Some(id.to_string()),
                        payload: json!({ "reason": reason.wire() }),
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await?;
            }
        }
        Ok(json!({ "session": id, "stopped": true }))
    }
}
