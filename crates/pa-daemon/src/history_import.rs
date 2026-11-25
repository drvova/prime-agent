//! A distribution build's one-time history import, run off every path a
//! client waits on: the supervisor hands it to the blocking pool after the
//! boot reap and keeps serving. Connected clients hear a `history_import`
//! broadcast when it starts and when it finishes (a client that connects
//! mid-import gets the started frame after its hello), and the resume
//! picker re-reads the catalog on the finished one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::settings::{
    plan_official_history_import, HistoryImportCheck, HistoryImportReport, HistoryImportRun,
};
use pa_types::daemon::{DaemonOutbound, HistoryImportPhase};
use pa_types::platform::StateLayout;
use serde_json::Value;

use crate::supervisor::{ClientRouting, Supervisor};

/// How one import settled, for the supervisor's log and telemetry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoryImportOutcome {
    /// Nothing ran: not this build's own dir, already imported, nothing to
    /// copy, or another process importing.
    Idle,
    Completed(HistoryImportReport),
    Failed(String),
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

/// Plan and run the import on the blocking pool; the returned handle settles
/// with its outcome. The call itself never waits on the copy: `announce`
/// receives the started frame before the first file is copied and the
/// finished frame after the marker, and `before_file` runs before each file
/// (the supervisor passes a no-op).
pub(crate) fn spawn_history_import(
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

impl Supervisor {
    /// Start the boot's history import in the background (a distribution
    /// build's own agent dir only), then the import of the agent dir's JSONL
    /// session files into the transcript store: it waits for the boot reap,
    /// never holds up the accept loop, and reports to every client (the
    /// history import), the log, and telemetry when each settles.
    pub(crate) fn spawn_boot_history_import(self: &Arc<Self>) {
        let Some(home) = pa_types::platform::home_dir() else {
            return;
        };
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            supervisor.wait_for_boot_reap().await;
            let started = std::time::Instant::now();
            let announcer = Arc::clone(&supervisor);
            let handle = spawn_history_import(
                StateLayout::BUILD,
                home,
                supervisor.options.agent_dir.clone(),
                move |frame| {
                    let frame = Arc::new(frame);
                    let running = frame.get("phase").and_then(Value::as_str) == Some("started");
                    *announcer.history_import.lock().unwrap() = running.then(|| Arc::clone(&frame));
                    let _ = announcer.events.send((ClientRouting::Broadcast, frame));
                },
                |_| {},
            );
            let outcome = handle
                .await
                .unwrap_or_else(|error| HistoryImportOutcome::Failed(error.to_string()));
            *supervisor.history_import.lock().unwrap() = None;
            supervisor.note_history_import(&outcome, started.elapsed());
            // Then the JSONL session files (the chats just copied among
            // them) into the transcript store's database.
            let started = std::time::Instant::now();
            let agent_dir = supervisor.options.agent_dir.clone();
            let imported = tokio::task::spawn_blocking(move || {
                pa_core::session::store::import_session_trees(&agent_dir)
            })
            .await;
            match imported {
                Ok(Some(report)) => supervisor.note_transcript_import(&report, started.elapsed()),
                Ok(None) => {}
                Err(error) => supervisor.log_line(&format!("transcript import stopped: {error}")),
            }
            // Then the agent's other state files: schedules, harness stores,
            // the reuse ledger, and this daemon's event bus and crash log.
            let agent_dir = supervisor.options.agent_dir.clone();
            let descriptor_dir =
                crate::descriptor::descriptor_dir(&agent_dir, &supervisor.options.socket_path);
            let daemon_files = [
                (
                    pa_core::agent_state::StateFile::EventBus,
                    descriptor_dir.join(crate::event_bus::EVENT_BUS_FILE_NAME),
                ),
                (
                    pa_core::agent_state::StateFile::CrashLog,
                    descriptor_dir.join(crate::supervisor::SESSION_CRASH_LOG),
                ),
            ];
            let imported = tokio::task::spawn_blocking(move || {
                pa_core::agent_state::import_agent_state(&agent_dir, &daemon_files)
            })
            .await;
            match imported {
                Ok(Some(report)) => {
                    if report.imported > 0 || !report.failed.is_empty() {
                        supervisor.log_line(&format!(
                            "agent state import: {} file(s) imported, {} already held, {} failed",
                            report.imported,
                            report.already_held,
                            report.failed.len()
                        ));
                    }
                    for (path, reason) in &report.failed {
                        supervisor.log_line(&format!(
                            "agent state import skipped {}: {reason}",
                            path.display()
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) => supervisor.log_line(&format!("agent state import stopped: {error}")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORK: StateLayout = StateLayout::Distribution { variant: "fork" };

    /// The boot path's contract, modeled on the startup-flush guard: with
    /// the copy hanging on its first file, the hand-off has already returned
    /// and announced the import, and no chat has landed yet; released, the
    /// import finishes, announces it, and the chats are in the fork's dir.
    /// A regression that copies inline reds the hand-off timeout instead of
    /// wedging the suite.
    #[test]
    fn a_hanging_copy_never_blocks_the_boot() {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(hanging_copy_never_blocks_the_boot());
    }

    async fn hanging_copy_never_blocks_the_boot() {
        let home = tempfile::tempdir().expect("home");
        let official = StateLayout::Official
            .home_agent_dir(home.path())
            .join("sessions");
        std::fs::create_dir_all(&official).expect("official sessions");
        let mut ids: Vec<String> = ["/repo", "/other"]
            .into_iter()
            .map(|cwd| {
                let mut session = crate::session_store::SessionFile::create(cwd, None, 0);
                session.append_session_info("a chat");
                let id = session.session_id().to_string();
                session.set_path(official.join(crate::session_store::session_file_name(&id)));
                session.rewrite().expect("official chat");
                // The official build's chats are its JSONL files.
                crate::session_store::test_support::into_original_file(&session.path);
                id
            })
            .collect();
        ids.sort();
        let fork = FORK.home_agent_dir(home.path());

        let (frames_tx, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
        let mut entered_tx = Some(entered_tx);
        // The hand-off runs on its own thread, so a regression to an inline copy reds the
        // bounded wait below instead of wedging the suite.
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Handle::current();
        let agent_dir = fork.clone();
        let home_dir = home.path().to_path_buf();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            let handle = spawn_history_import(
                FORK,
                home_dir,
                agent_dir,
                move |frame| {
                    let _ = frames_tx.send(frame);
                },
                move |_| {
                    if let Some(entered) = entered_tx.take() {
                        let _ = entered.send(());
                        // The copy hangs here until the test releases it.
                        let _ = release.recv();
                    }
                },
            );
            let _ = handle_tx.send(handle);
        });
        let hand_off = handle_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the hand-off returns while the copy still hangs");

        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .expect("the copy reached its first file")
            .expect("entered");
        let started = frames.recv().await.expect("started frame");
        assert_eq!(
            started,
            serde_json::json!({"type": "history_import", "phase": "started", "sessions": 2})
        );
        assert!(
            !fork.join("sessions").exists(),
            "no chat landed while the copy hangs"
        );

        release_tx.send(()).expect("release");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), hand_off)
            .await
            .expect("the released import settles")
            .expect("join");
        let finished = frames.recv().await.expect("finished frame");
        assert_eq!(
            finished,
            serde_json::json!({"type": "history_import", "phase": "finished", "sessions": 2, "kept": 0})
        );
        assert!(
            matches!(&outcome, HistoryImportOutcome::Completed(report) if report.chats_imported == 2),
            "{outcome:?}"
        );
        // The resume picker's scan over the fork's own dir lists the imported chats.
        let mut listed: Vec<String> = crate::session_scan::list_sessions(&fork.join("sessions"))
            .into_iter()
            .map(|info| info.id)
            .collect();
        listed.sort();
        assert_eq!(listed, ids);
    }
}
