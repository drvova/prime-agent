//! A runtime-daemon session's todo list, kernel questions and notifications
//! ([`SessionInteraction`]): the [`InteractionSurface`] of each lifetime's
//! [`InteractionHub`], ported from the old daemon worker's
//! (`pa-daemon` `user_interaction.rs`).
//!
//! - **Events.** The hub's events go out on the session's stream,
//!   `agent/<id>/stream`, as the old worker's `session_event` payloads:
//!   `todo_update {todos}`, `question_open {question}`,
//!   `question_closed {questionId, status}` and `kernel_notification
//!   {notification}`, each numbered with the session's other events. The
//!   json stream's shapes (a print run) carry none of them, and the list a
//!   lifetime adopts as its engine builds goes out only on a rebuild: the
//!   first build's stream has not chosen its shape yet, and no client of
//!   the old worker saw it either before attaching.
//! - **Who can answer.** A question waits only while a client that declared
//!   the `user_questions` capability is attached to the session: a
//!   `session.attach` that names it on a connection makes the connection's
//!   client one, until its `session.detach` (of that client, or of every
//!   client of the connection when it names none) or the connection's end. Any
//!   other client (ACP, the CLI, a client that never attaches) makes no
//!   question wait: the kernel's `ask` settles as `no_user` at once, as on
//!   the old daemon.
//! - **Replies.** `session.answer_question {questionId, reply}` delivers the
//!   reply to the open question, with the old worker's refusals.
//!
//! The todo list itself is the session manager's: the runtime's engine
//! writes its transcript, so the hub's own `session_todo_list` row is the
//! durable one (the old worker kept its file outside the engine).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use pa_core::interaction::{InteractionEvent, InteractionHub, InteractionSurface};
use pa_types::question::{
    Question, QuestionReply, QUESTIONS_CAPABILITY, QUESTION_CLOSED_EVENT, QUESTION_OPEN_EVENT,
};
use pa_types::todo::{TodoList, TODO_UPDATE_EVENT};
use serde_json::{json, Value};

use super::SessionStream;
use crate::lock;
use crate::wire::ConnectionId;

/// One session's interaction surface: its stream, the clients attached to
/// it, and the hub of its current lifetime.
pub(crate) struct SessionInteraction {
    stream: Arc<SessionStream>,
    /// Each attached client, by its connection and client id: whether its
    /// attach declared `user_questions`.
    attached: Mutex<BTreeMap<(ConnectionId, String), bool>>,
    /// The current lifetime's hub (a rebuilt engine has a new one).
    hub: Mutex<Weak<InteractionHub>>,
}

impl SessionInteraction {
    pub(crate) fn new(stream: Arc<SessionStream>) -> Self {
        SessionInteraction {
            stream,
            attached: Mutex::new(BTreeMap::new()),
            hub: Mutex::new(Weak::new()),
        }
    }

    /// A new lifetime's hub, whose events reach the session's stream; the
    /// one the calls below reach from now on.
    pub(crate) fn lifetime_hub(self: &Arc<Self>) -> Arc<InteractionHub> {
        let hub = InteractionHub::with_surface(Arc::clone(self) as Arc<dyn InteractionSurface>);
        *lock(&self.hub) = Arc::downgrade(&hub);
        hub
    }

    fn hub(&self) -> Option<Arc<InteractionHub>> {
        lock(&self.hub).upgrade()
    }

    /// The question waiting for a reply, if any (the attach snapshot's
    /// `state.pendingQuestion`).
    pub(crate) fn open_question(&self) -> Option<Question> {
        self.hub().and_then(|hub| hub.open_question())
    }

    /// `client_id` attached on `connection` with `capabilities`: remember
    /// whether it can answer questions, and let a waiting question look
    /// again.
    pub(crate) fn attached(
        &self,
        connection: ConnectionId,
        client_id: &str,
        capabilities: &[String],
    ) {
        let answers = capabilities
            .iter()
            .any(|capability| capability == QUESTIONS_CAPABILITY);
        lock(&self.attached).insert((connection, client_id.to_string()), answers);
        self.presence_changed();
    }

    /// `client` detached on `connection`; every client the connection
    /// attached when it names none (a connection is one client: the TUI's
    /// detach names no client id, the old supervisor added its attach's).
    pub(crate) fn detached(&self, connection: ConnectionId, client: Option<&str>) {
        lock(&self.attached).retain(|(held, id), _| {
            *held != connection || client.is_some_and(|client| client != id)
        });
        self.presence_changed();
    }

    /// `connection` ended: every client it attached is gone.
    pub(crate) fn disconnected(&self, connection: ConnectionId) {
        self.detached(connection, None);
    }

    /// A waiting question re-checks whether anyone can still answer it.
    fn presence_changed(&self) {
        if let Some(hub) = self.hub() {
            hub.presence_changed();
        }
    }

    /// The session is closing: a question still waiting settles as
    /// `cancelled`, since no client will answer it.
    pub(crate) fn closing(&self) {
        if let Some(hub) = self.hub() {
            hub.cancel_open_question();
        }
    }

    /// `session.answer_question`: delivers `args.reply` to the open question
    /// `args.questionId` (the old worker's `handle_answer`).
    ///
    /// # Errors
    ///
    /// The reply is missing or malformed, no question with that id is open,
    /// or the answer does not fit the question.
    pub(crate) fn answer(&self, args: &Value) -> anyhow::Result<()> {
        let question_id = args
            .get("questionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reply = match args
            .get("reply")
            .cloned()
            .map(serde_json::from_value::<QuestionReply>)
        {
            Some(Ok(reply)) => reply,
            Some(Err(error)) => anyhow::bail!("invalid reply: {error}"),
            None => anyhow::bail!("missing reply"),
        };
        match self.hub() {
            Some(hub) => hub.answer(question_id, reply),
            // No lifetime is running: nothing is open.
            None => anyhow::bail!("question \"{question_id}\" is not open"),
        }
    }
}

impl InteractionSurface for SessionInteraction {
    fn user_available(&self) -> bool {
        lock(&self.attached).values().any(|answers| *answers)
    }

    fn publish(&self, event: InteractionEvent) {
        let payload = match event {
            InteractionEvent::TodosChanged(todos) | InteractionEvent::TodosAdopted(todos) => {
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
        self.stream.publish_session_event(&payload);
    }

    fn persisted_todos(&self) -> Option<TodoList> {
        // The session manager's branch is the durable list.
        None
    }
}
