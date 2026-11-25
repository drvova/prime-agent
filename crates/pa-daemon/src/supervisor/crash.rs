//! Session crash visibility: the durable crash log, the descriptor's last
//! crash, the event attached clients receive, and the telemetry count.
use super::{json, persist_worker, util, Arc, ResidentWorker, Supervisor, Value};
use crate::session_host::SessionCrash;

/// The append-only crash log's file name inside the worker-records directory:
/// one JSON object per crash, never rewritten by the daemon. The agent
/// database keeps it under this path (`pa_core::agent_state`); with the file
/// transcript store, the file holds one record per line.
pub(crate) const SESSION_CRASH_LOG: &str = "session-crashes.jsonl";

/// What becomes of the turn that was running when a session went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InterruptedTurn {
    /// Settle it as failed for the attached clients and drop it from the
    /// recovery journal: the rebuilt session comes back idle.
    Settle,
    /// Leave it in the recovery journal: the rebuilt session runs it again.
    Replay,
}

/// The descriptor field counting consecutive daemon deaths that caught this
/// session mid-turn.
const UNCLEAN_REVIVALS: &str = "uncleanRevivals";

impl Supervisor {
    /// A boot found this session mid-turn in a daemon that died without
    /// settling it (an abort-class fault, a kill, a power loss). Make that
    /// visible the way a contained crash is, and decide the turn: the first
    /// time it is run again; when the daemon dies under the same session's
    /// turn twice in a row the turn is the likeliest cause, and it is
    /// settled as failed instead of taking the daemon down a third time.
    ///
    /// `revived_turn` is how far the transcript shows the turn got (`None`
    /// when no picked-up turn is on record): a turn whose answer was saved
    /// is over either way, and one whose prompt was saved continues behind
    /// a notice instead of being prompted again. `reaped_programs` is how
    /// many programs its commands left running the boot stopped.
    pub(super) async fn record_host_death(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        revived_turn: Option<crate::revived_turn::RevivedTurn>,
        reaped_programs: usize,
    ) {
        use crate::revived_turn::RevivedTurn;
        let prior = {
            let mut descriptor = resident.descriptor.lock().await;
            let prior = descriptor
                .rest
                .get(UNCLEAN_REVIVALS)
                .and_then(Value::as_u64)
                .unwrap_or(0);
            descriptor
                .rest
                .insert(UNCLEAN_REVIVALS.to_string(), json!(prior + 1));
            prior
        };
        let (message, turn) = match (revived_turn, prior) {
            (Some(RevivedTurn::Answered), _) => (
                "the daemon stopped as this session's turn was ending; its answer was already saved, so the session was rebuilt from its transcript and the turn was not started again",
                InterruptedTurn::Replay,
            ),
            (Some(RevivedTurn::Interrupted), 0) => (
                "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn continues from what was saved",
                InterruptedTurn::Replay,
            ),
            (Some(RevivedTurn::Unstarted) | None, 0) => (
                "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn was started again",
                InterruptedTurn::Replay,
            ),
            (Some(RevivedTurn::Interrupted | RevivedTurn::Unstarted) | None, _) => (
                "the daemon stopped again while this session's turn was running; the session was rebuilt from its transcript and the turn was not started again",
                InterruptedTurn::Settle,
            ),
        };
        self.record_session_crash(
            resident,
            &SessionCrash {
                kind: "daemon_restart".to_string(),
                message: message.to_string(),
                location: String::new(),
                thread: String::new(),
                at: util::now_iso(),
            },
            turn,
            reaped_programs,
        )
        .await;
    }

    /// A boot found this session idle: whatever stopped the daemon earlier,
    /// its turns have ended cleanly since.
    pub(super) async fn clear_host_death_count(&self, resident: &Arc<ResidentWorker>) {
        let mut descriptor = resident.descriptor.lock().await;
        if descriptor.rest.remove(UNCLEAN_REVIVALS).is_some() {
            if let Err(error) = persist_worker(&resident.descriptor_path, &descriptor) {
                self.log_line(&format!(
                    "session worker {}: its record could not be updated: {error:#}",
                    resident.worker_id
                ));
            }
        }
    }

    /// Make one session crash visible: append it to the crash log, stamp it
    /// on the session's descriptor (the roster's `lastCrash`), tell every
    /// attached client, and count it. The caller owns what happens next (the
    /// restart with backoff, or the give-up). `reaped_programs` (how many
    /// programs the session's commands left running were stopped after it
    /// went down) rides the record to the rebuilt session, which tells the
    /// model of them in the result of a call the crash cut off.
    pub(super) async fn record_session_crash(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        crash: &SessionCrash,
        turn: InterruptedTurn,
        reaped_programs: usize,
    ) {
        let (active_session_id, record, journal_path) = {
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
                "reapedPrograms": reaped_programs,
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
            (
                descriptor.root_active_session_id.clone(),
                record,
                std::path::PathBuf::from(&descriptor.recovery_journal_path),
            )
        };
        self.publish_fault_state(resident, "rebuilding").await;
        let log_path = self.descriptor_dir.join(SESSION_CRASH_LOG);
        let appended = tokio::task::spawn_blocking(move || {
            pa_core::agent_state::append_crash_record(&log_path, &record)
        })
        .await;
        match appended {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.log_line(&format!("session crash log append failed: {error:#}"));
            }
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
        // A turn that was running when the session went down will never emit
        // its own end: settle it for the attached clients with the reason.
        if turn == InterruptedTurn::Settle
            && crate::journal::WorkerRecoveryJournal::read_interrupted(&journal_path)
        {
            self.publish_session_fault(
                &active_session_id,
                json!({ "type": "turn_end", "error": crash.reason() }),
            );
            self.publish_session_fault(&active_session_id, json!({ "type": "agent_end" }));
            // The rebuilt session must not run that turn again: whatever in
            // it crashed the session would crash the rebuild too. The
            // lifetime that wrote the journal is gone, so this writer is its
            // only one.
            if let Err(error) = crate::journal::WorkerRecoveryJournal::drop_running_turns(
                &journal_path,
            )
            .and_then(|()| {
                crate::journal::WorkerRecoveryJournal::settle_busy_records(
                    &journal_path,
                    "session_crashed",
                )
            }) {
                self.log_line(&format!(
                    "session worker {}: the crashed turn could not be settled in its journal: {error:#}",
                    resident.worker_id
                ));
            }
        }
        if self.telemetry_recording_on() {
            if let Some(client) = &*self.telemetry.lock().unwrap() {
                pa_core::session_engine::telemetry::track_session_crashed(client, &crash.kind);
            }
        }
    }

    /// Stamp what became of the session since its last crash (`rebuilding`, `recovered`,
    /// `failed`) on the crash its record carries, and show it on the session's roster row:
    /// the agents view of every client then tells a session that is down, or was, from the
    /// rest, whether or not that client is attached to it.
    pub(super) async fn publish_fault_state(&self, resident: &Arc<ResidentWorker>, state: &str) {
        let (session_file, fault) = {
            let mut descriptor = resident.descriptor.lock().await;
            let Some(Value::Object(crash)) = descriptor.rest.get_mut("lastCrash") else {
                return;
            };
            crash.insert("state".to_string(), json!(state));
            let fault = Value::Object(crash.clone());
            if let Err(error) = persist_worker(&resident.descriptor_path, &descriptor) {
                self.log_line(&format!(
                    "session worker {}: its fault state could not be stamped on its record: {error:#}",
                    resident.worker_id
                ));
            }
            (descriptor.session_file.clone(), fault)
        };
        let Some(session_file) = session_file else {
            return;
        };
        let changed = self
            .roster
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_session_fault(&session_file, fault);
        if let Some(entry) = changed {
            self.push_roster_update(vec![entry], Vec::new());
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
