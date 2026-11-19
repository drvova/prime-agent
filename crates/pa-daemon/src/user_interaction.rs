//! The daemon worker's side of the session todo list and kernel questions: the
//! [`InteractionSurface`] that frames the hub's events as session events and answers whether an
//! attached client can reply, plus the worker arms that feed the hub (attach capability, detach,
//! the `answer_question` command).
//!
//! A user is available while at least one attached client declared the `user_questions`
//! capability. Clients that never declare it (RPC, ACP, an older TUI) never see a question wait
//! for them: the kernel's `ask` settles as `no_user` at once.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pa_core::interaction::{InteractionEvent, InteractionHub, InteractionSurface};
use pa_types::question::{
    QuestionReply, QUESTIONS_CAPABILITY, QUESTION_CLOSED_EVENT, QUESTION_OPEN_EVENT,
};
use pa_types::todo::{TodoList, TODO_LIST_CUSTOM_TYPE, TODO_UPDATE_EVENT};
use serde_json::{json, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::{emit_worker_event_with, EventPump, SessionCore};

/// The worker's interaction surface: the session core (attached client ids) and the frame queue
/// into the event pump.
pub(crate) struct WorkerInteractionSurface {
    core: Arc<Mutex<SessionCore>>,
    /// Frames for the pump task, which sequences each one under the core lock: `publish` only
    /// enqueues, so no hub caller ever waits on the event pump.
    frames: tokio::sync::mpsc::UnboundedSender<Value>,
    /// Client ids whose attach declared `user_questions`; a question waits only while one of
    /// them is still attached.
    answering_clients: Mutex<HashSet<String>>,
}

impl InteractionSurface for WorkerInteractionSurface {
    fn user_available(&self) -> bool {
        // The core lock first, released before the set's: the attach arm records into the set
        // while it may still hold the core lock, so the two are never held in reverse order.
        let attached = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attached_client_ids
            .clone();
        let answering = self
            .answering_clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attached.iter().any(|id| answering.contains(id))
    }

    fn publish(&self, event: InteractionEvent) {
        let frame = match event {
            InteractionEvent::TodosChanged(todos) => {
                // The worker's session file is the durable store (the engine's session manager
                // is in-memory): the row lands before the kernel's request returns.
                if let Some(store) = self
                    .core
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .store
                    .as_mut()
                {
                    let _ = store.persist_entry(
                        "custom",
                        json!({ "customType": TODO_LIST_CUSTOM_TYPE, "data": todos }),
                    );
                }
                json!({ "type": TODO_UPDATE_EVENT, "todos": todos })
            }
            InteractionEvent::TodosAdopted(todos) => {
                json!({ "type": TODO_UPDATE_EVENT, "todos": todos })
            }
            InteractionEvent::QuestionOpened(question) => {
                json!({ "type": QUESTION_OPEN_EVENT, "question": question })
            }
            InteractionEvent::QuestionClosed {
                question_id,
                status,
            } => json!({
                "type": QUESTION_CLOSED_EVENT,
                "questionId": question_id,
                "status": status,
            }),
            InteractionEvent::Notified(notification) => json!({
                "type": pa_types::notification::NOTIFICATION_EVENT,
                "notification": notification,
            }),
        };
        // A send fails only once the pump task ended with the worker.
        let _ = self.frames.send(frame);
    }

    fn persisted_todos(&self) -> Option<TodoList> {
        let core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        core.store
            .as_ref()?
            .branch()
            .iter()
            .rev()
            .find_map(|entry| {
                (entry.type_ == "custom"
                    && entry.fields.get("customType").and_then(Value::as_str)
                        == Some(TODO_LIST_CUSTOM_TYPE))
                .then(|| serde_json::from_value(entry.fields.get("data")?.clone()).ok())
                .flatten()
            })
    }
}

/// The worker's handle on its session's hub and surface.
pub(crate) struct WorkerInteraction {
    pub(crate) hub: Arc<InteractionHub>,
    surface: Arc<WorkerInteractionSurface>,
}

impl WorkerInteraction {
    /// Must run inside the worker's tokio runtime: it spawns the frame pump.
    pub(crate) fn new(core: Arc<Mutex<SessionCore>>, events: Arc<EventPump>) -> Self {
        let (frames, mut queued) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let pump_core = Arc::clone(&core);
        tokio::spawn(async move {
            while let Some(frame) = queued.recv().await {
                emit_worker_event_with(&pump_core, &events, frame);
            }
        });
        let surface = Arc::new(WorkerInteractionSurface {
            core,
            frames,
            answering_clients: Mutex::new(HashSet::new()),
        });
        Self {
            hub: InteractionHub::with_surface(Arc::clone(&surface) as Arc<dyn InteractionSurface>),
            surface,
        }
    }

    /// An attach registered `client_id`: remember whether it can answer questions. Call after
    /// the core holds the id, so the presence re-check sees it.
    pub(crate) fn note_attach(&self, client_id: &str, client_capabilities: &[String]) {
        if client_capabilities
            .iter()
            .any(|capability| capability == QUESTIONS_CAPABILITY)
        {
            self.surface
                .answering_clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(client_id.to_string());
        }
        self.hub.presence_changed();
    }

    /// The `answer_question` command: deliver the reply to the open question.
    pub(crate) fn handle_answer(&self, payload: &Value) -> DaemonResponse {
        let question_id = payload
            .get("questionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reply = match payload
            .get("reply")
            .cloned()
            .map(serde_json::from_value::<QuestionReply>)
        {
            Some(Ok(reply)) => reply,
            Some(Err(error)) => {
                return response_failure(
                    None,
                    "answer_question",
                    &format!("invalid reply: {error}"),
                    None,
                )
            }
            None => return response_failure(None, "answer_question", "missing reply", None),
        };
        match self.hub.answer(question_id, reply) {
            Ok(()) => response_success(None, "answer_question", None),
            Err(error) => response_failure(None, "answer_question", &error.to_string(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{Worker, WorkerConfig};
    use pa_types::question::{Question, QuestionKind};

    async fn scripted_worker() -> (tempfile::TempDir, Arc<Worker>) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let worker = Arc::new(Worker::new(
            WorkerConfig {
                socket_path: dir.join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "token".to_string(),
                worker_instance_id: String::new(),
                active_session_id: "ask-session".to_string(),
                agent_dir: dir.join("agent"),
                recovery_journal_path: dir.join("recovery.jsonl"),
                telemetry_disabled: None,
                script: Some(json!({ "responses": ["ack"] })),
            },
            None,
        ));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "ask" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        (tmp, worker)
    }

    fn attach(client_id: &str, capabilities: &[&str]) -> Value {
        json!({
            "activeSessionId": "ask-session",
            "clientId": client_id,
            "capabilities": capabilities,
        })
    }

    /// Only an attached client that declared `user_questions` makes the user available; an RPC
    /// client never does, and the answering client's detach takes availability away.
    #[tokio::test]
    async fn a_user_is_available_only_while_an_answering_client_is_attached() {
        let (_tmp, worker) = scripted_worker().await;
        let surface = &worker.interaction.surface;
        assert!(
            worker
                .dispatch("attach", &attach("rpc", &["attach_snapshot"]))
                .await
                .success
        );
        assert!(!surface.user_available());
        assert!(
            worker
                .dispatch(
                    "attach",
                    &attach("tui", &["attach_snapshot", QUESTIONS_CAPABILITY])
                )
                .await
                .success
        );
        assert!(surface.user_available());
        assert!(
            worker
                .dispatch("detach", &json!({ "clientId": "tui" }))
                .await
                .success
        );
        assert!(!surface.user_available());
    }

    /// A committed todo list lands in the worker's session file (the durable store; the engine's
    /// session manager is in-memory), and a rebuilt session adopts it from there.
    #[tokio::test]
    async fn committed_todos_persist_in_the_worker_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let worker = Worker::new(
            WorkerConfig {
                socket_path: dir.join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "token".to_string(),
                worker_instance_id: String::new(),
                active_session_id: "todo-session".to_string(),
                agent_dir: dir.join("agent"),
                recovery_journal_path: dir.join("recovery.jsonl"),
                telemetry_disabled: None,
                script: Some(json!({ "responses": ["ack"] })),
            },
            None,
        );
        let created = worker
            .dispatch(
                "create",
                &json!({ "cwd": dir.display().to_string(), "sessionDir": dir.join("sessions").display().to_string() }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let list = TodoList {
            items: vec![pa_types::todo::TodoItem {
                id: "1".to_string(),
                text: "ship".to_string(),
                status: pa_types::todo::TodoStatus::InProgress,
                note: None,
            }],
        };
        let surface = &worker.interaction.surface;
        assert_eq!(surface.persisted_todos(), None);
        surface.publish(InteractionEvent::TodosChanged(list.clone()));
        assert_eq!(surface.persisted_todos(), Some(list));
    }

    /// The hub's events reach clients as session events, and a reply for a question that is
    /// not open is refused.
    #[tokio::test]
    async fn questions_frame_as_session_events_and_stale_answers_fail() {
        let (_tmp, worker) = scripted_worker().await;
        let mut subscription = worker.events.subscribe();
        let question = Question {
            id: "q1".to_string(),
            text: "Proceed?".to_string(),
            kind: QuestionKind::Single,
            choices: vec!["yes".to_string(), "no".to_string()],
            default: Vec::new(),
            timeout_seconds: None,
        };
        worker
            .interaction
            .surface
            .publish(InteractionEvent::QuestionOpened(question.clone()));
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = subscription.recv().await.expect("the pump stays open");
                if frame.outbound_type == "session_event" {
                    let outbound: Value = serde_json::from_slice(&frame.payload).unwrap();
                    break outbound["event"].clone();
                }
            }
        })
        .await
        .expect("the frame is pumped");
        assert_eq!(
            frame,
            json!({ "type": QUESTION_OPEN_EVENT, "question": question })
        );
        let stale = worker
            .dispatch(
                "answer_question",
                &json!({
                    "activeSessionId": "ask-session",
                    "questionId": "q1",
                    "reply": { "status": "declined" },
                }),
            )
            .await;
        assert_eq!(stale.error.as_deref(), Some("question \"q1\" is not open"));
    }
}
