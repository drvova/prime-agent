//! Prime Agent's event-driven runtime, storage-independent core.
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
//! Storage, the agent turn, the kernel and the wire come later, behind the
//! traits defined here. The `Memory*` implementations are for tests and
//! headless use.

use std::sync::{Mutex, MutexGuard, PoisonError};

mod bus;
mod routes;
mod store;
mod subscriptions;
mod tasks;

pub use bus::{Bus, BusClosed, BusParts, Heard, Intake, Listener, Published};
pub use routes::{ModelCall, ModelCaller, TaskRequest, TaskSink};
pub use store::{EventStore, MemoryEventStore, MemoryStateStore, StateKey, StateStore, StateWrite};
pub use subscriptions::{DuplicateSubscription, MemorySessionTree, SessionTree};
pub use tasks::{
    MemoryTaskStore, Steered, TaskOutcome, TaskQueue, TaskRunner, TaskStore, INTERRUPTED_BY_RESTART,
};

/// Locks `mutex`, taking the value even when a panicking holder poisoned it:
/// every lock here is shared between sessions, and one session's panic must
/// not stop the others.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
