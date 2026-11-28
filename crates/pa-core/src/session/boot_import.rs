//! A daemon's boot import, shared by both daemons (moved from pa-daemon's
//! `history_import.rs`): a distribution build's one-time history import
//! ([`crate::settings::plan_official_history_import`]), then the agent dir's
//! JSONL session files into the transcript store
//! ([`crate::session::store::import_session_trees`]), then the agent's
//! other state files ([`crate::agent_state::import_agent_state`]).
//!
//! It runs off every path a client waits on: a daemon spawns
//! [`BootImport::run`] on its own task once it serves (the old daemon after
//! its boot reap), and each copy runs on the blocking pool. Connected clients
//! hear a `history_import` frame when the history import starts and when it
//! finishes ([`BootImportReporter`]'s `announce`); a client that connects
//! mid-import reads the started frame from [`BootImport::running`], and the
//! resume picker re-reads the catalog on the finished one.
//!
//! Debug builds only, for the daemons' end-to-end tests:
//! [`HISTORY_IMPORT_VARIANT_ENV`] plans the import as that distribution
//! variant's build, and [`HISTORY_IMPORT_HOLD_ENV`] holds the copy before
//! its first file while the file it names exists.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pa_types::daemon::{DaemonOutbound, HistoryImportPhase};
use pa_types::platform::StateLayout;
use serde_json::Value;

use crate::agent_state::StateFile;
use crate::settings::{
    plan_official_history_import, HistoryImportCheck, HistoryImportReport, HistoryImportRun,
};
use pa_telemetry::TelemetryClient;

/// Debug builds only: the distribution variant the boot's history import is
/// planned for in place of the build's own layout, so a test of an official
/// build can boot a daemon over a distribution build's agent dir.
pub const HISTORY_IMPORT_VARIANT_ENV: &str = "PA_DAEMON_HISTORY_IMPORT_VARIANT";

/// Debug builds only: a file whose existence holds the boot's history import
/// before its first file is copied (after the started frame), so a test can
/// connect a client mid-import; removing it releases the copy.
pub const HISTORY_IMPORT_HOLD_ENV: &str = "PA_DAEMON_HISTORY_IMPORT_HOLD";

/// How long the hold seam holds at most, so a test that died never leaves a
/// daemon's import held for good.
#[cfg(debug_assertions)]
const HOLD_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// How one history import settled, for the daemon's log and telemetry.
#[derive(Debug, PartialEq, Eq)]
enum HistoryImportOutcome {
    /// Nothing ran: not this build's own dir, already imported, nothing to
    /// copy, or another process importing.
    Idle,
    Completed(HistoryImportReport),
    Failed(String),
}

/// Where a settled import's telemetry goes: the daemon's client, `None`
/// while telemetry is off.
pub type TelemetrySource = Box<dyn Fn() -> Option<TelemetryClient> + Send + Sync>;

/// Where a daemon's boot import reports, each called from the import's
/// task or the blocking pool between file copies, so each returns at once.
pub struct BootImportReporter {
    /// Hands every connected client one `history_import` frame (the old
    /// wire's `DaemonOutbound::HistoryImport`, as JSON): the started frame
    /// before the first file is copied, the finished one after the marker.
    pub announce: Box<dyn Fn(Value) + Send + Sync>,
    /// Appends one line to the daemon's log.
    pub log: Box<dyn Fn(&str) + Send + Sync>,
    /// The telemetry client a settled import is reported to.
    pub telemetry: TelemetrySource,
}

/// One daemon's boot import: what is running now, for the clients that
/// connect mid-import.
#[derive(Debug, Default)]
pub struct BootImport {
    /// The started frame while the history import copies.
    running: Mutex<Option<Arc<Value>>>,
}

/// One `history_import` frame.
fn frame(
    phase: HistoryImportPhase,
    sessions: usize,
    kept: Option<usize>,
    config: Vec<String>,
    error: Option<String>,
) -> Value {
    serde_json::to_value(DaemonOutbound::HistoryImport {
        phase,
        sessions: sessions as u64,
        kept: kept.map(|kept| kept as u64),
        config,
        error,
        rest: serde_json::Map::default(),
    })
    .unwrap_or(Value::Null)
}

/// Plan and run the history import on the blocking pool; the returned handle
/// settles with its outcome. The call itself never waits on the copy:
/// `announce` receives the started frame before the first file is copied and
/// the finished frame after the marker, and `before_file` runs before each
/// file.
fn spawn_history_import(
    layout: StateLayout,
    home: PathBuf,
    agent_dir: PathBuf,
    announce: impl Fn(Value) + Send + 'static,
    mut before_file: impl FnMut(&Path) + Send + 'static,
) -> tokio::task::JoinHandle<HistoryImportOutcome> {
    tokio::task::spawn_blocking(move || {
        let plan =
            match plan_official_history_import(layout, &home, &agent_dir, HistoryImportRun::Once) {
                Ok(HistoryImportCheck::Pending(plan)) => plan,
                Ok(
                    HistoryImportCheck::NotApplicable
                    | HistoryImportCheck::AlreadyImported
                    | HistoryImportCheck::NothingToImport
                    | HistoryImportCheck::Busy,
                ) => return HistoryImportOutcome::Idle,
                Err(error) => return HistoryImportOutcome::Failed(error.to_string()),
            };
        let config: Vec<String> = plan
            .config_entries()
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        announce(frame(
            HistoryImportPhase::Started,
            plan.chats(),
            None,
            config,
            None,
        ));
        match plan.run(&mut before_file) {
            Ok(report) => {
                announce(frame(
                    HistoryImportPhase::Finished,
                    report.chats_imported,
                    Some(report.kept.len()),
                    report.config_added.clone(),
                    None,
                ));
                HistoryImportOutcome::Completed(report)
            }
            Err(error) => {
                announce(frame(
                    HistoryImportPhase::Finished,
                    0,
                    None,
                    Vec::new(),
                    Some(error.to_string()),
                ));
                HistoryImportOutcome::Failed(error.to_string())
            }
        }
    })
}

impl BootImport {
    /// The started frame of the history import running now, for a client
    /// that connects mid-import (the finished one reaches it as every
    /// client's); `None` when none runs.
    #[must_use]
    pub fn running(&self) -> Option<Arc<Value>> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_running(&self, frame: Option<Arc<Value>>) {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = frame;
    }

    /// Runs the boot's imports into `agent_dir`, in order, each settled
    /// before the next: the history import (a distribution build's own agent
    /// dir only; nothing at all without a home directory), the transcript
    /// import, then the agent-state import with the daemon's own state files
    /// `daemon_files` (its event bus and crash log). Each copy runs on the
    /// blocking pool; each settled import is reported to `reporter`'s log and
    /// telemetry.
    #[tracing::instrument(level = "debug", skip_all, name = "boot_import")]
    pub async fn run(
        self: Arc<Self>,
        agent_dir: PathBuf,
        daemon_files: Vec<(StateFile, PathBuf)>,
        reporter: BootImportReporter,
    ) {
        let reporter = Arc::new(reporter);
        let Some(home) = pa_types::platform::home_dir() else {
            return;
        };
        // The build's layout, or in a debug build the seam's variant.
        let layout = StateLayout::BUILD;
        #[cfg(debug_assertions)]
        let layout = match pa_types::session_scope::var(HISTORY_IMPORT_VARIANT_ENV) {
            Ok(variant)
                if !variant.is_empty()
                    && variant
                        .bytes()
                        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-')) =>
            {
                StateLayout::from_stamp(Some(Box::leak(variant.into_boxed_str())))
            }
            _ => layout,
        };
        // In a debug build, the seam's hold before the first file.
        #[cfg(debug_assertions)]
        let mut hold = pa_types::session_scope::var_os(HISTORY_IMPORT_HOLD_ENV)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let started = std::time::Instant::now();
        let (import, announcer) = (Arc::clone(&self), Arc::clone(&reporter));
        let handle = spawn_history_import(
            layout,
            home,
            agent_dir.clone(),
            move |frame| {
                let frame = Arc::new(frame);
                let running = frame.get("phase").and_then(Value::as_str) == Some("started");
                import.set_running(running.then(|| Arc::clone(&frame)));
                (announcer.announce)(Arc::unwrap_or_clone(frame));
            },
            move |_| {
                #[cfg(debug_assertions)]
                if let Some(hold) = hold.take() {
                    let deadline = std::time::Instant::now() + HOLD_LIMIT;
                    while hold.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            },
        );
        let outcome = handle
            .await
            .unwrap_or_else(|error| HistoryImportOutcome::Failed(error.to_string()));
        self.set_running(None);
        report_history_import(&reporter, &outcome, started.elapsed());
        // Then the JSONL session files (the chats just copied among them)
        // into the transcript store's database.
        let started = std::time::Instant::now();
        let dir = agent_dir.clone();
        let imported =
            tokio::task::spawn_blocking(move || crate::session::store::import_session_trees(&dir))
                .await;
        match imported {
            Ok(Some(report)) => report_transcript_import(&reporter, &report, started.elapsed()),
            Ok(None) => {}
            Err(error) => (reporter.log)(&format!("transcript import stopped: {error}")),
        }
        // Then the agent's other state files: schedules, harness stores, the
        // reuse ledger, and the daemon's event bus and crash log.
        let imported = tokio::task::spawn_blocking(move || {
            crate::agent_state::import_agent_state(&agent_dir, &daemon_files)
        })
        .await;
        match imported {
            Ok(Some(report)) => {
                if report.imported > 0 || !report.failed.is_empty() {
                    (reporter.log)(&format!(
                        "agent state import: {} file(s) imported, {} already held, {} failed",
                        report.imported,
                        report.already_held,
                        report.failed.len()
                    ));
                }
                for (path, reason) in &report.failed {
                    (reporter.log)(&format!(
                        "agent state import skipped {}: {reason}",
                        path.display()
                    ));
                }
                for (path, ids) in &report.duplicate_job_ids {
                    (reporter.log)(&format!(
                        "agent state import: {} lists job id(s) {} more than once; \
                         kept the last job with each",
                        path.display(),
                        ids.join(", ")
                    ));
                }
            }
            Ok(None) => {}
            Err(error) => (reporter.log)(&format!("agent state import stopped: {error}")),
        }
    }
}

/// Reports a settled history import: one log line, and its `agent sessions
/// imported` event (counts and a size bucket only). Nothing ran: nothing to
/// report.
fn report_history_import(
    reporter: &BootImportReporter,
    outcome: &HistoryImportOutcome,
    elapsed: std::time::Duration,
) {
    let (report, result) = match outcome {
        HistoryImportOutcome::Idle => return,
        HistoryImportOutcome::Completed(report) => {
            (reporter.log)(&format!(
                "history import: {} chat(s), {} file(s) copied, {} kept, config added: [{}]",
                report.chats_imported,
                report.files_imported,
                report.kept.len(),
                report.config_added.join(", ")
            ));
            (report.clone(), "completed")
        }
        HistoryImportOutcome::Failed(error) => {
            (reporter.log)(&format!(
                "history import stopped (resumes on the next boot): {error}"
            ));
            (HistoryImportReport::default(), "failed")
        }
    };
    if let Some(client) = (reporter.telemetry)() {
        crate::session_engine::telemetry::track_sessions_imported(
            &client,
            "automatic",
            result,
            &report,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        );
    }
}

/// Reports a settled import of JSONL session files into the transcript
/// store: one log line, one per file that failed (skipped whole; the next
/// boot retries it), and its `agent transcripts imported` event (counts and
/// a size bucket only).
fn report_transcript_import(
    reporter: &BootImportReporter,
    report: &crate::session::store::TranscriptImportReport,
    elapsed: std::time::Duration,
) {
    (reporter.log)(&format!(
        "transcript import: {} session file(s) imported ({} bytes), {} already stored, {} failed",
        report.imported,
        report.bytes,
        report.already_stored,
        report.failed.len()
    ));
    for (path, error) in &report.failed {
        (reporter.log)(&format!(
            "transcript import skipped {} (retried on the next boot): {error}",
            path.display()
        ));
    }
    if let Some(client) = (reporter.telemetry)() {
        crate::session_engine::telemetry::track_transcripts_imported(
            &client,
            report,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        );
    }
}

#[cfg(test)]
#[path = "boot_import_tests.rs"]
mod tests;
