//! The update boot on the runtime daemon (`docs/legacy-upgrade-contract.md`,
//! "Daemon and TUI handoff"): the roster the coordinator spawns the daemon
//! with (`PRIME_AGENT_UPDATE_ROSTER`, read by the embedder before the boot
//! sweep deletes it, [`crate::daemon::DaemonOptions::update_roster`]) is
//! restored by pa-core's restore pass (`pa_core::update::restart::restore`),
//! the old supervisor's, with the daemon as its [`RestoreHost`]:
//!
//! - **Rows.** A row whose transcript the daemon already holds open (the
//!   boot revived it, a client opened it) is found; any other is opened as
//!   `session.create {path, cwd}` opens it, with the options the session was
//!   created with. A TS roster's subagent row opens as a session of its own:
//!   its parent handle is the TS daemon's.
//! - **The continuation.** A row mid-turn when the snapshot was taken gets
//!   the continuation prompt, except in a session the boot revived with
//!   unfinished tasks: the runtime predecessor's stop queued that
//!   continuation already, so it is not queued twice.
//! - **A TS roster's replay** (the old worker commands the pass sends): the
//!   input pause is a hold on the session's lane; `append_custom_message`
//!   appends the row to the transcript; `restore_next_turn`'s rows are
//!   steered to the session's agent, read at its next turn; `restore_actions`
//!   is checked as the old worker checks it and each `turn` action queued as
//!   input (`next_turn_boundary` a steer, else a follow-up), an action
//!   carrying a custom message refused; `prompt` queues a steer; `abort`
//!   cuts the running turn off.
//! - **Progress.** `daemon.update_restore_status` answers the old
//!   `update_restore_status` body, and `session.create` and `session.attach`
//!   of a roster row wait behind the pass until its row settles (spec
//!   §10.4); `daemon.identity` carries the hello's `updateResume`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;
use pa_core::update::restart::restore::{
    report_dormant_jobs, validate_actions, RestoreHost, RouteReply,
};
use pa_types::daemon::update_flow::UpdateRosterSession;
use pa_types::event_log::{AgentInput, Priority};
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::session::SessionBuilder;
use crate::wire::ConnectionId;

/// The runtime daemon as the restore pass's host.
pub(in crate::daemon) struct RuntimeRestore<B: SessionBuilder> {
    pub(in crate::daemon) handlers: Arc<RuntimeHandlers<B>>,
    /// The sessions the boot revived with unfinished tasks.
    pub(in crate::daemon) revived: HashSet<String>,
}

impl<B: SessionBuilder> RestoreHost for RuntimeRestore<B> {
    type Session = String;

    fn find(
        &self,
        row: &UpdateRosterSession,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(self.handlers.open.id_of(Path::new(&row.session_file)))
    }

    async fn open(
        &self,
        row: &UpdateRosterSession,
        _restored_ids: Option<&BTreeMap<String, String>>,
    ) -> anyhow::Result<String> {
        let created = self
            .handlers
            .create(
                json!({ "path": row.session_file, "cwd": row.cwd }),
                ConnectionId(0),
            )
            .await?;
        created["session"]
            .as_str()
            .map(str::to_string)
            .context("the create reply carries no session id")
    }

    fn active_session_id(
        &self,
        session: &String,
    ) -> impl std::future::Future<Output = String> + Send {
        std::future::ready(session.clone())
    }

    async fn route(
        &self,
        session: &String,
        command: &'static str,
        payload: Value,
    ) -> anyhow::Result<RouteReply> {
        Ok(
            match self
                .handlers
                .restore_command(session, command, &payload, &self.revived)
                .await
            {
                Ok(data) => RouteReply::ok(data),
                Err(error) => RouteReply::failed(format!("{error:#}")),
            },
        )
    }

    fn log(&self, line: &str) {
        tracing::info!("{line}");
    }

    async fn report_dormant(&self) {
        let live: HashSet<String> = self
            .handlers
            .open
            .paths()
            .into_values()
            .chain(self.handlers.open.dormant_paths())
            .map(|path| {
                pa_core::session::lease::canonical_session_path(&path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let agent_dir = self.handlers.agent_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            report_dormant_jobs(&agent_dir, &live, |line| tracing::info!("{line}"));
        })
        .await;
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// Spec §10.4: a `session.create` or `session.attach` of a session the
    /// restore pass owns waits until its row settles; one whose row failed
    /// and that is not open is refused with the old typed attach error.
    pub(super) async fn await_restored(&self, args: &Value) -> anyhow::Result<()> {
        let Some(id) = args.get("session").and_then(Value::as_str) else {
            return Ok(());
        };
        self.restore.await_target(id).await;
        if !self.open.contains(id) {
            if let Some(failure) = self.restore.failure_for(id) {
                anyhow::bail!("{failure}");
            }
        }
        Ok(())
    }

    /// `daemon.identity`: the old `daemon_hello`'s identity fields.
    pub(super) fn identity(&self) -> Value {
        let pid = std::process::id();
        json!({
            "supervisorPid": pid,
            "supervisorProcessStartId": pa_core::session::hosted::process_start_id(pid),
            "supervisorGeneration": format!("runtime:{pid}"),
            "keeperPid": self.boot.keeper_pid,
            "appVersion": self.boot.app_version,
            "schemaId": pa_types::daemon::DAEMON_SCHEMA_ID,
            "supervisorSocketPath": self.update.socket_path(),
            "updateResume": self.restore.hello_resume(),
        })
    }

    /// One old worker command of the restore pass on open session `id`
    /// (see the module documentation): the old response's `data`.
    async fn restore_command(
        &self,
        id: &str,
        command: &str,
        payload: &Value,
        revived: &HashSet<String>,
    ) -> anyhow::Result<Option<Value>> {
        match command {
            "prompt" => {
                let text = payload["message"]
                    .as_str()
                    .context("prompt requires a message")?;
                if text == UPDATE_RESTART_CONTINUATION_PROMPT && revived.contains(id) {
                    return Ok(None);
                }
                self.publish_input(id, input(text, Priority::Steer)).await?;
                Ok(None)
            }
            "acquire_session_input_pause" => {
                let key = format!(
                    "update-restore:{}",
                    payload["leaseKey"].as_str().unwrap_or_default()
                );
                self.runtime.hold_input(id, &key);
                Ok(Some(json!({ "pauseId": key })))
            }
            "release_session_input_pause" => {
                let key = payload["pauseId"]
                    .as_str()
                    .context("release_session_input_pause requires a pauseId")?;
                self.runtime.release_input(id, key);
                Ok(None)
            }
            "abort" => {
                self.runtime.interrupt(id).await?;
                Ok(None)
            }
            "append_custom_message" => {
                let message = custom_message(&payload["message"])
                    .context("append_custom_message requires a custom message")?;
                self.change(&json!({ "session": id }), move |manager| {
                    manager.append_custom_message_entry(
                        &message.custom_type,
                        message.content,
                        message.display,
                        message.details,
                    )?;
                    Ok(Value::Null)
                })
                .await?;
                Ok(None)
            }
            "restore_next_turn" => {
                let messages = payload["messages"]
                    .as_array()
                    .context("restore_next_turn requires a messages array")?
                    .iter()
                    .map(|message| {
                        custom_message(message)
                            .as_ref()
                            .and_then(
                                pa_core::session_engine::goal_boundary::custom_message_to_loop_row,
                            )
                            .context("restore_next_turn requires custom messages")
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let live = self.runtime.built_engine(id).await?;
                let agent = live.engine.session.agent();
                for message in messages {
                    agent.steer(message);
                }
                Ok(None)
            }
            "restore_actions" => {
                let actions = validate_actions(payload.get("snapshot"), |id, action| {
                    if action["payload"]
                        .get("customMessage")
                        .is_some_and(|message| !message.is_null())
                    {
                        return Err(format!(
                            "Session action {id} carries a custom message, which the runtime daemon does not restore"
                        ));
                    }
                    Ok(())
                })
                .map_err(anyhow::Error::msg)?;
                for action in actions {
                    let text = action["payload"]["text"].as_str().unwrap_or_default();
                    let priority = if action["delivery"] == "next_turn_boundary" {
                        Priority::Steer
                    } else {
                        Priority::FollowUp
                    };
                    self.publish_input(id, input(text, priority)).await?;
                }
                Ok(None)
            }
            other => anyhow::bail!("the runtime daemon does not restore with {other}"),
        }
    }
}

/// Input `text` at `priority`.
fn input(text: &str, priority: Priority) -> AgentInput {
    AgentInput {
        text: text.to_string(),
        images: Vec::new(),
        priority: Some(priority),
    }
}

/// An old command's custom message (`customType`, `content`, `display`,
/// `details`).
fn custom_message(message: &Value) -> Option<pa_types::session::CustomMessage> {
    let mut message = message.as_object()?.clone();
    message
        .entry("display")
        .or_insert_with(|| Value::Bool(true));
    message.insert("role".to_string(), json!("custom"));
    let pa_types::session::AgentMessage::Custom(message) =
        serde_json::from_value(Value::Object(message)).ok()?
    else {
        return None;
    };
    Some(message)
}
