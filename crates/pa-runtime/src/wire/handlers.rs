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
