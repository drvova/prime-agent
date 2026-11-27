//! The daemon's crash log (`session-crashes.jsonl`): one JSON object per
//! crash, appended and never rewritten.

use std::path::Path;

use anyhow::Context;
use pa_log::{Log, Origin};
use serde_json::Value;

use super::{import_state_file, process_log, StateFile};

/// The name of `pa_log`'s writer thread: a panic there reaches the daemon's
/// panic hook on the one thread that could answer the hook's append.
const DATABASE_WRITER_THREAD: &str = "pa-log-writer";

/// Append `record` to the crash log at `path`, durably: to the database
/// when the process keeps one (the log's earlier records, still in the
/// file, are imported first), else to the file, synced.
///
/// # Errors
///
/// The record could not be stored; or it was, and the file's earlier
/// records could not be imported (the error names the file).
pub fn append_crash_record(path: &Path, record: &Value) -> anyhow::Result<()> {
    let Some(log) = process_log() else {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        file.write_all(format!("{record}\n").as_bytes())?;
        file.sync_data()?;
        return Ok(());
    };
    let scope = StateFile::CrashLog.scope(path);
    let on_writer = std::thread::current().name() == Some(DATABASE_WRITER_THREAD);
    let imported = if on_writer || log.partition(&scope)?.is_some() {
        Ok(())
    } else {
        import_state_file(log, StateFile::CrashLog, path).map(|_| ())
    };
    let receipt = log.append_crashes(&scope, vec![record.to_string()], Origin::Written);
    if on_writer {
        // Nothing could answer while this thread waits: the writer runs the
        // append once it is back, if it comes back.
        drop(receipt);
        return Ok(());
    }
    receipt.wait_blocking()?;
    imported.map_err(|reason| {
        anyhow::anyhow!(
            "the crash was recorded, but the earlier records in {} were not imported: {reason}",
            path.display()
        )
    })
}

/// The records of the crash log at `path`, oldest first.
///
/// # Errors
///
/// The database or the file could not be read, or a record is not JSON.
pub fn crash_records(path: &Path) -> anyhow::Result<Vec<Value>> {
    records_in(process_log(), path)
}

/// [`crash_records`] from `log`.
pub(super) fn records_in(log: Option<&Log>, path: &Path) -> anyhow::Result<Vec<Value>> {
    if let Some(log) = log {
        let scope = StateFile::CrashLog.scope(path);
        if log.partition(&scope)?.is_some() {
            return log
                .crashes(&scope)?
                .into_iter()
                .map(|row| Ok(serde_json::from_str(&row.record)?))
                .collect();
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}
