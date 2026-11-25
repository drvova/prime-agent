//! The database's schema, as numbered migrations applied in order at open.
//!
//! `PRAGMA user_version` records how many have run. A new table or column is
//! a new migration appended here, never an edit of an applied one; a database
//! a newer build migrated further is refused rather than written by code that
//! does not know its shape.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// The migrations, in order: migration `n` (1-based) brings `user_version`
/// from `n - 1` to `n`.
const MIGRATIONS: &[&str] = &[
    // 1: the event log, and session transcripts kept line for line.
    r"
    CREATE TABLE events (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        id         TEXT NOT NULL UNIQUE,
        ts         INTEGER NOT NULL,
        topic      TEXT NOT NULL,
        source     TEXT NOT NULL,
        session    TEXT,
        trust      TEXT NOT NULL,
        cause      TEXT,
        reply      TEXT,
        payload    TEXT NOT NULL,
        expires_at INTEGER
    );
    CREATE INDEX events_topic ON events (topic, seq);
    CREATE INDEX events_session ON events (session, seq);
    CREATE INDEX events_expiry ON events (expires_at) WHERE expires_at IS NOT NULL;

    CREATE TABLE sessions (
        id               TEXT PRIMARY KEY,
        cwd              TEXT,
        created          TEXT,
        updated          INTEGER NOT NULL,
        trailing_newline INTEGER NOT NULL,
        imported_from    TEXT,
        source_sha256    TEXT
    );
    CREATE UNIQUE INDEX sessions_imported_from ON sessions (imported_from)
        WHERE imported_from IS NOT NULL;

    CREATE TABLE entries (
        session   TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
        seq       INTEGER NOT NULL,
        entry_id  TEXT,
        parent_id TEXT,
        kind      TEXT,
        line      TEXT NOT NULL,
        PRIMARY KEY (session, seq)
    ) WITHOUT ROWID;
    CREATE INDEX entries_entry_id ON entries (session, entry_id) WHERE entry_id IS NOT NULL;
    ",
];

/// Bring `connection`'s database to the current schema.
///
/// # Errors
///
/// A migration failed, or the database was migrated by a newer build.
pub(crate) fn migrate(connection: &mut Connection) -> Result<()> {
    loop {
        // The version is read under the write lock (`BEGIN IMMEDIATE`), so of
        // two processes opening one new database, only one applies a
        // migration and the other reads it applied.
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let applied: i64 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read the schema version")?;
        let applied = usize::try_from(applied).context("a negative schema version")?;
        if applied > MIGRATIONS.len() {
            bail!(
                "the database is at schema version {applied}, newer than this build's {}: it \
                 was written by a newer Prime Agent",
                MIGRATIONS.len()
            );
        }
        let Some(migration) = MIGRATIONS.get(applied) else {
            return Ok(());
        };
        let version = applied + 1;
        transaction
            .execute_batch(migration)
            .with_context(|| format!("apply schema migration {version}"))?;
        transaction.pragma_update(None, "user_version", i64::try_from(version)?)?;
        transaction.commit()?;
    }
}
