use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_types::question::{Question, QuestionKind, QuestionReply, QuestionStatus};
use pa_types::todo::{TodoItem, TodoList, TodoStatus};
use serde_json::{json, Value};

use super::{InteractionEvent, InteractionHub, InteractionSurface};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;

/// A scripted surface: records every event, and its availability is a switch the test flips.
#[derive(Default)]
struct ScriptedSurface {
    available: Mutex<bool>,
    events: Mutex<Vec<InteractionEvent>>,
    opened: tokio::sync::Notify,
}

impl InteractionSurface for ScriptedSurface {
    fn user_available(&self) -> bool {
        *self.available.lock().unwrap()
    }

    fn persisted_todos(&self) -> Option<TodoList> {
        None
    }

    fn publish(&self, event: InteractionEvent) {
        let opened = matches!(event, InteractionEvent::QuestionOpened(_));
        self.events.lock().unwrap().push(event);
        if opened {
            self.opened.notify_one();
        }
    }
}

impl ScriptedSurface {
    fn attached() -> Arc<Self> {
        let surface = Arc::new(Self::default());
        *surface.available.lock().unwrap() = true;
        surface
    }
}

/// Bounded wait for an observable condition: a hang fails the test instead of the suite.
async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("timed out waiting")
}

fn handlers_for(
    hub: &Arc<InteractionHub>,
    session: SessionManager,
) -> (HostRequestHandlers, Arc<tokio::sync::Mutex<SessionManager>>) {
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut handlers = HostRequestHandlers::default();
    hub.register_host_handlers(&session, &mut handlers);
    (handlers, session)
}

async fn call(handlers: &HostRequestHandlers, request_type: &str, data: Value) -> Value {
    let handler = handlers.get(request_type).unwrap().clone();
    handler(HostRequestPayload { data }).await.unwrap()
}

fn item(id: &str, text: &str, status: TodoStatus, note: Option<&str>) -> TodoItem {
    TodoItem {
        id: id.to_string(),
        text: text.to_string(),
        status,
        note: note.map(str::to_string),
    }
}

fn question(kind: QuestionKind, choices: &[&str], default: &[&str]) -> Value {
    json!({
        "id": "q1",
        "text": "Which store?",
        "kind": match kind {
            QuestionKind::Text => "text",
            QuestionKind::Single => "single",
            QuestionKind::Multi => "multi",
        },
        "choices": choices,
        "default": default,
    })
}

/// `todo.set` assigns ids and statuses, `todo.update` patches one item, `todo.get` reads the
/// list back, and the surface sees every change; a reopened session file adopts the newest list.
#[tokio::test]
async fn todo_round_trips_and_survives_a_session_reload() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    let session_file = session.materialize_session_file(Some(dir.path().to_path_buf()));
    // The kernel runs inside a turn, so an assistant row precedes every todo row (and makes the
    // session file durable).
    session
        .append_message(pa_types::session::AgentMessage::Assistant(
            pa_types::ai::AssistantMessage {
                content: vec![],
                api: "openai-completions".to_string(),
                provider: "openai".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pa_types::ai::Usage::default(),
                stop_reason: pa_types::ai::StopReason::ToolUse,
                stop_reason_raw: None,
                error_message: None,
                timestamp: 0,
                rest: serde_json::Map::default(),
            },
        ))
        .unwrap();
    let surface = ScriptedSurface::attached();
    let hub = InteractionHub::with_surface(surface.clone());
    let (handlers, _session) = handlers_for(&hub, session);

    call(
        &handlers,
        "todo.set",
        json!({"items": [
            {"text": "read the code"},
            {"text": "write the fix", "status": "in_progress"},
            {"id": "ship", "text": "ship it", "note": "after review"},
        ]}),
    )
    .await;
    call(
        &handlers,
        "todo.update",
        json!({"id": "1", "status": "done", "note": "skimmed"}),
    )
    .await;
    let expected = TodoList {
        items: vec![
            item("1", "read the code", TodoStatus::Done, Some("skimmed")),
            item("2", "write the fix", TodoStatus::InProgress, None),
            item("ship", "ship it", TodoStatus::Pending, Some("after review")),
        ],
    };
    assert_eq!(
        call(&handlers, "todo.get", json!({})).await,
        serde_json::to_value(&expected).unwrap()
    );
    let published: Vec<InteractionEvent> = surface.events.lock().unwrap().clone();
    assert_eq!(
        published.last(),
        Some(&InteractionEvent::TodosChanged(expected.clone()))
    );

    // A fresh hub over the reopened file (a worker restart, a resume) adopts the same list.
    let reopened = SessionManager::open(dir.path(), dir.path(), &session_file);
    let fresh = InteractionHub::headless();
    fresh.adopt_session(&reopened);
    assert_eq!(fresh.todos(), expected);
}

#[tokio::test]
async fn todo_update_names_the_known_ids_when_one_is_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let hub = InteractionHub::headless();
    let (handlers, _session) = handlers_for(&hub, SessionManager::in_memory(dir.path()));
    call(&handlers, "todo.set", json!({"items": [{"text": "a"}]})).await;
    let handler = handlers.get("todo.update").unwrap().clone();
    let error = handler(HostRequestPayload {
        data: json!({"id": "9", "status": "done"}),
    })
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no todo with id \"9\"; the list holds: 1"
    );
}

/// No user can answer (print, RPC, a subagent nobody watches): the question settles at once as
/// `no_user` with the default, and nothing opens.
#[tokio::test]
async fn ask_without_a_user_settles_at_once_with_the_default() {
    let dir = tempfile::TempDir::new().unwrap();
    for hub in [
        InteractionHub::headless(),
        InteractionHub::with_surface(Arc::new(ScriptedSurface::default())),
    ] {
        let (handlers, _session) = handlers_for(&hub, SessionManager::in_memory(dir.path()));
        let reply = within(call(
            &handlers,
            "question.ask",
            question(QuestionKind::Single, &["sqlite", "redis"], &["sqlite"]),
        ))
        .await;
        assert_eq!(reply, json!({"status": "no_user", "answer": ["sqlite"]}));
        assert_eq!(hub.open_question(), None);
    }
}

/// With a user attached the question opens, waits, and returns the user's reply; the surface
/// sees the open and the close.
#[tokio::test]
async fn ask_returns_the_users_answer() {
    let dir = tempfile::TempDir::new().unwrap();
    let surface = ScriptedSurface::attached();
    let hub = InteractionHub::with_surface(surface.clone());
    let (handlers, _session) = handlers_for(&hub, SessionManager::in_memory(dir.path()));
    let asking = tokio::spawn({
        let handlers = handlers.clone();
        async move {
            call(
                &handlers,
                "question.ask",
                question(QuestionKind::Multi, &["a", "b", "c"], &[]),
            )
            .await
        }
    });
    within(surface.opened.notified()).await;
    let open = Question {
        id: "q1".to_string(),
        text: "Which store?".to_string(),
        kind: QuestionKind::Multi,
        choices: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        default: Vec::new(),
        timeout_seconds: None,
    };
    assert_eq!(hub.open_question(), Some(open.clone()));
    // An answer outside the choices is refused and the question stays open.
    assert!(hub
        .answer(
            "q1",
            QuestionReply::Answered {
                answer: vec!["z".to_string()]
            }
        )
        .is_err());
    hub.answer(
        "q1",
        QuestionReply::Answered {
            answer: vec!["a".to_string(), "c".to_string()],
        },
    )
    .unwrap();
    assert_eq!(
        within(asking).await.unwrap(),
        json!({"status": "answered", "answer": ["a", "c"]})
    );
    assert_eq!(
        surface.events.lock().unwrap().clone(),
        vec![
            InteractionEvent::QuestionOpened(open),
            InteractionEvent::QuestionClosed {
                question_id: "q1".to_string(),
                status: QuestionStatus::Answered,
            },
        ]
    );
}

/// The deadline submits the default as `timed_out`; a user who detaches mid-question leaves
/// `no_user`; an interrupted cell's `question.cancel` settles `cancelled`.
#[tokio::test(start_paused = true)]
async fn ask_settles_on_timeout_detach_and_cancel() {
    let dir = tempfile::TempDir::new().unwrap();
    let surface = ScriptedSurface::attached();
    let hub = InteractionHub::with_surface(surface.clone());
    let (handlers, _session) = handlers_for(&hub, SessionManager::in_memory(dir.path()));

    let mut timed = question(QuestionKind::Text, &[], &["keep going"]);
    timed["timeoutSeconds"] = json!(30);
    assert_eq!(
        call(&handlers, "question.ask", timed).await,
        json!({"status": "timed_out", "answer": ["keep going"]})
    );

    let detach = tokio::spawn({
        let handlers = handlers.clone();
        async move {
            call(
                &handlers,
                "question.ask",
                question(QuestionKind::Single, &["x", "y"], &["y"]),
            )
            .await
        }
    });
    surface.opened.notified().await;
    *surface.available.lock().unwrap() = false;
    hub.presence_changed();
    assert_eq!(
        detach.await.unwrap(),
        json!({"status": "no_user", "answer": ["y"]})
    );

    *surface.available.lock().unwrap() = true;
    let cancelled = tokio::spawn({
        let handlers = handlers.clone();
        async move {
            call(
                &handlers,
                "question.ask",
                question(QuestionKind::Text, &[], &[]),
            )
            .await
        }
    });
    surface.opened.notified().await;
    call(&handlers, "question.cancel", json!({"id": "q1"})).await;
    assert_eq!(
        cancelled.await.unwrap(),
        json!({"status": "cancelled", "answer": []})
    );
    assert_eq!(hub.open_question(), None);
}
