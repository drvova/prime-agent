//! The one-time import of the agent's state files into the database, and
//! the way back out (the bytes each file would hold, for `export-jsonl`).
//!
//! A file is imported whole, in one transaction, or not at all, and only
//! while the database does not hold its partition: a second import, or one
//! racing the store's own first write, changes nothing. A file that does not
//! parse is skipped and reported, and stays the store's source until the
//! store's next write (which, as it did to the file, replaces it). An
//! imported original is left in place, made read-only (as the transcript
//! originals are), and never written again: it is the rollback.

use std::path::{Path, PathBuf};

use pa_log::{Log, Origin};
use sha2::Digest;

use super::{event_bus, StateFile, DOCUMENT_KEY};

/// What [`import_agent_state`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateImportReport {
    /// Files imported by this pass.
    pub imported: u64,
    /// Files whose state the database already held.
    pub already_held: u64,
    /// Files that could not be imported, and why; nothing of them is stored.
    pub failed: Vec<(PathBuf, String)>,
    /// `scheduled-jobs.json` files that list a job id more than once, with
    /// those ids: each was imported keeping the last job with the id.
    pub duplicate_job_ids: Vec<(PathBuf, Vec<String>)>,
}

/// What [`import_state_file`] did with one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StateImport {
    /// Stored; `duplicate_job_ids` names the ids a `scheduled-jobs.json`
    /// listed more than once (the last job with each was kept).
    Imported {
        duplicate_job_ids: Vec<String>,
    },
    AlreadyHeld,
    /// There is no file.
    Missing,
}

/// Import the file `file` at `path` into `log`.
///
/// # Errors
///
/// Why the file was not imported: it could not be read or does not parse,
/// or the write did not commit. Nothing of it is stored then.
pub(crate) fn import_state_file(
    log: &Log,
    file: StateFile,
    path: &Path,
) -> Result<StateImport, String> {
    let scope = file.scope(path);
    if log
        .partition(&scope)
        .map_err(|error| format!("{error:#}"))?
        .is_some()
    {
        return Ok(StateImport::AlreadyHeld);
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StateImport::Missing)
        }
        Err(error) => return Err(error.to_string()),
    };
    let origin = Origin::Imported {
        from: path.to_string_lossy().into_owned(),
        sha256: sha2::Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    };
    let text = String::from_utf8(bytes).map_err(|_| "it is not UTF-8".to_string())?;
    let mut duplicate_job_ids = Vec::new();
    let receipt = match file {
        StateFile::ScheduledJobs => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            if !value.is_object() {
                return Err("it is not a JSON object".to_string());
            }
            let mut state = crate::cron::store::jobs_state_from(&value);
            // Two jobs with one id: the last is kept, in its place, as the
            // file store's merging write keeps it, and the ids are reported.
            let mut seen = std::collections::HashSet::new();
            let mut kept = Vec::with_capacity(state.jobs.len());
            for job in state.jobs.into_iter().rev() {
                if seen.insert(job.id.clone()) {
                    kept.push(job);
                } else if !duplicate_job_ids.contains(&job.id) {
                    duplicate_job_ids.push(job.id);
                }
            }
            kept.reverse();
            duplicate_job_ids.reverse();
            state.jobs = kept;
            let rows = crate::cron::store::jobs_state_rows(&state)
                .map_err(|error| format!("{error:#}"))?;
            log.replace_sources(&scope, rows, origin)
        }
        StateFile::HarnessState | StateFile::ReuseLedger => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            if !value.is_object() {
                return Err("it is not a JSON object".to_string());
            }
            log.put_value(&scope, DOCUMENT_KEY, text, origin)
        }
        StateFile::EventBus => {
            let state: event_bus::EventBusState =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            let rows = state.rows().map_err(|error| format!("{error:#}"))?;
            log.replace_sources(&scope, rows, origin)
        }
        StateFile::CrashLog => {
            let mut records = Vec::new();
            let mut lines = text.split_inclusive('\n').peekable();
            while let Some(line) = lines.next() {
                let record = line.trim();
                if record.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(record) {
                    Ok(_) => records.push(record.to_string()),
                    // An unterminated last line is an append a crash cut
                    // short, not a record.
                    Err(_) if lines.peek().is_none() && !line.ends_with('\n') => {}
                    Err(error) => return Err(format!("a record is not a JSON object: {error}")),
                }
            }
            log.append_crashes(&scope, records, origin)
        }
    };
    match receipt.wait_blocking() {
        Ok(true) => {
            crate::session::store::freeze_original(path);
            Ok(StateImport::Imported { duplicate_job_ids })
        }
        Ok(false) => Ok(StateImport::AlreadyHeld),
        Err(error) => Err(format!("{error:#}")),
    }
}

/// The state files of `agent_dir`: its global harness store and reuse
/// ledger, and each session's schedules and harness store under the
/// `session-artifacts/` beside every session tree.
fn state_files(agent_dir: &Path) -> Vec<(StateFile, PathBuf)> {
    let mut files = vec![
        (
            StateFile::HarnessState,
            crate::refinement::get_harness_state_path(
                &crate::refinement::get_global_harness_state_dir(agent_dir),
            ),
        ),
        (StateFile::ReuseLedger, crate::reuse::ledger_path(agent_dir)),
    ];
    let mut artifact_roots: Vec<PathBuf> = crate::session::store::session_trees(agent_dir)
        .iter()
        .filter_map(|tree| tree.parent().map(|parent| parent.join("session-artifacts")))
        .collect();
    artifact_roots.sort();
    artifact_roots.dedup();
    for root in artifact_roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let dir = entry.path();
            files.push((
                StateFile::ScheduledJobs,
                dir.join(crate::cron::store::SESSION_SCHEDULED_JOBS_FILENAME),
            ));
            files.push((
                StateFile::HarnessState,
                crate::refinement::get_harness_state_path(
                    &crate::refinement::get_local_harness_state_dir(Some(&dir))
                        .unwrap_or_else(|| dir.clone()),
                ),
            ));
        }
    }
    files
}

/// Import the state files of `agent_dir` (its harness store, reuse ledger,
/// and every session's schedules and harness store) and `more` (the
/// daemon's event bus and crash log) that the database does not hold yet;
/// `None` when nothing ran (the file store is in use, or `agent_dir` is not
/// the one whose database the process opened). Idempotent, and each file is
/// its own transaction, so a pass that stopped part way resumes. Run it in
/// the background, never on a path a client waits on: until then, each
/// store reads its file.
#[must_use]
pub fn import_agent_state(
    agent_dir: &Path,
    more: &[(StateFile, PathBuf)],
) -> Option<StateImportReport> {
    let log = crate::session::store::database_log_of(agent_dir)?;
    let mut report = StateImportReport::default();
    for (file, path) in state_files(agent_dir).iter().chain(more) {
        match import_state_file(log, *file, path) {
            Ok(StateImport::Imported { duplicate_job_ids }) => {
                report.imported += 1;
                if !duplicate_job_ids.is_empty() {
                    report
                        .duplicate_job_ids
                        .push((path.clone(), duplicate_job_ids));
                }
            }
            Ok(StateImport::AlreadyHeld) => report.already_held += 1,
            Ok(StateImport::Missing) => {}
            Err(reason) => report.failed.push((path.clone(), reason)),
        }
    }
    Some(report)
}

/// The bytes the file `file` at `path` would hold now, from `log`: the
/// layout a build before the move reads.
///
/// # Errors
///
/// The database could not be read, or holds rows that do not parse.
pub(super) fn render(log: &Log, file: StateFile, path: &Path) -> anyhow::Result<Vec<u8>> {
    let scope = file.scope(path);
    match file {
        StateFile::ScheduledJobs => {
            let state = crate::cron::store::read_jobs_state_in(Some(log), path);
            Ok(format!("{}\n", serde_json::to_string_pretty(&state)?).into_bytes())
        }
        StateFile::HarnessState | StateFile::ReuseLedger => Ok(log
            .value(&scope, DOCUMENT_KEY)?
            .unwrap_or_default()
            .into_bytes()),
        StateFile::EventBus => {
            let state = event_bus::held_event_bus_in(log, path)?.unwrap_or_default();
            Ok(serde_json::to_string_pretty(&state)?.into_bytes())
        }
        StateFile::CrashLog => {
            let mut text = String::new();
            for row in log.crashes(&scope)? {
                text.push_str(&row.record);
                text.push('\n');
            }
            Ok(text.into_bytes())
        }
    }
}
