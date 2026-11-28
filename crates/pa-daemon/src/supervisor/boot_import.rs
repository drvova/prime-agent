//! The supervisor's boot imports (`pa_core::session::boot_import`, shared
//! with the runtime daemon), run off every path a client waits on: the
//! supervisor starts them after the boot reap and keeps serving. Connected
//! clients hear the history import's `history_import` broadcasts; a client
//! that connects mid-import gets the started frame after its hello
//! (`BootImport::running`).

use std::sync::Arc;

use pa_core::agent_state::StateFile;
use pa_core::session::boot_import::BootImportReporter;

use super::{ClientRouting, Supervisor};

impl Supervisor {
    /// Start the boot's imports in the background once the boot reap is
    /// done: the history import (a distribution build's own agent dir only),
    /// then the agent dir's JSONL session files into the transcript store,
    /// then the agent's other state files with this daemon's event bus and
    /// crash log. Each reports to every client, the log, and telemetry while
    /// recording is on; none holds up the accept loop.
    pub(crate) fn spawn_boot_history_import(self: &Arc<Self>) {
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            supervisor.wait_for_boot_reap().await;
            let agent_dir = supervisor.options.agent_dir.clone();
            let descriptor_dir = pa_core::platform::daemon_endpoint::daemon_state_dir(
                &agent_dir,
                &supervisor.options.socket_path,
            );
            let daemon_files = vec![
                (
                    StateFile::EventBus,
                    descriptor_dir.join(crate::event_bus::EVENT_BUS_FILE_NAME),
                ),
                (
                    StateFile::CrashLog,
                    descriptor_dir.join(pa_core::session::crash::SESSION_CRASH_LOG),
                ),
            ];
            let (announcer, logger, recorder) = (
                Arc::clone(&supervisor),
                Arc::clone(&supervisor),
                Arc::clone(&supervisor),
            );
            let reporter = BootImportReporter {
                announce: Box::new(move |frame| {
                    let _ = announcer
                        .events
                        .send((ClientRouting::Broadcast, Arc::new(frame)));
                }),
                log: Box::new(move |line| logger.log_line(line)),
                telemetry: Box::new(move || {
                    if !recorder.telemetry_recording_on() {
                        return None;
                    }
                    recorder.telemetry.lock().unwrap().clone()
                }),
            };
            Arc::clone(&supervisor.history_import)
                .run(agent_dir, daemon_files, reporter)
                .await;
        });
    }
}
