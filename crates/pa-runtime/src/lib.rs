//! Prime Agent's event-driven runtime.
//!
//! - [`Bus`]: publishes an event (stores it through an [`EventStore`], fans
//!   it out to [`Listener`]s, matches it against subscriptions) and runs the
//!   routes of the subscriptions it matches.
//! - Subscriptions: scope ([`SessionTree`] answers "is a subagent of"),
//!   `when` predicates, throttle and debounce, then the route: a
//!   [`StateStore`] write, a caused event, a [`ModelCaller`] call or a
//!   [`TaskSink`] task.
//! - [`TaskQueue`]: one serial lane per session, sessions in parallel under a
//!   global cap, run by a [`TaskRunner`] and kept in a [`TaskStore`].
//!
//! - [`WireServer`]: the wire2 endpoint clients connect to: subscriptions
//!   with gap-free replay, publishes of client input, and queries and calls
//!   answered by [`Handlers`].
//!
//! - [`LogEventStore`], [`LogTaskStore`], [`LogStateStore`]: the stores in
//!   the agent's SQLite database (`pa_log::Log`).
//!
//! - [`Runtime`]: all of it over the agent's database, with sessions that
//!   run each task as an agent turn on their own thread, over the engine a
//!   [`SessionBuilder`] builds; every session's `agent/<id>/input` queues
//!   its tasks, and each turn streams on `agent/<id>/stream` and ends with
//!   `agent/<id>/done`; a run is settled ([`Runtime::settle`], the
//!   [`Settled`] exit facts) and finished ([`Runtime::finish`]) on the
//!   session's thread too. The print and json modes run on it.
//! - Subagents: a session's `rlm-spawn` opens its child as a runtime session
//!   of its own ([`SubagentSpec`]), given its task through
//!   `agent/<child>/input`; the parent hears the child's settle through a
//!   subscription on `agent/<child>/done`, and a settle waits until every
//!   child is at rest.
//! - The headless turn driver: what runs around a turn (the turn boundary,
//!   the goal surface, the autonomous run, the continuation hook, the
//!   session slash commands), which the runtime's sessions run.
//!
//! - Sources: timers and folder watches ([`SourceDefinition`],
//!   `pa-sources`) defined on the runtime and kept in its database, each
//!   running while a subscription hears it.
//!
//! The `Memory*` implementations are for tests and headless use.

use std::sync::{Mutex, MutexGuard, PoisonError};

mod bus;
mod events;
mod log_stores;
mod routes;
mod runtime;
mod session;
mod sources;
mod store;
mod subagents;
mod subscriptions;
mod tasks;
mod trust;
mod turn;
mod wire;

pub use bus::{Bus, BusClosed, BusParts, Heard, Intake, Listener, Published};
pub use log_stores::{LogEventStore, LogStateStore, LogTaskStore};
pub use routes::{ModelCall, ModelCaller, TaskRequest, TaskSink};
pub use runtime::{Runtime, RuntimeOptions};
pub use session::{
    BuiltSession, DriverEvents, Finished, SessionBuilder, SessionSeams, SessionSpec, SettleAfter,
    Settled, SubagentSpec,
};
pub use sources::SourceDefinition;
pub use store::{EventStore, MemoryEventStore, MemoryStateStore, StateKey, StateStore, StateWrite};
pub use subscriptions::{DuplicateSubscription, MemorySessionTree, SessionTree};
pub use tasks::{
    MemoryTaskStore, Steered, TaskOutcome, TaskQueue, TaskRunner, TaskStore, INTERRUPTED_BY_RESTART,
};
pub use wire::{
    Call, Handlers, Query, UnknownOperation, WireConfig, WireServer, CLIENT_TOPICS, IN_FLIGHT,
    MAX_TAIL,
};

/// Locks `mutex`, taking the value even when a panicking holder poisoned it:
/// every lock here is shared between sessions, and one session's panic must
/// not stop the others.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
