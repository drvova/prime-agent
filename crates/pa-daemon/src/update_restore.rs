//! The supervisor's side of the update boot's restore (spec §6): the
//! supervisor as pa-core's `RestoreHost` (`pa_core::update::restart::restore`
//! drives the pass, the boot sweep and the roster's consumption, shared with
//! the runtime daemon), and the progress queries the supervisor's command
//! paths read.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use pa_core::update::restart::restore::{
    remap_legacy_parent, report_dormant_jobs, RestoreHost, RouteReply, UPDATE_RESTORE_CLIENT_ID,
};
use pa_types::daemon::update_flow::{UpdateRoster, UpdateRosterSession};
use pa_types::daemon::DaemonCommand;
use serde_json::Value;

use crate::backpressure::RouteAdmission;
use crate::registry::ResidentWorker;
use crate::supervisor::Supervisor;

/// The supervisor as the restore pass's host: its resident workers.
struct SupervisorRestore<'a>(&'a Arc<Supervisor>);

impl RestoreHost for SupervisorRestore<'_> {
    type Session = Arc<ResidentWorker>;

    async fn find(&self, row: &UpdateRosterSession) -> Option<Arc<ResidentWorker>> {
        self.0
            .registry
            .find_by_session_file(&row.session_file)
            .await
    }

    /// Re-creates one roster row's session from the durable create command the roster captured
    /// (spec §8 `runtime_config`): the supervisor's own create path, under the restore client id.
    async fn open(
        &self,
        row: &UpdateRosterSession,
        restored_ids: Option<&BTreeMap<String, String>>,
    ) -> Result<Arc<ResidentWorker>> {
        let supervisor = self.0;
        let durable = row
            .runtime_config
            .get("create")
            .cloned()
            .context("the roster row carries no create command")?;
        let durable: pa_types::daemon::DurableDaemonCreateCommand =
            serde_json::from_value(durable).context("parse the roster row's create command")?;
        let mut payload = crate::descriptor::create_command_payload(&durable);
        if let Some(restored_ids) = restored_ids {
            remap_legacy_parent(&mut payload, restored_ids);
        }
        let command: DaemonCommand =
            serde_json::from_value(payload).context("rebuild the create command")?;
        let summary = supervisor
            .handle_create(&command, UPDATE_RESTORE_CLIENT_ID.to_string())
            .await?;
        let session_id = summary.get("sessionId").and_then(|value| value.as_str());
        if session_id.is_none() {
            anyhow::bail!("the create reply carries no session id");
        }
        supervisor
            .registry
            .find_by_session_file(&row.session_file)
            .await
            .context("the restored session did not register")
    }

    async fn active_session_id(&self, session: &Arc<ResidentWorker>) -> String {
        session
            .descriptor
            .lock()
            .await
            .root_active_session_id
            .clone()
    }

    async fn route(
        &self,
        session: &Arc<ResidentWorker>,
        command: &'static str,
        payload: Value,
    ) -> Result<RouteReply> {
        let response = self
            .0
            .route_command_typed(
                session,
                command,
                payload,
                crate::supervisor::ROUTE_TIMEOUT_MS,
                RouteAdmission::SupervisorInternal,
            )
            .await?;
        Ok(RouteReply {
            success: response.success,
            error: response.error,
            data: response.data,
        })
    }

    fn log(&self, line: &str) {
        self.0.log_line(line);
    }

    /// A session that went down with the previous daemon is not dormant: its wake is armed
    /// (`supervisor/scheduled_wake.rs`).
    async fn report_dormant(&self) {
        let supervisor = self.0;
        let mut live = supervisor.live_session_files().await;
        live.extend(
            supervisor
                .scheduled_wakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        report_dormant_jobs(&supervisor.options.agent_dir, &live, |line| {
            supervisor.log_line(line);
        });
    }
}

/// The boot restore driver: run once the descriptor-adoption task settles
/// (kept workers relaunch from their descriptors first, the roster covers the
/// rest), then pa-core's pass over the roster.
pub(crate) async fn restore_pass(
    supervisor: &Arc<Supervisor>,
    adoption: tokio::task::JoinHandle<()>,
    roster: Option<UpdateRoster>,
) {
    let _ = adoption.await;
    pa_core::update::restart::restore::restore_pass(
        &SupervisorRestore(supervisor),
        &supervisor.restore,
        roster,
    )
    .await;
}

impl Supervisor {
    /// Spec §10.4: queue behind an in-flight restore pass that owns the
    /// selector, then re-resolve or fail typed against the settled outcome.
    pub(crate) async fn await_restore_target(&self, selector: &str) {
        self.restore.await_target(selector).await;
    }

    /// Spec §10.4: the settled per-row failure, as the typed attach
    /// error with the session file path and the manual-resume hint.
    pub(crate) fn restore_failure_for(&self, selector: &str) -> Option<String> {
        self.restore.failure_for(selector)
    }
}

#[cfg(test)]
mod tests {
    use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;
    use pa_core::update::restart::restore::restore_legacy_session;
    use serde_json::json;

    use super::*;

    /// The worker's answer, as the restore pass reads it.
    fn reply(response: crate::protocol::DaemonResponse) -> RouteReply {
        RouteReply {
            success: response.success,
            error: response.error,
            data: response.data,
        }
    }

    /// A two-row roster (update `u-1`): row `a-1`, and row `durable-b`
    /// whose session-file stem (`b-2`) differs from its durable id.
    fn two_row_roster() -> UpdateRoster {
        serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "update_id": "u-1",
            "socket_path": "/tmp/s.sock",
            "created_at": "2026-01-01T00:00:00Z",
            "supervisor": { "pid": 1, "process_start_id": "p", "generation": "g" },
            "binary": { "from_version": "0.1", "to_version": "0.2" },
            "sessions": [
                {
                    "session_id": "a-1",
                    "active_session_id": "active-a",
                    "session_file": "/sessions/a-1.jsonl",
                    "name": "alpha",
                    "kind": "top-level",
                    "rlm_depth": 0,
                    "cwd": "/w",
                    "runtime_config": {},
                    "queue": { "next_turn": [], "actions": {} },
                    "in_flight": {
                        "streaming": false, "compacting": false, "bash_running": false,
                        "rlm_children": false, "retrying": false, "prompt_in_flight": false
                    },
                    "should_resume": false
                },
                {
                    "session_id": "durable-b",
                    "active_session_id": "active-b",
                    "session_file": "/sessions/b-2.jsonl",
                    "kind": "top-level",
                    "rlm_depth": 0,
                    "cwd": "/w",
                    "runtime_config": {},
                    "queue": { "next_turn": [], "actions": {} },
                    "in_flight": {
                        "streaming": false, "compacting": false, "bash_running": false,
                        "rlm_children": false, "retrying": false, "prompt_in_flight": false
                    },
                    "should_resume": false
                }
            ],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn legacy_manifest_restores_real_worker_context_and_both_queue_lanes() {
        let dir = tempfile::TempDir::new().unwrap();
        let worker = crate::worker::Worker::new(
            crate::worker::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "token".into(),
                worker_instance_id: String::new(),
                active_session_id: "active-a".into(),
                agent_dir: dir.path().join("agent"),
                recovery_journal_path: dir.path().join("recovery.jsonl"),
                telemetry_disabled: Some(true),
                script: Some(json!({ "responses": ["ack"] })),
            },
            None,
        );
        assert!(
            worker
                .dispatch("create", &json!({ "noSession": true, "cwd": dir.path() }))
                .await
                .success
        );
        let mut row = two_row_roster().sessions.remove(0);
        row.should_resume = true;
        row.queue.next_turn = serde_json::from_value(json!([{
            "customType": "saved-context", "content": "pending context", "display": false,
            "timestamp": 1
        }]))
        .unwrap();
        row.queue.actions = json!({ "formatVersion": 1, "actions": [
            { "id": "steer", "delivery": "next_turn_boundary", "payload": { "kind": "turn", "text": "steering task" } },
            { "id": "follow", "delivery": "when_run_idle", "payload": { "kind": "turn", "text": "follow-up task" } }
        ] });
        let resumed = restore_legacy_session(&row, None, "1.0.0", |command, payload| {
            let worker = &worker;
            async move { Ok(reply(worker.dispatch(command, &payload).await)) }
        })
        .await
        .unwrap();
        assert!(resumed);
        let core = worker.core.lock().unwrap();
        assert_eq!(
            (
                core.pending_next_turn[0]["content"].clone(),
                core.steering
                    .iter()
                    .map(|item| item.message.clone())
                    .collect::<Vec<_>>(),
                core.follow_up
                    .iter()
                    .map(|item| item.message.clone())
                    .collect::<Vec<_>>()
            ),
            (
                json!("pending context"),
                vec!["steering task".to_string()],
                vec!["follow-up task".to_string()]
            )
        );
    }

    #[tokio::test]
    async fn legacy_busy_restore_keeps_follow_up_behind_continuation_with_a_running_worker() {
        let dir = tempfile::TempDir::new().unwrap();
        let worker = crate::worker::Worker::new(
            crate::worker::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "token".into(),
                worker_instance_id: String::new(),
                active_session_id: "active-a".into(),
                agent_dir: dir.path().join("agent"),
                recovery_journal_path: dir.path().join("recovery.jsonl"),
                telemetry_disabled: Some(true),
                script: Some(json!({ "responses": ["ack", "done"] })),
            },
            None,
        );
        assert!(
            worker
                .dispatch("create", &json!({ "noSession": true, "cwd": dir.path() }))
                .await
                .success
        );
        let mut events = worker.events.subscribe();
        let mut row = two_row_roster().sessions.remove(0);
        row.should_resume = true;
        row.in_flight.streaming = true;
        row.queue.actions = json!({ "formatVersion": 1, "actions": [{
            "id": "follow", "delivery": "when_run_idle", "source": "interactive",
            "payload": { "kind": "turn", "text": "queued follow-up", "queueVisible": true }
        }] });
        assert!(
            restore_legacy_session(&row, None, "1.0.0", |command, payload| {
                let worker = &worker;
                async move {
                    // Observe the real runner handling the restore wake before the
                    // coordinator can admit its continuation. No sleeps or retries.
                    let parked = worker.idle_notify.notified();
                    tokio::pin!(parked);
                    parked.as_mut().enable();
                    let response = worker.dispatch(command, &payload).await;
                    if command == "restore_actions" {
                        worker.work_notify.notify_one();
                        tokio::time::timeout(std::time::Duration::from_secs(5), parked)
                            .await
                            .unwrap();
                        let core = worker.core.lock().unwrap();
                        assert_eq!(
                            (core.busy, core.follow_up.len()),
                            (false, 1),
                            "restored follow-up ran before continuation admission"
                        );
                    }
                    Ok(reply(response))
                }
            })
            .await
            .unwrap()
        );
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut delivered = Vec::new();
            while delivered.len() < 2 {
                let frame = events.recv().await.unwrap();
                let frame: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
                let event = &frame["event"];
                if event["type"] == "message_end" && event["message"]["role"] == "user" {
                    let content = &event["message"]["content"];
                    let text = content
                        .as_str()
                        .or_else(|| content[0]["text"].as_str())
                        .unwrap();
                    delivered.push(text.to_string());
                }
            }
            delivered
        })
        .await
        .unwrap();
        assert_eq!(
            delivered,
            [UPDATE_RESTART_CONTINUATION_PROMPT, "queued follow-up"]
        );
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
    }
}
