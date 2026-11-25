//! The agent's state besides transcripts, kept in the transcript store's
//! database (`agent.db`, [`pa_log`]'s partitions) instead of the files that
//! held it:
//!
//! | File | Partition rows |
//! | --- | --- |
//! | `scheduled-jobs.json` (schedules, heartbeats) | `sources`, timer kind, a claimed run on its job |
//! | `harness/harness_state.json` | `kv` |
//! | `reuse/ledger.json` | `kv` |
//! | `event-bus.json` | `sources`, bus kind, and `subscriptions` |
//! | `session-crashes.jsonl` | `crashes` |
//!
//! Every store keeps addressing its state by the path of its file, so its
//! callers and their semantics are unchanged; a partition is named by the
//! file's kind and its path, spelled one way for every process. Until the
//! database holds a partition, the file is read as it is (a store from
//! before the move works at once); the first write stores the whole state
//! in the database, and the file is never read or written again.
//! [`import_agent_state`] brings the files in off the launch path.
//!
//! With the file transcript store (`"transcriptStore": "file"`) there is no
//! database, and every store keeps its file as before.

use std::io;
use std::path::{Path, PathBuf};

use pa_log::{Log, Origin};

mod crash_log;
mod event_bus;
mod import;

pub use crash_log::{append_crash_record, crash_records};
pub use event_bus::{held_event_bus, store_event_bus, EventBusState};
pub(crate) use import::import_state_file;
pub use import::{import_agent_state, StateImportReport};

/// The key a document is kept under in its `kv` partition.
const DOCUMENT_KEY: &str = "document";

/// A file whose state the database keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFile {
    /// A session's (or a store's) `scheduled-jobs.json`.
    ScheduledJobs,
    /// A `harness/harness_state.json`, global or a session's.
    HarnessState,
    /// The agent dir's `reuse/ledger.json`.
    ReuseLedger,
    /// The daemon's `event-bus.json`.
    EventBus,
    /// The daemon's `session-crashes.jsonl`.
    CrashLog,
}

impl StateFile {
    /// Every kind.
    const ALL: [StateFile; 5] = [
        StateFile::ScheduledJobs,
        StateFile::HarnessState,
        StateFile::ReuseLedger,
        StateFile::EventBus,
        StateFile::CrashLog,
    ];

    /// What its partitions' scopes start with.
    fn prefix(self) -> &'static str {
        match self {
            StateFile::ScheduledJobs => "cron:",
            StateFile::HarnessState => "harness:",
            StateFile::ReuseLedger => "reuse:",
            StateFile::EventBus => "event-bus:",
            StateFile::CrashLog => "crashes:",
        }
    }

    /// The partition that keeps the file at `path`.
    pub(crate) fn scope(self, path: &Path) -> String {
        format!(
            "{}{}",
            self.prefix(),
            crate::session::store::spelled(path).to_string_lossy()
        )
    }
}

/// The process's database, when it keeps one.
pub(crate) fn process_log() -> Option<&'static Log> {
    crate::session::store::database_log()
}

fn io_error(error: &anyhow::Error) -> io::Error {
    io::Error::other(format!("{error:#}"))
}

/// The document `file` keeps at `path` (the harness store, the reuse
/// ledger): the database's copy once it holds one, else the file's text;
/// `None` when neither exists.
///
/// # Errors
///
/// The database or the file could not be read.
pub(crate) fn read_document(
    log: Option<&Log>,
    file: StateFile,
    path: &Path,
) -> io::Result<Option<String>> {
    if let Some(log) = log {
        if let Some(text) = log
            .value(&file.scope(path), DOCUMENT_KEY)
            .map_err(|error| io_error(&error))?
        {
            return Ok(Some(text));
        }
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Replace the document `file` keeps at `path` with `text`: in the
/// database when the process keeps one, else in the file, written
/// atomically with `options`.
///
/// # Errors
///
/// The write did not commit, or the file could not be written.
pub(crate) fn write_document(
    file: StateFile,
    path: &Path,
    text: String,
    options: crate::settings::storage::AtomicWriteOptions,
) -> anyhow::Result<()> {
    match process_log() {
        Some(log) => {
            log.put_value(&file.scope(path), DOCUMENT_KEY, text, Origin::Written)
                .wait_blocking()?;
            Ok(())
        }
        None => crate::settings::storage::atomic_write_with(path, &text, options),
    }
}

/// The agent state of another process's database, such as a daemon's
/// whose agent dir is not this process's (its tests read it this way).
pub struct StateDatabase {
    log: Log,
}

impl StateDatabase {
    /// Open the database at `database` (`session::store::database_path`).
    ///
    /// # Errors
    ///
    /// The database cannot be opened or migrated.
    pub fn open(database: &Path) -> anyhow::Result<Self> {
        Ok(StateDatabase {
            log: Log::open(database, pa_log::DEFAULT_EVENT_RETENTION)?,
        })
    }

    /// The jobs of the `scheduled-jobs.json` at `path`, as the cron store
    /// reads them.
    #[must_use]
    pub fn scheduled_jobs(&self, path: &Path) -> Vec<crate::cron::AgentCronJob> {
        crate::cron::store::read_jobs_state_in(Some(&self.log), path).jobs
    }

    /// The records of the crash log at `path`, oldest first.
    ///
    /// # Errors
    ///
    /// The database or the file could not be read, or a record is not JSON.
    pub fn crash_records(&self, path: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
        crash_log::records_in(Some(&self.log), path)
    }

    /// The document (a harness store, the reuse ledger) kept at `path`.
    ///
    /// # Errors
    ///
    /// The database or the file could not be read.
    pub fn document(&self, file: StateFile, path: &Path) -> io::Result<Option<String>> {
        read_document(Some(&self.log), file, path)
    }
}

/// Every file the database holds the state of, with the bytes its file
/// would hold now (the export's layout: what a build before the move
/// reads). Empty without a database.
///
/// # Errors
///
/// The partitions could not be listed.
pub(crate) fn exported_files() -> anyhow::Result<Vec<(PathBuf, io::Result<Vec<u8>>)>> {
    let Some(log) = process_log() else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for file in StateFile::ALL {
        for partition in log.partitions(file.prefix())? {
            let path = PathBuf::from(&partition.scope[file.prefix().len()..]);
            let bytes = import::render(log, file, &path).map_err(|error| io_error(&error));
            files.push((path, bytes));
        }
    }
    Ok(files)
}

/// The bytes the store `file` at `path` keeps, in its file's layout: the
/// database's once it holds them, else the file's; `None` when neither is.
/// Tests read a store's state this way wherever it is kept.
#[cfg(test)]
pub(crate) fn kept_bytes(file: StateFile, path: &Path) -> Option<Vec<u8>> {
    if let Some(log) = process_log() {
        if log.partition(&file.scope(path)).ok().flatten().is_some() {
            return import::render(log, file, path).ok();
        }
    }
    std::fs::read(path).ok()
}

#[cfg(test)]
mod tests;
