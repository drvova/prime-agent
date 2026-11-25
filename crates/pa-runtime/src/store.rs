//! Where the bus keeps the events it publishes and the state its `Set`
//! routes write.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pa_types::event_log::{Event, EventId, NewEvent, Trust};
use serde_json::Value;

use crate::lock;

/// The event log the bus appends to.
///
/// `append` gives the event the next sequence number (the first is 1, each
/// one more than the last), an id unique in the log and the time in Unix
/// milliseconds, stores it, and returns it as stored. The bus never runs two
/// appends at once and fans an event out only after its append returned, so
/// listeners see events in sequence order and a listener that lagged catches
/// up with `since`. An append that fails stores nothing; the bus then
/// reports the error to the publisher and runs no route.
///
/// The SQLite implementation wraps `pa_log::Log::append_event`.
pub trait EventStore: Send + Sync + 'static {
    /// Stores `event` and returns it with its id, sequence number and time.
    fn append(&self, event: NewEvent) -> impl Future<Output = anyhow::Result<Event>> + Send;

    /// At most `limit` stored events whose sequence number is above `seq`,
    /// oldest first.
    fn since(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send;
}

/// An event log in memory, for tests and headless runs. Clones share the
/// same log.
#[derive(Debug, Clone, Default)]
pub struct MemoryEventStore {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventStore for MemoryEventStore {
    fn append(&self, event: NewEvent) -> impl Future<Output = anyhow::Result<Event>> + Send {
        let mut events = lock(&self.events);
        let seq = events.last().map_or(1, |last| last.seq + 1);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let stored = Event {
            id: EventId(format!("{seq:020}")),
            seq,
            ts,
            event,
        };
        events.push(stored.clone());
        std::future::ready(Ok(stored))
    }

    fn since(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send {
        let events = lock(&self.events);
        let start = events.partition_point(|event| event.seq <= seq);
        std::future::ready(Ok(events[start..].iter().take(limit).cloned().collect()))
    }
}

/// A key of the runtime's state: a session's own (`Local` and `Agent`
/// subscriptions write their session's) or global (`session` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateKey {
    pub session: Option<String>,
    pub key: String,
}

/// A value a `Set` route writes: the matching event's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct StateWrite {
    pub key: StateKey,
    pub value: Value,
    /// The lower of the subscription's and the event's trust: a reader that
    /// acts on the value acts with at most this trust.
    pub trust: Trust,
    /// The event whose payload this is.
    pub cause: EventId,
}

/// The runtime's key-value state, which `Set` routes write.
///
/// `set` replaces the value under the write's key and returns once it is
/// stored; a later write to the same key wins. It is awaited while the bus
/// runs routes, so it must not wait on anything but its own storage. An
/// error is reported as a `system/route_failed` event.
pub trait StateStore: Send + Sync + 'static {
    /// Stores `write.value` under `write.key`.
    fn set(&self, write: StateWrite) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// State in memory, for tests and headless runs. Clones share the same
/// state.
#[derive(Debug, Clone, Default)]
pub struct MemoryStateStore {
    values: Arc<Mutex<HashMap<StateKey, StateWrite>>>,
}

impl MemoryStateStore {
    /// The last write under `key`.
    #[must_use]
    pub fn get(&self, key: &StateKey) -> Option<StateWrite> {
        lock(&self.values).get(key).cloned()
    }
}

impl StateStore for MemoryStateStore {
    fn set(&self, write: StateWrite) -> impl Future<Output = anyhow::Result<()>> + Send {
        lock(&self.values).insert(write.key.clone(), write);
        std::future::ready(Ok(()))
    }
}
