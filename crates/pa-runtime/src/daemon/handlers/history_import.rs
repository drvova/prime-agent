//! The runtime daemon's boot imports (`pa_core::session::boot_import`, the
//! old supervisor's, shared): a distribution build's one-time history
//! import, then the agent dir's JSONL session files into the transcript
//! store, then the agent's other state files with this daemon's event bus
//! and crash log. They start once the daemon serves and run on their own
//! task and the blocking pool, so neither its readiness nor a client's
//! first frame waits on them.
//!
//! The history import's `history_import` frames, without their `type`, go
//! out on the live-only [`SYSTEM_HISTORY_IMPORT`] (`{phase: "started",
//! sessions, config?}` before the first file is copied, `{phase:
//! "finished", sessions, kept?, config?, error?}` after the marker), in
//! order. A client that connects mid-import reads the started one with the
//! `system.history_import` query (`null` while none runs), after it
//! subscribed to the topic, as the old supervisor sends it the started frame
//! after its hello.

use std::path::PathBuf;
use std::sync::Arc;

use pa_core::agent_state::StateFile;
use pa_core::session::boot_import::{BootImportReporter, TelemetrySource};
use pa_types::event_log::{NewEvent, Topic, Trust, SYSTEM_HISTORY_IMPORT};
use serde_json::Value;
use tokio::task::JoinHandle;

use super::RuntimeHandlers;
use crate::session::SessionBuilder;

/// A `history_import` frame without its `type`: the topic's payload and the
/// query's result.
fn without_type(frame: &Value) -> Value {
    let mut payload = frame.clone();
    if let Some(payload) = payload.as_object_mut() {
        payload.remove("type");
    }
    payload
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `system.history_import`: the running history import's started
    /// frame, without its `type`; `null` while none runs.
    pub(super) fn history_import(&self) -> Value {
        self.boot_import
            .running()
            .map_or(Value::Null, |frame| without_type(&frame))
    }

    /// Starts the boot's imports into the agent dir, with `crash_log` the
    /// daemon's crash log and `telemetry` its client's: the imports' task and
    /// the one that publishes their frames, in order. Both end on their own;
    /// the drain aborts what still runs.
    pub(in crate::daemon) fn spawn_boot_import(
        &self,
        crash_log: PathBuf,
        telemetry: TelemetrySource,
    ) -> [JoinHandle<()>; 2] {
        let (frames, mut announced) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let runtime = Arc::clone(&self.runtime);
        let publisher = tokio::spawn(async move {
            while let Some(frame) = announced.recv().await {
                let published = runtime
                    .publish(NewEvent {
                        topic: Topic::new(SYSTEM_HISTORY_IMPORT).expect("a topic"),
                        source: "runtime".to_string(),
                        session: None,
                        payload: without_type(&frame),
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await;
                if let Err(error) = published {
                    tracing::warn!("a history import frame was not published: {error:#}");
                }
            }
        });
        let reporter = BootImportReporter {
            announce: Box::new(move |frame| {
                let _ = frames.send(frame);
            }),
            log: Box::new(|line| tracing::info!("{line}")),
            telemetry,
        };
        let daemon_files = vec![
            (
                StateFile::EventBus,
                self.agent_dir.join(crate::events::EVENT_BUS_FILE_NAME),
            ),
            (StateFile::CrashLog, crash_log),
        ];
        let imports = tokio::spawn(Arc::clone(&self.boot_import).run(
            self.agent_dir.clone(),
            daemon_files,
            reporter,
        ));
        [imports, publisher]
    }
}
