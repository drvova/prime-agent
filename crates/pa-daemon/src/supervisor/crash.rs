//! Session crash visibility: the durable crash log, the descriptor's last
//! crash, the event attached clients receive, and the telemetry count.
use super::{json, persist_worker, util, Arc, ResidentWorker, Supervisor, Value};
use crate::session_host::SessionCrash;

/// The append-only crash log's file name inside the worker-records directory:
/// one JSON object per line, never rewritten by the daemon.
pub(crate) const SESSION_CRASH_LOG: &str = "session-crashes.jsonl";

impl Supervisor {
    /// Make one session crash visible: append it to the crash log, stamp it
    /// on the session's descriptor (the roster's `lastCrash`), tell every
    /// attached client, and count it. The caller owns what happens next (the
    /// restart with backoff, or the give-up).
    pub(super) async fn record_session_crash(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        crash: &SessionCrash,
    ) {
        let (active_session_id, record) = {
            let mut descriptor = resident.descriptor.lock().await;
            let record = json!({
                "workerId": resident.worker_id,
                "activeSessionId": descriptor.root_active_session_id,
                "sessionId": descriptor.root_session_id,
                "sessionFile": descriptor.session_file,
                "kind": crash.kind,
                "message": crash.message,
                "location": crash.location,
                "thread": crash.thread,
                "at": crash.at,
            });
            descriptor.last_failure_at = Some(util::now_iso());
            descriptor.last_error = Some(format!(
                "session {}: {} ({})",
                crash.kind, crash.message, crash.location
            ));
            descriptor
                .rest
                .insert("lastCrash".to_string(), record.clone());
            if let Err(error) = persist_worker(&resident.descriptor_path, &descriptor) {
                self.log_line(&format!(
                    "session worker {}: the crash could not be stamped on its record: {error:#}",
                    resident.worker_id
                ));
            }
            (descriptor.root_active_session_id.clone(), record)
        };
        let log_path = self.descriptor_dir.join(SESSION_CRASH_LOG);
        let line = format!("{record}\n");
        let appended = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut file| {
                    file.write_all(line.as_bytes())?;
                    file.sync_data()
                })
        })
        .await;
        match appended {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.log_line(&format!("session crash log append failed: {error}")),
            Err(error) => self.log_line(&format!("session crash log append failed: {error}")),
        }
        self.log_line(&format!(
            "session worker {} crashed ({}): {} at {} on thread {}",
            resident.worker_id, crash.kind, crash.message, crash.location, crash.thread
        ));
        self.publish_session_fault(
            &active_session_id,
            json!({
                "type": "session_crashed",
                "kind": crash.kind,
                "message": crash.message,
                "location": crash.location,
                "thread": crash.thread,
                "at": crash.at,
            }),
        );
        if self.telemetry_recording_on() {
            if let Some(client) = &*self.telemetry.lock().unwrap() {
                pa_core::session_engine::telemetry::track_session_crashed(client, &crash.kind);
            }
        }
    }

    /// Send one supervisor-made session event to the session's attached
    /// clients (the session itself cannot: it is down).
    pub(super) fn publish_session_fault(&self, active_session_id: &str, event: Value) {
        let frame = serde_json::to_value(pa_types::daemon::DaemonOutbound::SessionEvent {
            active_session_id: active_session_id.to_string(),
            event,
            meta: None,
            rest: serde_json::Map::default(),
        })
        .unwrap_or_default();
        self.publish_session_event(active_session_id, &Arc::new(frame));
    }
}
