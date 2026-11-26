//! The queries and calls a client may send, and what answers them.
//!
//! On the wire a query or call names its operation as a string and carries
//! its arguments as JSON (`pa_types::wire2::ClientFrame::{Query, Call}`).
//! The server parses the name into [`Query`] or [`Call`] here and refuses an
//! unknown one with an error reply, so a client newer or older than the
//! runtime loses that one request, not its connection (a typed enum in the
//! frame itself would fail the whole frame, which has no id to reply to).
//! The arguments and the result stay JSON: their shapes arrive with the
//! storage-backed implementations that answer them.

use std::future::Future;
use std::str::FromStr;

use serde_json::Value;

/// Something a client reads that the log does not stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Query {
    /// `sessions.list`: the saved and running sessions.
    SessionsList,
    /// `session.snapshot`: one session's state, to attach to it.
    SessionSnapshot,
    /// `transcript.page`: a page of a session's transcript.
    TranscriptPage,
    /// `settings`: the effective settings.
    Settings,
    /// `models`: the model catalog.
    Models,
    /// `sources`: the event sources and their timers.
    Sources,
    /// `subscriptions`: the subscriptions on the bus.
    Subscriptions,
    /// `tasks`: queued and running tasks.
    Tasks,
    /// `session.context`: the messages the session's next request is built
    /// from, with its model and thinking level.
    SessionContext,
    /// `session.usage`: the session's message counts, token spend and
    /// context use.
    SessionUsage,
    /// `session.tree`: the session's entries as a tree (id, parent, kind,
    /// label) and its leaf.
    SessionTree,
    /// `session.context_tree`: the session's place in its family with its
    /// spend and context use.
    SessionContextTree,
    /// `session.breakdown`: the context window's details (the next
    /// request's estimate by category, prompt-cache totals, compactions,
    /// spend).
    SessionBreakdown,
}

/// A name that is not a query or call this runtime answers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this runtime has no {kind} {name:?}")]
pub struct UnknownOperation {
    /// `query` or `call`.
    pub kind: &'static str,
    pub name: String,
}

impl FromStr for Query {
    type Err = UnknownOperation;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "sessions.list" => Query::SessionsList,
            "session.snapshot" => Query::SessionSnapshot,
            "transcript.page" => Query::TranscriptPage,
            "settings" => Query::Settings,
            "models" => Query::Models,
            "sources" => Query::Sources,
            "subscriptions" => Query::Subscriptions,
            "tasks" => Query::Tasks,
            "session.context" => Query::SessionContext,
            "session.usage" => Query::SessionUsage,
            "session.tree" => Query::SessionTree,
            "session.context_tree" => Query::SessionContextTree,
            "session.breakdown" => Query::SessionBreakdown,
            unknown => {
                return Err(UnknownOperation {
                    kind: "query",
                    name: unknown.to_string(),
                })
            }
        })
    }
}

/// A change a client asks for that is not an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// `session.create`
    SessionCreate,
    /// `session.stop`
    SessionStop,
    /// `session.rename`
    SessionRename,
    /// `session.delete`
    SessionDelete,
    /// `session.set_model`
    SessionSetModel,
    /// `session.set_effort`
    SessionSetEffort,
    /// `session.fork`
    SessionFork,
    /// `session.label`
    SessionLabel,
    /// `session.export`
    SessionExport,
    /// `session.abort`: aborts the running turn and parks the session's
    /// queue (nothing queued starts until `session.resume_queue`).
    SessionAbort,
    /// `session.abort_and_send_queued`: aborts the running turn and keeps
    /// the queue going: the steers queued run next, delivered together.
    SessionAbortAndSendQueued,
    /// `session.resume_queue`: starts a parked queue again.
    SessionResumeQueue,
    /// `session.withdraw`: takes the session's queued input back out of its
    /// queue and answers it.
    SessionWithdraw,
    /// `session.restore`: puts input back at the front of the session's
    /// queue as steers, delivered together as its next turn.
    SessionRestore,
    /// `session.compact`: compacts the session's context in its lane and
    /// answers how it went.
    SessionCompact,
}

impl FromStr for Call {
    type Err = UnknownOperation;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "session.create" => Call::SessionCreate,
            "session.stop" => Call::SessionStop,
            "session.rename" => Call::SessionRename,
            "session.delete" => Call::SessionDelete,
            "session.set_model" => Call::SessionSetModel,
            "session.set_effort" => Call::SessionSetEffort,
            "session.fork" => Call::SessionFork,
            "session.label" => Call::SessionLabel,
            "session.export" => Call::SessionExport,
            "session.abort" => Call::SessionAbort,
            "session.abort_and_send_queued" => Call::SessionAbortAndSendQueued,
            "session.resume_queue" => Call::SessionResumeQueue,
            "session.withdraw" => Call::SessionWithdraw,
            "session.restore" => Call::SessionRestore,
            "session.compact" => Call::SessionCompact,
            unknown => {
                return Err(UnknownOperation {
                    kind: "call",
                    name: unknown.to_string(),
                })
            }
        })
    }
}

/// Answers the queries and calls of the wire's clients.
///
/// The server runs each query or call on its own task, up to a few at once
/// per connection, so a slow answer delays neither the connection's events
/// nor its publishes; replies may therefore arrive out of order, matched by
/// their ids. `Ok(result)` becomes the reply's `result`, and an error its
/// `message` (with its causes), so an error must say what went wrong in
/// words a user can act on. A query must not change anything. Neither may
/// wait on a client: answer from storage or memory and return.
pub trait Handlers: Send + Sync + 'static {
    /// Answers `query` with `args`.
    fn query(
        &self,
        query: Query,
        args: Value,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;

    /// Makes the change `call` with `args` asks for and says what it did.
    fn call(&self, call: Call, args: Value) -> impl Future<Output = anyhow::Result<Value>> + Send;
}
