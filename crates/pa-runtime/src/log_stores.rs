//! The runtime's stores over one shared `pa_log::Log`: the event log, the
//! task lanes and the state `Set` routes write, all in the agent's
//! database.
//!
//! A write is submitted to the log's writer thread and awaited through its
//! `Receipt`, so it never blocks the runtime's thread. A read runs SQLite on
//! the calling thread, so it goes to Tokio's blocking pool.
//!
//! State lives in `pa-log`'s `kv` table: a session's keys in the partition
//! `state/<session>`, global keys in `state`, each under the key itself.
//! No other `kv` partition starts with `state` (pa-core's are named
//! `cron:`, `harness:`, `reuse:`, `event-bus:` and `crashes:`), so the two
//! never meet. The value is the JSON `{"value", "trust", "cause"}`.

use std::future::Future;
use std::sync::Arc;

use anyhow::Context;
use pa_log::{Log, Origin};
use pa_types::event_log::{Event, NewEvent, Task};
use serde_json::{json, Value};

use crate::store::{EventStore, StateKey, StateStore, StateWrite};
use crate::tasks::TaskStore;

/// Runs `read` against `log` on Tokio's blocking pool.
async fn read_blocking<T: Send + 'static>(
    log: &Arc<Log>,
    read: impl FnOnce(&Log) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let log = Arc::clone(log);
    tokio::task::spawn_blocking(move || read(&log))
        .await
        .context("the database read did not finish")?
}

/// The event log in the agent's database (`Log::append_event`,
/// `events_since`, `events_before`).
#[derive(Clone)]
pub struct LogEventStore {
    log: Arc<Log>,
}

impl LogEventStore {
    /// The event log in `log`.
    #[must_use]
    pub fn new(log: Arc<Log>) -> Self {
        LogEventStore { log }
    }
}

impl EventStore for LogEventStore {
    fn append(&self, event: NewEvent) -> impl Future<Output = anyhow::Result<Event>> + Send {
        self.log.append_event(event).wait()
    }

    fn since(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send {
        read_blocking(&self.log, move |log| log.events_since(seq, limit))
    }

    fn before(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send {
        read_blocking(&self.log, move |log| log.events_before(seq, limit))
    }
}

/// The task lanes in the agent's database (`Log::save_task`,
/// `unfinished_tasks`), so queued work survives a restart.
#[derive(Clone)]
pub struct LogTaskStore {
    log: Arc<Log>,
}

impl LogTaskStore {
    /// The task lanes in `log`.
    #[must_use]
    pub fn new(log: Arc<Log>) -> Self {
        LogTaskStore { log }
    }
}

impl TaskStore for LogTaskStore {
    fn save(&self, task: Task) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.log.save_task(task).wait()
    }

    fn unfinished(&self, session: &str) -> impl Future<Output = anyhow::Result<Vec<Task>>> + Send {
        let session = session.to_string();
        read_blocking(&self.log, move |log| log.unfinished_tasks(&session))
    }
}

/// The runtime's state in the agent's database, on the `kv` table (see the
/// module documentation for its keys).
#[derive(Clone)]
pub struct LogStateStore {
    log: Arc<Log>,
}

/// The `kv` partition that holds `key`.
fn partition(key: &StateKey) -> String {
    match &key.session {
        Some(session) => format!("state/{session}"),
        None => "state".to_string(),
    }
}

impl LogStateStore {
    /// The state in `log`.
    #[must_use]
    pub fn new(log: Arc<Log>) -> Self {
        LogStateStore { log }
    }

    /// The last write under `key`.
    ///
    /// # Errors
    ///
    /// The database could not be read, or the stored value does not decode.
    pub async fn get(&self, key: &StateKey) -> anyhow::Result<Option<StateWrite>> {
        let wanted = key.clone();
        let stored = read_blocking(&self.log, move |log| {
            log.value(&partition(&wanted), &wanted.key)
        })
        .await?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let mut stored: Value = serde_json::from_str(&stored)
            .with_context(|| format!("decode the state under {key:?}"))?;
        Ok(Some(StateWrite {
            key: key.clone(),
            value: stored["value"].take(),
            trust: serde_json::from_value(stored["trust"].take())
                .with_context(|| format!("decode the trust of the state under {key:?}"))?,
            cause: serde_json::from_value(stored["cause"].take())
                .with_context(|| format!("decode the cause of the state under {key:?}"))?,
        }))
    }
}

impl StateStore for LogStateStore {
    fn set(&self, write: StateWrite) -> impl Future<Output = anyhow::Result<()>> + Send {
        let stored = json!({
            "value": write.value,
            "trust": write.trust,
            "cause": write.cause,
        })
        .to_string();
        let receipt = self.log.put_value(
            &partition(&write.key),
            &write.key.key,
            stored,
            Origin::Written,
        );
        async move {
            // A written value is always stored: `false` is only an import's.
            receipt.wait().await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
