//! The agent's state besides transcripts (schema migration 3): timer and bus
//! sources, bus subscriptions, key-value documents and crash records, each
//! kept in a partition.
//!
//! A partition is named by a scope the caller chooses (pa-core names each one
//! for the file it replaces, such as `cron:<path of scheduled-jobs.json>`).
//! The database holds a partition once it records it in `partitions`: from
//! then on its rows are the truth, and the file it came from is never read
//! again. A partition comes in one of two ways ([`Origin`]):
//!
//! - **Written** here: the caller stores the partition's whole state (or
//!   appends a crash), and the partition is held from then on.
//! - **Imported** from a file: stored only when the partition is not held
//!   yet, so a second import, or one racing a write, changes nothing. The
//!   partition records the file and the SHA-256 of its bytes.
//!
//! Every write is one writer job, so a partition's rows and its record
//! change in one transaction: a partition is never half-written or
//! half-imported.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{now_ms, Log, Receipt};

/// How a write brings a partition in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Written by this build: always stored, and the partition is held.
    Written,
    /// Imported from the file `from` whose bytes hash to `sha256`: stored
    /// only when the partition is not held yet.
    Imported { from: String, sha256: String },
}

/// A partition the database holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub scope: String,
    /// The file it was imported from, when it was imported.
    pub imported_from: Option<String>,
    /// The SHA-256 (hex) of that file's bytes.
    pub source_sha256: Option<String>,
    /// Unix milliseconds of its first write or import.
    pub held_since: u64,
}

/// What a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A schedule or heartbeat: fires on its own clock.
    Timer,
    /// A named event-bus source sessions publish to.
    Bus,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::Timer => "timer",
            SourceKind::Bus => "bus",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "timer" => Some(SourceKind::Timer),
            "bus" => Some(SourceKind::Bus),
            _ => None,
        }
    }
}

/// One source in a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    /// Unique within the partition.
    pub id: String,
    pub kind: SourceKind,
    /// What the source is, as the caller serializes it (JSON).
    pub spec: String,
    /// The work it has in flight, when it has some (JSON): a timer's claimed
    /// run.
    pub claim: Option<String>,
}

/// One subscription in a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRow {
    /// Unique within the partition.
    pub id: String,
    /// The source it listens to (a source id of the partition, or a name the
    /// caller resolves).
    pub source: String,
    /// What the subscription is, as the caller serializes it (JSON).
    pub spec: String,
}

/// A partition's sources and subscriptions, each in its stored order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceSet {
    pub sources: Vec<SourceRow>,
    pub subscriptions: Vec<SubscriptionRow>,
}

/// One stored crash record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRow {
    /// Increases with every record, across partitions.
    pub seq: i64,
    /// Unix milliseconds of when it was stored.
    pub stored_at: u64,
    /// The record, as the caller serialized it (one JSON object).
    pub record: String,
}

fn unsigned(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            error.into(),
        )
    })
}

fn partition_row(row: &Row<'_>) -> rusqlite::Result<Partition> {
    Ok(Partition {
        scope: row.get(0)?,
        imported_from: row.get(1)?,
        source_sha256: row.get(2)?,
        held_since: unsigned(row, 3)?,
    })
}

/// Record `scope` as held for a write from `origin`; whether the write goes
/// ahead (an import of a partition already held does not).
fn hold(connection: &Connection, scope: &str, origin: &Origin) -> Result<bool> {
    let now = i64::try_from(now_ms())?;
    match origin {
        Origin::Written => {
            connection
                .prepare_cached(
                    "INSERT INTO partitions (scope, imported_from, source_sha256, held_since) \
                     VALUES (?1, NULL, NULL, ?2) ON CONFLICT (scope) DO NOTHING",
                )?
                .execute(params![scope, now])?;
            Ok(true)
        }
        Origin::Imported { from, sha256 } => {
            let inserted = connection
                .prepare_cached(
                    "INSERT INTO partitions (scope, imported_from, source_sha256, held_since) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (scope) DO NOTHING",
                )?
                .execute(params![scope, from, sha256, now])?;
            Ok(inserted == 1)
        }
    }
}

impl Log {
    /// The partition `scope`, when the database holds it.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn partition(&self, scope: &str) -> Result<Option<Partition>> {
        let scope = scope.to_string();
        self.read(move |connection| {
            Ok(connection
                .prepare_cached(
                    "SELECT scope, imported_from, source_sha256, held_since FROM partitions \
                     WHERE scope = ?1",
                )?
                .query_row(params![scope], partition_row)
                .optional()?)
        })
    }

    /// Every held partition whose scope starts with `prefix`, by scope.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn partitions(&self, prefix: &str) -> Result<Vec<Partition>> {
        let prefix = prefix.to_string();
        self.read(move |connection| {
            let length = i64::try_from(prefix.chars().count())?;
            let rows = connection
                .prepare_cached(
                    "SELECT scope, imported_from, source_sha256, held_since FROM partitions \
                     WHERE substr(scope, 1, ?2) = ?1 ORDER BY scope",
                )?
                .query_map(params![prefix, length], partition_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The sources and subscriptions of partition `scope`, in their stored
    /// order (empty when it holds none or is not held).
    ///
    /// # Errors
    ///
    /// The database could not be read, or a row names a kind this build does
    /// not know.
    pub fn sources(&self, scope: &str) -> Result<SourceSet> {
        let scope = scope.to_string();
        self.read(move |connection| {
            let mut sources = Vec::new();
            let mut statement = connection.prepare_cached(
                "SELECT id, kind, spec, claim FROM sources WHERE scope = ?1 ORDER BY position",
            )?;
            let mut rows = statement.query(params![scope])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(1)?;
                let kind = SourceKind::parse(&kind)
                    .ok_or_else(|| anyhow::anyhow!("source kind {kind:?} is not known"))?;
                sources.push(SourceRow {
                    id: row.get(0)?,
                    kind,
                    spec: row.get(2)?,
                    claim: row.get(3)?,
                });
            }
            let subscriptions = connection
                .prepare_cached(
                    "SELECT id, source, spec FROM subscriptions WHERE scope = ?1 \
                     ORDER BY position",
                )?
                .query_map(params![scope], |row| {
                    Ok(SubscriptionRow {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        spec: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(SourceSet {
                sources,
                subscriptions,
            })
        })
    }

    /// Replace partition `scope`'s sources and subscriptions with `set`, kept
    /// in its order; answers whether it was stored (an import of a held
    /// partition is not).
    ///
    /// # Errors
    ///
    /// The receipt fails when two rows of one kind share an id, or the write
    /// did not commit; nothing of `set` is stored then.
    pub fn replace_sources(&self, scope: &str, set: SourceSet, origin: Origin) -> Receipt<bool> {
        let scope = scope.to_string();
        self.writer.submit(move |connection| {
            if !hold(connection, &scope, &origin)? {
                return Ok(false);
            }
            connection
                .prepare_cached("DELETE FROM sources WHERE scope = ?1")?
                .execute(params![scope])?;
            connection
                .prepare_cached("DELETE FROM subscriptions WHERE scope = ?1")?
                .execute(params![scope])?;
            let mut insert = connection.prepare_cached(
                "INSERT INTO sources (scope, id, kind, position, spec, claim) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (position, source) in set.sources.iter().enumerate() {
                insert.execute(params![
                    scope,
                    source.id,
                    source.kind.as_str(),
                    i64::try_from(position)?,
                    source.spec,
                    source.claim
                ])?;
            }
            let mut insert = connection.prepare_cached(
                "INSERT INTO subscriptions (scope, id, source, position, spec) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (position, subscription) in set.subscriptions.iter().enumerate() {
                insert.execute(params![
                    scope,
                    subscription.id,
                    subscription.source,
                    i64::try_from(position)?,
                    subscription.spec
                ])?;
            }
            Ok(true)
        })
    }

    /// The value at `key` in partition `scope`.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn value(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let (scope, key) = (scope.to_string(), key.to_string());
        self.read(move |connection| {
            Ok(connection
                .prepare_cached("SELECT value FROM kv WHERE scope = ?1 AND key = ?2")?
                .query_row(params![scope, key], |row| row.get(0))
                .optional()?)
        })
    }

    /// Set the value at `key` in partition `scope`; answers whether it was
    /// stored (an import of a held partition is not).
    pub fn put_value(
        &self,
        scope: &str,
        key: &str,
        value: String,
        origin: Origin,
    ) -> Receipt<bool> {
        let (scope, key) = (scope.to_string(), key.to_string());
        self.writer.submit(move |connection| {
            if !hold(connection, &scope, &origin)? {
                return Ok(false);
            }
            connection
                .prepare_cached(
                    "INSERT INTO kv (scope, key, value, updated) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (scope, key) DO UPDATE SET value = excluded.value, \
                     updated = excluded.updated",
                )?
                .execute(params![scope, key, value, i64::try_from(now_ms())?])?;
            Ok(true)
        })
    }

    /// Partition `scope`'s crash records, oldest first.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn crashes(&self, scope: &str) -> Result<Vec<CrashRow>> {
        let scope = scope.to_string();
        self.read(move |connection| {
            let rows = connection
                .prepare_cached(
                    "SELECT seq, stored_at, record FROM crashes WHERE scope = ?1 ORDER BY seq",
                )?
                .query_map(params![scope], |row| {
                    Ok(CrashRow {
                        seq: row.get(0)?,
                        stored_at: unsigned(row, 1)?,
                        record: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Append `records` to partition `scope`'s crash records, in order;
    /// answers whether they were stored (an import of a held partition is
    /// not). Records are never rewritten.
    pub fn append_crashes(
        &self,
        scope: &str,
        records: Vec<String>,
        origin: Origin,
    ) -> Receipt<bool> {
        let scope = scope.to_string();
        self.writer.submit(move |connection| {
            if !hold(connection, &scope, &origin)? {
                return Ok(false);
            }
            let now = i64::try_from(now_ms())?;
            let mut insert = connection.prepare_cached(
                "INSERT INTO crashes (scope, stored_at, record) VALUES (?1, ?2, ?3)",
            )?;
            for record in &records {
                insert.execute(params![scope, now, record])?;
            }
            Ok(true)
        })
    }
}
