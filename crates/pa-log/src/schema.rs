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
    // 2: transcripts addressed by locator, byte for byte (the transcript
    // store's tables), and the store's own facts (its identity, import
    // markers). A line is stored with its newline; `start` is where it
    // starts in the transcript's bytes.
    r"
    CREATE TABLE store_meta (
        key   TEXT PRIMARY KEY,
        value NOT NULL
    );
    INSERT INTO store_meta (key, value) VALUES ('store_id', abs(random()));

    CREATE TABLE transcripts (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        locator     TEXT NOT NULL UNIQUE,
        dir         TEXT NOT NULL,
        session     TEXT,
        len         INTEGER NOT NULL,
        lines       INTEGER NOT NULL,
        version     INTEGER NOT NULL,
        created_ns  INTEGER NOT NULL,
        modified_ns INTEGER NOT NULL,
        source      TEXT
    );
    CREATE INDEX transcripts_dir ON transcripts (dir);
    CREATE INDEX transcripts_session ON transcripts (session) WHERE session IS NOT NULL;

    CREATE TABLE transcript_lines (
        transcript INTEGER NOT NULL REFERENCES transcripts (id) ON DELETE CASCADE,
        seq        INTEGER NOT NULL,
        start      INTEGER NOT NULL,
        kind       TEXT,
        entry_id   TEXT,
        parent_id  TEXT,
        bytes      BLOB NOT NULL,
        PRIMARY KEY (transcript, seq)
    );
    CREATE UNIQUE INDEX transcript_lines_start ON transcript_lines (transcript, start);
    CREATE INDEX transcript_lines_entry_id ON transcript_lines (transcript, entry_id)
        WHERE entry_id IS NOT NULL;
    ",
    // 3: the agent's state besides transcripts, by partition: the stores
    // that were files (schedules, heartbeats, the event bus, the harness
    // store, the reuse ledger, the crash log). A partition's rows count once
    // `partitions` holds it; deleting it deletes them.
    r"
    CREATE TABLE partitions (
        scope         TEXT PRIMARY KEY,
        imported_from TEXT,
        source_sha256 TEXT,
        held_since    INTEGER NOT NULL
    ) WITHOUT ROWID;

    CREATE TABLE sources (
        scope    TEXT NOT NULL REFERENCES partitions (scope) ON DELETE CASCADE,
        id       TEXT NOT NULL,
        kind     TEXT NOT NULL,
        position INTEGER NOT NULL,
        spec     TEXT NOT NULL,
        claim    TEXT,
        PRIMARY KEY (scope, id)
    ) WITHOUT ROWID;
    CREATE INDEX sources_kind ON sources (kind, scope);

    CREATE TABLE subscriptions (
        scope    TEXT NOT NULL REFERENCES partitions (scope) ON DELETE CASCADE,
        id       TEXT NOT NULL,
        source   TEXT NOT NULL,
        position INTEGER NOT NULL,
        spec     TEXT NOT NULL,
        PRIMARY KEY (scope, id)
    ) WITHOUT ROWID;
    CREATE INDEX subscriptions_source ON subscriptions (scope, source);

    CREATE TABLE kv (
        scope   TEXT NOT NULL REFERENCES partitions (scope) ON DELETE CASCADE,
        key     TEXT NOT NULL,
        value   TEXT NOT NULL,
        updated INTEGER NOT NULL,
        PRIMARY KEY (scope, key)
    ) WITHOUT ROWID;

    CREATE TABLE crashes (
        seq       INTEGER PRIMARY KEY AUTOINCREMENT,
        scope     TEXT NOT NULL REFERENCES partitions (scope) ON DELETE CASCADE,
        stored_at INTEGER NOT NULL,
        record    TEXT NOT NULL
    );
    CREATE INDEX crashes_scope ON crashes (scope, seq);
    ",
];

/// Bring `connection`'s database to the current schema.
///
/// # Errors
///
/// A migration failed, or the database was migrated by a newer build.
pub(crate) fn migrate(connection: &mut Connection) -> Result<()> {
    let applied: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read the schema version")?;
    let applied = usize::try_from(applied).context("a negative schema version")?;
    if applied > MIGRATIONS.len() {
        bail!(
            "the database is at schema version {applied}, newer than this build's {}: it was \
             written by a newer Prime Agent",
            MIGRATIONS.len()
        );
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
        let version = index + 1;
        let transaction = connection.transaction()?;
        transaction
            .execute_batch(migration)
            .with_context(|| format!("apply schema migration {version}"))?;
        transaction.pragma_update(None, "user_version", i64::try_from(version)?)?;
        transaction.commit()?;
    }
    Ok(())
}
