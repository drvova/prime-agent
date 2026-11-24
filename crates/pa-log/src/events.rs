//! The event log: appending, reading by topic filter, and expiry.

use std::time::Duration;

use anyhow::{Context, Result};
use pa_types::event_log::{Cause, Event, EventId, NewEvent, Topic, TopicFilter, Trust};
use pa_types::wire2::Since;
use rusqlite::{params, Row};

use crate::{Log, Receipt};

/// How long stored events are kept by default: 30 days.
pub const DEFAULT_EVENT_RETENTION: EventRetention = EventRetention {
    keep: Some(Duration::from_hours(24 * 30)),
};

/// How long the log keeps an event before [`Log::prune_events`] drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRetention {
    /// `None` keeps events until something deletes them.
    pub keep: Option<Duration>,
}

/// Rows read per query while filtering by topic in Rust.
const SCAN_BATCH: i64 = 512;

fn text<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode(row: &Row<'_>) -> rusqlite::Result<Result<Event>> {
    let seq: i64 = row.get("seq")?;
    let ts: i64 = row.get("ts")?;
    let id: String = row.get("id")?;
    let topic: String = row.get("topic")?;
    let source: String = row.get("source")?;
    let session: Option<String> = row.get("session")?;
    let trust: String = row.get("trust")?;
    let cause: Option<String> = row.get("cause")?;
    let reply: Option<String> = row.get("reply")?;
    let payload: String = row.get("payload")?;
    Ok((|| -> Result<Event> {
        Ok(Event {
            id: EventId(id),
            seq: u64::try_from(seq)?,
            ts: u64::try_from(ts)?,
            event: NewEvent {
                topic: Topic::new(topic)?,
                source,
                session,
                payload: serde_json::from_str(&payload)?,
                trust: serde_json::from_value::<Trust>(serde_json::Value::String(trust))?,
                cause: cause
                    .map(|cause| serde_json::from_str::<Cause>(&cause))
                    .transpose()?,
                reply: reply.map(Topic::new).transpose()?,
            },
        })
    })())
}

impl Log {
    /// Store `event`, giving it its id, sequence number and time; the stored
    /// event once committed.
    pub fn append_event(&self, event: NewEvent) -> Receipt<Event> {
        let expires = self
            .retention
            .keep
            .map(|keep| u64::try_from(keep.as_millis()).unwrap_or(u64::MAX));
        self.writer.submit(move |connection| {
            let ts = crate::now_ms();
            let id = EventId(format!("evt-{}", uuid::Uuid::now_v7()));
            connection
                .prepare_cached(
                    "INSERT INTO events (id, ts, topic, source, session, trust, cause, reply, \
                     payload, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )?
                .execute(params![
                    id.0,
                    i64::try_from(ts)?,
                    event.topic.as_str(),
                    event.source,
                    event.session,
                    serde_json::to_value(event.trust)?.as_str(),
                    event.cause.as_ref().map(text).transpose()?,
                    event.reply.as_ref().map(Topic::as_str),
                    text(&event.payload)?,
                    expires
                        .map(|keep| i64::try_from(ts.saturating_add(keep)))
                        .transpose()?,
                ])
                .context("store the event")?;
            Ok(Event {
                id,
                seq: u64::try_from(connection.last_insert_rowid())?,
                ts,
                event,
            })
        })
    }

    /// Stored events matching `filter`, oldest first: those after a sequence
    /// number, or the last `count`; at most `limit`.
    ///
    /// # Errors
    ///
    /// The database could not be read, or a stored row does not decode.
    pub fn events(&self, filter: &TopicFilter, since: Since, limit: usize) -> Result<Vec<Event>> {
        // The filter's literal levels before its first wildcard (`agent/3f2a`
        // of `agent/3f2a/+`) narrow the scan in SQL; the filter itself decides.
        let levels: Vec<&str> = filter
            .as_str()
            .split('/')
            .take_while(|level| *level != "+" && *level != "#")
            .collect();
        let prefix = (!levels.is_empty()).then(|| levels.join("/"));
        let like = prefix.as_ref().map(|prefix| {
            format!(
                "{}/%",
                prefix
                    .replace('\\', r"\\")
                    .replace('%', r"\%")
                    .replace('_', r"\_")
            )
        });
        self.read(|connection| {
            let (mut cursor, wanted, newest_first) = match since {
                Since::Now => return Ok(Vec::new()),
                Since::After { seq } => (i64::try_from(seq)?, limit, false),
                Since::Tail { count } => (
                    i64::MAX,
                    limit.min(usize::try_from(count).unwrap_or(usize::MAX)),
                    true,
                ),
            };
            let (bound, order) = if newest_first {
                ("seq < ?1", "DESC")
            } else {
                ("seq > ?1", "ASC")
            };
            let topic = if prefix.is_some() {
                r" AND (topic = ?2 OR topic LIKE ?3 ESCAPE '\')"
            } else {
                ""
            };
            let sql = format!(
                "SELECT seq, ts, id, topic, source, session, trust, cause, reply, payload \
                 FROM events WHERE {bound}{topic} ORDER BY seq {order} LIMIT {SCAN_BATCH}"
            );
            let mut statement = connection.prepare_cached(&sql)?;
            let mut found = Vec::new();
            while found.len() < wanted {
                let batch = match (&prefix, &like) {
                    (Some(prefix), Some(like)) => statement
                        .query_map(params![cursor, prefix, like], decode)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                    _ => statement
                        .query_map(params![cursor], decode)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                };
                let batch = batch.into_iter().collect::<Result<Vec<Event>>>()?;
                let Some(last) = batch.last() else {
                    break;
                };
                cursor = i64::try_from(last.seq)?;
                found.extend(
                    batch
                        .into_iter()
                        .filter(|event| filter.matches(&event.event.topic)),
                );
            }
            found.truncate(wanted);
            if newest_first {
                found.reverse();
            }
            Ok(found)
        })
    }

    /// Delete every event whose retention ended by `now_ms`; how many went.
    pub fn prune_events(&self, now_ms: u64) -> Receipt<usize> {
        self.writer.submit(move |connection| {
            Ok(connection.execute(
                "DELETE FROM events WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                params![i64::try_from(now_ms)?],
            )?)
        })
    }
}
