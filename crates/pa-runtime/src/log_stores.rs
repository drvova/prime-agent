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
//!
//! The stores keep nothing of an [`Unstored`] session (an ephemeral session,
//! `--no-session`, and the subagents it spawns): its events are fanned out
//! and run their routes, but are not appended (each has `seq` 0, as a
//! live-only event has, and an id of its own); its tasks are not saved; its
//! state is not written, nor any state a route writes for one of its events.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use pa_log::{Log, Origin};
use pa_types::event_log::{Event, EventId, NewEvent, Task};
use serde_json::{json, Value};

use crate::lock;
use crate::store::{EventStore, StateKey, StateStore, StateWrite};
use crate::tasks::TaskStore;

/// How the id of an event that is not in the log starts.
const UNSTORED_ID: &str = "unstored-";

/// The sessions the stores keep nothing of; clones share them. A session
/// joins when it is opened (an ephemeral one, or a subagent of one) and
/// stays for the life of the runtime, so nothing published for it after it
/// closed is stored either.
#[derive(Debug, Clone, Default)]
pub(crate) struct Unstored {
    sessions: Arc<Mutex<HashSet<String>>>,
}

impl Unstored {
    pub(crate) fn insert(&self, session: &str) {
        lock(&self.sessions).insert(session.to_string());
    }

    pub(crate) fn contains(&self, session: &str) -> bool {
        lock(&self.sessions).contains(session)
    }

    /// Whether `event` is an unstored session's: published in it, or on
    /// its `agent/<id>/...` or `session/<id>/...` topics.
    fn holds(&self, event: &NewEvent) -> bool {
        let mut levels = event.topic.levels();
        let topic_session = match (levels.next(), levels.next()) {
            (Some("agent" | "session"), Some(session)) => Some(session),
            _ => None,
        };
        let sessions = lock(&self.sessions);
        event
            .session
            .as_deref()
            .into_iter()
            .chain(topic_session)
            .any(|session| sessions.contains(session))
    }
}

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
    unstored: Unstored,
}

impl LogEventStore {
    /// The event log in `log`.
    #[must_use]
    pub fn new(log: Arc<Log>) -> Self {
        LogEventStore {
            log,
            unstored: Unstored::default(),
        }
    }

    /// The event log in `log`, which stores no event of `unstored`'s
    /// sessions.
    pub(crate) fn sparing(log: Arc<Log>, unstored: Unstored) -> Self {
        LogEventStore { log, unstored }
    }
}

impl EventStore for LogEventStore {
    fn append(&self, event: NewEvent) -> impl Future<Output = anyhow::Result<Event>> + Send {
        let appended = if self.unstored.holds(&event) {
            Err(event)
        } else {
            Ok(self.log.append_event(event))
        };
        async move {
            match appended {
                Ok(receipt) => receipt.wait().await,
                // Not in the log: no sequence number, and an id of its own.
                Err(event) => Ok(Event {
                    id: EventId(format!("{UNSTORED_ID}{}", uuid::Uuid::now_v7())),
                    seq: 0,
                    ts: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |since| {
                            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                        }),
                    event,
                }),
            }
        }
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
    unstored: Unstored,
}

impl LogTaskStore {
    /// The task lanes in `log`.
    #[must_use]
    pub fn new(log: Arc<Log>) -> Self {
        LogTaskStore {
            log,
            unstored: Unstored::default(),
        }
    }

    /// The task lanes in `log`, which saves no task of `unstored`'s
    /// sessions.
    pub(crate) fn sparing(log: Arc<Log>, unstored: Unstored) -> Self {
        LogTaskStore { log, unstored }
    }
}

impl TaskStore for LogTaskStore {
    fn save(&self, task: Task) -> impl Future<Output = anyhow::Result<()>> + Send {
        let saved = (!self.unstored.contains(&task.session)).then(|| self.log.save_task(task));
        async move {
            match saved {
                Some(receipt) => receipt.wait().await,
                None => Ok(()),
            }
        }
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
    unstored: Unstored,
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
        LogStateStore {
            log,
            unstored: Unstored::default(),
        }
    }

    /// The state in `log`, which writes none of `unstored`'s sessions' keys.
    pub(crate) fn sparing(log: Arc<Log>, unstored: Unstored) -> Self {
        LogStateStore { log, unstored }
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
        let unstored = write.cause.0.starts_with(UNSTORED_ID)
            || write
                .key
                .session
                .as_deref()
                .is_some_and(|session| self.unstored.contains(session));
        let receipt = (!unstored).then(|| {
            let stored = json!({
                "value": write.value,
                "trust": write.trust,
                "cause": write.cause,
            })
            .to_string();
            self.log.put_value(
                &partition(&write.key),
                &write.key.key,
                stored,
                Origin::Written,
            )
        });
        async move {
            // A written value is always stored: `false` is only an import's.
            if let Some(receipt) = receipt {
                receipt.wait().await?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
