//! Session todos, user questions, and notifications: the host side of the kernel's `todo.*`,
//! `ask(...)`, and `notify(...)`.
//!
//! The [`InteractionHub`] owns one session's todo list (persisted as `session_todo_list` custom
//! entries, newest on the branch wins, so it survives a kernel restart and a session reload) and
//! its single open question. The embedding supplies an [`InteractionSurface`]: the daemon worker
//! publishes the hub's events to attached clients and answers whether anyone can reply; print,
//! RPC and other headless embeddings supply none, so `ask` settles at once as `no_user` and never
//! waits.

mod notify;
mod question;
mod todo;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, OnceLock};

use pa_types::question::{Question, QuestionReply, QuestionStatus};
use pa_types::todo::TodoList;

use crate::kernel::shared::HostRequestHandlers;
use crate::session::manager::SessionManager;
use crate::session_engine::telemetry::SessionCounters;

pub(crate) use todo::persisted_todo_list;

/// One change the hub publishes to the attached surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionEvent {
    /// A kernel request committed a new todo list: an embedding that owns persistence outside
    /// the session manager records it durably.
    TodosChanged(TodoList),
    /// A (re)built session adopted its persisted list: display only, already durable.
    TodosAdopted(TodoList),
    /// A question opened and waits for a reply.
    QuestionOpened(Question),
    /// The open question settled.
    QuestionClosed {
        question_id: String,
        status: QuestionStatus,
    },
    /// A kernel `notify(...)` ping: shown, never waited on.
    Notified(pa_types::notification::KernelNotification),
}

/// The embedding's user-facing side of an [`InteractionHub`].
///
/// The hub calls it from kernel host-request tasks and session builds, so every method must
/// return promptly: no network, no awaiting a client. Implementations publish events
/// fire-and-forget (the daemon worker sequences them as `session_event` frames) and answer from
/// state they already hold.
pub trait InteractionSurface: Send + Sync {
    /// Whether a person who can answer a question is attached right now. Consulted when a
    /// question opens and again after every [`InteractionHub::presence_changed`].
    fn user_available(&self) -> bool;

    /// Deliver one event to the attached surfaces (and, for
    /// [`InteractionEvent::TodosChanged`], to the embedding's durable store when it owns one).
    fn publish(&self, event: InteractionEvent);

    /// The durable todo list when the embedding owns the session file outside the session
    /// manager (the daemon worker); `None` defers to the session manager's branch.
    fn persisted_todos(&self) -> Option<TodoList>;
}

/// One session's todo list and open question (see the module docs).
pub struct InteractionHub {
    surface: Option<Arc<dyn InteractionSurface>>,
    todos: Mutex<TodoList>,
    question: Mutex<Option<question::OpenQuestion>>,
    presence: tokio::sync::Notify,
    counters: OnceLock<Arc<SessionCounters>>,
}

impl InteractionHub {
    /// A hub with no user: `ask` answers `no_user` immediately.
    #[must_use]
    pub fn headless() -> Arc<Self> {
        Arc::new(Self::build(None))
    }

    /// A hub whose events reach `surface` and whose questions wait for its user.
    #[must_use]
    pub fn with_surface(surface: Arc<dyn InteractionSurface>) -> Arc<Self> {
        Arc::new(Self::build(Some(surface)))
    }

    fn build(surface: Option<Arc<dyn InteractionSurface>>) -> Self {
        Self {
            surface,
            todos: Mutex::new(TodoList::default()),
            question: Mutex::new(None),
            presence: tokio::sync::Notify::new(),
            counters: OnceLock::new(),
        }
    }

    /// The current todo list (the attach snapshot's `state.todos`).
    #[must_use]
    pub fn todos(&self) -> TodoList {
        lock(&self.todos).clone()
    }

    /// The open question, if any (the attach snapshot's `state.pendingQuestion`).
    #[must_use]
    pub fn open_question(&self) -> Option<Question> {
        lock(&self.question)
            .as_ref()
            .map(|open| open.question.clone())
    }

    /// Deliver a surface's reply to the open question.
    ///
    /// # Errors
    ///
    /// Returns an error when no question with `question_id` is open (it already settled), or the
    /// answer does not fit the question (an unknown choice, two choices for a single-choice
    /// question).
    pub fn answer(&self, question_id: &str, reply: QuestionReply) -> anyhow::Result<()> {
        question::answer(self, question_id, reply)
    }

    /// The set of attached users changed: a waiting question re-checks
    /// [`InteractionSurface::user_available`] and settles as `no_user` when nobody is left.
    pub fn presence_changed(&self) {
        self.presence.notify_waiters();
    }

    /// Settle the open question as `cancelled` (the session is closing).
    pub fn cancel_open_question(&self) {
        question::cancel(self, None);
    }

    /// Count adoption into the session's telemetry counters (set once, by the engine).
    pub(crate) fn set_counters(&self, counters: Arc<SessionCounters>) {
        let _ = self.counters.set(counters);
    }

    /// Adopt a (re)built session's persisted list and announce it.
    pub(crate) fn adopt_session(&self, session: &SessionManager) {
        let todos = self
            .surface
            .as_ref()
            .and_then(|surface| surface.persisted_todos())
            .or_else(|| session.active_todo_list())
            .unwrap_or_default();
        *lock(&self.todos) = todos.clone();
        self.publish(InteractionEvent::TodosAdopted(todos));
    }

    /// Register the `todo.*`, `question.*`, and `notify.send` host requests.
    pub(crate) fn register_host_handlers(
        self: &Arc<Self>,
        session: &Arc<tokio::sync::Mutex<SessionManager>>,
        handlers: &mut HostRequestHandlers,
    ) {
        todo::register(self, session, handlers);
        question::register(self, handlers);
        notify::register(self, handlers);
    }

    fn publish(&self, event: InteractionEvent) {
        if let Some(surface) = &self.surface {
            surface.publish(event);
        }
    }

    /// Ask the user a host-initiated question (a confirmation a kernel request needs) through
    /// the same single open-question slot and surfaces as the kernel's `ask`.
    pub(crate) async fn ask(
        &self,
        question: Question,
    ) -> anyhow::Result<(QuestionStatus, Vec<String>)> {
        question::ask(self, question).await
    }

    pub(crate) fn counters(&self) -> Option<&Arc<SessionCounters>> {
        self.counters.get()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
