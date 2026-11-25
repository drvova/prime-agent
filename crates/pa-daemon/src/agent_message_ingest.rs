//! The agent-message ingestion surface: the worker arms for
//! `agent_messages_status`/`_pause`/`_resume`/`_clear`, plus the paused
//! delivery gate, and the per-(sender, target) wake budget. The per-sender
//! rate bucket the status reports is enforced on the sending side, in
//! pa-core's `agent_message.send` handler; the wake budget is enforced here,
//! by the receiving worker, so no sender can spend another's.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pa_core::session_engine::agent_messaging::{
    DEFAULT_AGENT_MESSAGE_MAX_CHARS, DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
    DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY, DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
    DEFAULT_AGENT_MESSAGE_WAKE_BUDGET, DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS,
};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

/// The worker's agent-message ingestion state: the pause flag and the wake budget.
pub(crate) struct AgentMessageIngest {
    paused: AtomicBool,
    /// Each sender's wakes of this (idle) session inside the trailing
    /// [`DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS`], oldest first, keyed by the
    /// sender's durable session id.
    wakes: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl AgentMessageIngest {
    pub(crate) fn new() -> Self {
        AgentMessageIngest {
            paused: AtomicBool::new(false),
            wakes: Mutex::new(HashMap::new()),
        }
    }

    /// Spend one of `sender`'s wakes of this idle session, or answer the ms until its oldest
    /// counted wake leaves the window. Two idle agents that answer each other's every message
    /// stop here instead of ping-ponging forever.
    pub(crate) fn admit_wake(&self, sender: &str, now: Instant) -> Result<(), u64> {
        let window = Duration::from_millis(DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS);
        let mut wakes = self
            .wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Forget every sender's aged-out wakes, so the map holds only live windows.
        wakes.retain(|_, times| {
            while times
                .front()
                .is_some_and(|oldest| now.saturating_duration_since(*oldest) >= window)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = wakes.entry(sender.to_string()).or_default();
        if times.len() >= DEFAULT_AGENT_MESSAGE_WAKE_BUDGET {
            let oldest = times.front().copied().unwrap_or(now);
            let retry_after = window.saturating_sub(now.saturating_duration_since(oldest));
            return Err(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX));
        }
        times.push_back(now);
        Ok(())
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }
}

impl Default for AgentMessageIngest {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker {
    /// The safety-status wire object.
    fn agent_message_safety_status(&self) -> Value {
        json!({
            "paused": self.agent_messages.paused(),
            "maxMessageChars": DEFAULT_AGENT_MESSAGE_MAX_CHARS,
            "maxPendingPerSession": DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
            "rateLimitCapacity": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY,
            "rateLimitRefillMs": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
        })
    }

    /// `agent_messages_status`: the safety status, no side effects.
    pub(crate) fn handle_agent_messages_status(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_status") {
            return response;
        }
        response_success(
            None,
            "agent_messages_status",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_pause`: set the flag, drop every queued agent-message item, answer the
    /// safety status.
    pub(crate) fn handle_agent_messages_pause(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_pause") {
            return response;
        }
        self.agent_messages.set_paused(true);
        let cleared = self.clear_queued_agent_messages();
        let _ = cleared;
        response_success(
            None,
            "agent_messages_pause",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_resume`: clear the flag, answer the safety status.
    pub(crate) fn handle_agent_messages_resume(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_resume") {
            return response;
        }
        self.agent_messages.set_paused(false);
        response_success(
            None,
            "agent_messages_resume",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_clear`: drop queued agent-message items, answer the TS shape (removed
    /// prompts per lane).
    pub(crate) fn handle_agent_messages_clear(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_clear") {
            return response;
        }
        let cleared = self.clear_queued_agent_messages();
        response_success(None, "agent_messages_clear", Some(cleared))
    }

    /// The delivery gate's paused check: the exact TS error string, surfaced
    /// through `worker_deliver_message`.
    #[expect(
        clippy::result_large_err,
        reason = "the error channel carries the whole wire response, which is wide by design"
    )]
    pub(crate) fn refuse_delivery_if_paused(&self) -> Result<(), DaemonResponse> {
        if self.agent_messages.paused() {
            return Err(response_failure(
                None,
                "worker_deliver_message",
                "Agent messaging is paused",
                None,
            ));
        }
        Ok(())
    }

    /// Remove the queued agent-message items from both lanes (never
    /// client-queued prompts), in the `{ steering, followUp }` shape.
    fn clear_queued_agent_messages(&self) -> Value {
        let mut core = self.core.lock().unwrap();
        let mut steering = Vec::new();
        let mut follow_up = Vec::new();
        let mut retained_steering = std::collections::VecDeque::new();
        while let Some(item) = core.steering.pop_front() {
            match item.agent_message {
                Some(_) => steering.push(item.message),
                None => retained_steering.push_back(item),
            }
        }
        core.steering = retained_steering;
        let mut retained_follow_up = std::collections::VecDeque::new();
        while let Some(item) = core.follow_up.pop_front() {
            match item.agent_message {
                Some(_) => follow_up.push(item.message),
                None => retained_follow_up.push_back(item),
            }
        }
        core.follow_up = retained_follow_up;
        crate::worker::clear_withdrawn_progress_note(&core, self.engine.as_ref());
        let snapshot = Self::snapshot_locked(&core);
        drop(core);
        // The sweep may have settled the session: the verdict follows the remaining lanes.
        self.checkpoint_queue(crate::worker::QueueCheckpoint::Settle {
            operation: "queue_mutated",
        });
        let _ = self.emit_action_update(&snapshot);
        json!({ "steering": steering, "followUp": follow_up })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    async fn created_worker() -> (tempfile::TempDir, Arc<Worker>) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "ami-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "ami" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        (tmp, worker)
    }

    /// A pause clears queued agent messages but keeps client prompts.
    #[tokio::test]
    async fn status_pause_resume_and_clear_match_ts_shapes() {
        let (_tmp, worker) = created_worker().await;

        let status = worker.dispatch("agent_messages_status", &json!({})).await;
        assert!(status.success);
        assert_eq!(
            status.data,
            Some(json!({
                "paused": false,
                "maxMessageChars": 16384,
                "maxPendingPerSession": 20,
                "rateLimitCapacity": 3,
                "rateLimitRefillMs": 1000,
            }))
        );

        // One agent-message delivery and one client steer, both on the steering lane.
        let delivered = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "from a peer",
                    "sender": { "activeSessionId": "peer-1" },
                }),
            )
            .await;
        assert!(delivered.success, "delivery failed: {delivered:?}");
        worker
            .dispatch(
                "steer",
                &json!({ "activeSessionId": "ami-session", "message": "client text" }),
            )
            .await;

        // Pause: the flag flips and the queued agent message is dropped; the client prompt stays.
        let paused = worker.dispatch("agent_messages_pause", &json!({})).await;
        assert!(paused.success);
        assert_eq!(paused.data.as_ref().unwrap()["paused"], json!(true));
        {
            let core = worker.core.lock().unwrap();
            let texts: Vec<&str> = core
                .steering
                .iter()
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(texts, vec!["client text"]);
        }

        // A delivery while paused answers the TS gate error.
        let refused = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "while paused",
                    "sender": { "activeSessionId": "peer-1" },
                }),
            )
            .await;
        assert!(!refused.success);
        assert_eq!(refused.error.as_deref(), Some("Agent messaging is paused"));

        // Resume flips the flag back.
        let resumed = worker.dispatch("agent_messages_resume", &json!({})).await;
        assert_eq!(resumed.data.as_ref().unwrap()["paused"], json!(false));

        // Clear answers the TS removed-prompts shape; the client prompt stays.
        let cleared = worker.dispatch("agent_messages_clear", &json!({})).await;
        assert!(cleared.success);
        assert_eq!(
            cleared.data,
            Some(json!({ "steering": [], "followUp": [] }))
        );
        {
            let core = worker.core.lock().unwrap();
            let texts: Vec<&str> = core
                .steering
                .iter()
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(texts, vec!["client text"]);
        }
    }

    #[tokio::test]
    async fn clear_reports_the_follow_up_lane() {
        let (_tmp, worker) = created_worker().await;
        let delivered = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "queued for later",
                    "sender": { "activeSessionId": "peer-1" },
                    "deliveryMode": "follow_up",
                }),
            )
            .await;
        assert!(delivered.success, "delivery failed: {delivered:?}");
        let cleared = worker.dispatch("agent_messages_clear", &json!({})).await;
        // The removed text is the queued turn's prompt (TS `payload.text`).
        assert_eq!(
            cleared.data,
            Some(json!({
                "steering": [],
                "followUp": ["[agent-message from peer-1]\n\nqueued for later"],
            }))
        );
    }
}
