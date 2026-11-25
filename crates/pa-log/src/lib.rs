//! Prime Agent's SQLite store: the event log and the session transcripts, in
//! one database file.
//!
//! - One writer thread owns the read-write connection and commits submitted
//!   changes in groups ([`Receipt`] answers each). Nothing that paints waits
//!   on a receipt.
//! - Reads take a read-only connection from a small pool; in WAL mode a read
//!   never waits for the writer.
//! - [`Log::append_event`], [`Log::events`] and [`Log::prune_events`] are the
//!   event log ([`pa_types::event_log`]): an event gets its id, sequence
//!   number and time when it is stored, and expires after its retention.
//! - [`Log::import_session_file`], [`Log::export_session`],
//!   [`Log::append_entry`], [`Log::sessions`] and [`Log::entries`] keep
//!   session transcripts line for line, so a JSONL session file imported and
//!   exported again is the same bytes.
//! - [`Log::put_transcript`], [`Log::append_transcript`],
//!   [`Log::transcript_bytes`] and their siblings keep transcripts by
//!   locator, byte for byte, for the transcript store in `pa-core`: a read at
//!   any byte position goes straight to the line holding it.
//! - [`Log::sources`], [`Log::replace_sources`], [`Log::value`],
//!   [`Log::put_value`], [`Log::crashes`] and [`Log::append_crashes`] keep the
//!   agent's other state (schedules, the event bus, the harness store, the
//!   crash log) by partition, each written whole or imported once from the
//!   file it replaces ([`Origin`]).
//!
//! The database runs with `journal_mode=WAL` and `synchronous=NORMAL`: a power
//! loss can drop the last commits, but never leaves the file inconsistent.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

mod events;
mod partitions;
mod schema;
mod sessions;
mod transcripts;
mod writer;

pub use events::{EventRetention, DEFAULT_EVENT_RETENTION};
pub use partitions::{
    CrashRow, Origin, Partition, SourceKind, SourceRow, SourceSet, SubscriptionRow,
};
pub use sessions::{EntryRow, ImportOutcome, SessionRow};
pub use transcripts::{
    AppendMode, MoveOutcome, NewTranscript, PutMode, TranscriptChunk, TranscriptRow,
};
pub use writer::Receipt;

/// How long a connection waits on a lock another holds before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The store, open on one database file.
pub struct Log {
    path: PathBuf,
    writer: writer::Writer,
    readers: Mutex<Vec<Connection>>,
    retention: EventRetention,
}

impl Log {
    /// Open (creating it and its directory if needed) the database at `path`,
    /// bring it to the current schema, and start its writer.
    ///
    /// # Errors
    ///
    /// The file cannot be opened or created, or its schema cannot be migrated
    /// (including a database a newer build migrated further).
    pub fn open(path: impl Into<PathBuf>, retention: EventRetention) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut connection =
            Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        // The transcripts are the user's own: the database is private, and
        // SQLite gives its WAL and shared-memory files the same mode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("make {} private", path.display()))?;
        }
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // Switching a new database to WAL answers SQLITE_BUSY at once, without
        // the busy handler, while another process creates the same file (a
        // daemon and the CLI command that reads its database): retried until
        // the busy timeout.
        let deadline = Instant::now() + BUSY_TIMEOUT;
        loop {
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => break,
                Err(error)
                    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("set WAL on {}", path.display()))
                }
            }
        }
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        schema::migrate(&mut connection).with_context(|| format!("migrate {}", path.display()))?;
        Ok(Log {
            writer: writer::Writer::start(connection)?,
            readers: Mutex::new(Vec::new()),
            path,
            retention,
        })
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `read` on a read-only connection from the pool.
    fn read<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let pooled = self
            .readers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let connection = if let Some(connection) = pooled {
            connection
        } else {
            let connection = Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("open {} to read", self.path.display()))?;
            connection.busy_timeout(BUSY_TIMEOUT)?;
            connection
        };
        let result = read(&connection);
        self.readers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(connection);
        result
    }
}

/// Unix milliseconds now.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
