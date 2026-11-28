//! The update/restart flow on the supervisor: its residents' snapshot for
//! pa-core's prepare transaction (`pa_core::update::restart`), the graceful
//! worker stop of the commit, exit-for-update, and the attach-stream salvage
//! helpers.
use pa_core::update::restart::roster::{process_identity, RosterInputs};
use pa_core::update::restart::{now_ms, UpdateRestart};
use pa_types::daemon::UpdateRoster;

use super::{
    anyhow, app_version, build_update_roster, join_all, json, response_failure, response_line,
    response_success, stop_workers_gracefully, stream_attach, Arc, ClientRouting, DaemonCommand,
    DaemonResponse, Duration, Ordering, Path, Result, RouteAdmission, SnapshotPurpose, Supervisor,
    UpdateId, Value, WorkerSnapshot, WorkerStopVerdict, WorkerTransition,
    WORKER_REQUEST_TIMEOUT_MS,
};

pub(super) fn streamed_attach_lines(
    mut response: DaemonResponse,
    active_session_id: &str,
    purpose: SnapshotPurpose,
) -> (Vec<Value>, bool) {
    let Some(data) = response.data.take() else {
        return (vec![response_line(&response)], false);
    };
    match stream_attach(data, active_session_id, purpose) {
        Ok((streamed, events)) => {
            response.data = Some(streamed);
            let mut lines = vec![response_line(&response)];
            lines.extend(events.lines());
            (lines, false)
        }
        // The snapshot could not even be identified: the attach itself
        // fails, before any snapshot record exists on the wire.
        Err(error) => (
            vec![response_line(&response_failure(
                response.id.as_deref(),
                &response.command,
                &error.to_string(),
                None,
            ))],
            false,
        ),
    }
}

/// The request id of an unparsable line, so the failure stays matchable by
/// the client (TS `salvageDaemonCommandId`).
pub(super) fn salvage_id(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
}

/// The command type of an unparsable line, salvaged for the failure echo
/// (bare commands carry the type at the top level; envelopes nest it).
pub(super) fn salvage_command_type(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    value
        .get("command")
        .unwrap_or(&value)
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl Supervisor {
    /// `prepare_update_restart`: pa-core's update transaction
    /// (`UpdateRestart::prepare`) over this supervisor's residents — a new
    /// `updateId` starts it, waits for the mutation drain, snapshots every
    /// resident and reports `prepared`; a repeat reports the current state; a
    /// different id is refused. Any failure aborts — nothing left half-prepared.
    pub(super) async fn handle_prepare_update_restart(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        command: &DaemonCommand,
    ) -> DaemonResponse {
        let DaemonCommand::PrepareUpdateRestart {
            update_id, rest, ..
        } = command
        else {
            return response_failure(
                Some(command_id),
                type_name,
                "not a prepare_update_restart command",
                None,
            );
        };
        let to_version = rest
            .get("toVersion")
            .and_then(Value::as_str)
            .unwrap_or(app_version());
        match self
            .update
            .prepare(update_id.clone(), |update_id, prepare_deadline_ms| {
                self.snapshot_for_update(update_id, to_version, prepare_deadline_ms)
            })
            .await
        {
            Ok(data) => response_success(Some(command_id), type_name, Some(data)),
            Err(refusal) => {
                response_failure(Some(command_id), type_name, &refusal.message, refusal.info)
            }
        }
    }

    /// The prepare transaction's snapshot (spec §5 `Fenced`): every
    /// resident's `update_snapshot`, assembled into the roster (spec §8).
    async fn snapshot_for_update(
        self: &Arc<Self>,
        update_id: UpdateId,
        to_version: &str,
        prepare_deadline_ms: u64,
    ) -> Result<UpdateRoster> {
        let residents = self.registry.list().await;
        // TS parity: refuse to snapshot over a worker that is stopping or
        // disconnected - its state is not collectible.
        for resident in &residents {
            let state = if self.is_stopping(resident) {
                "stopping"
            } else {
                "disconnected"
            };
            let connected = resident.route_state().connected;
            if self.is_stopping(resident) || !connected {
                // TS #2515: the refusal names the blocking session (`sessionFile`, else the
                // root active session id).
                let descriptor = resident.descriptor.lock().await;
                let session = descriptor
                    .session_file
                    .clone()
                    .unwrap_or_else(|| descriptor.root_active_session_id.clone());
                anyhow::bail!(
                    "{}",
                    update_prepare_resident_refusal(&resident.worker_id, state, &session)
                );
            }
        }
        let remaining = prepare_deadline_ms.saturating_sub(now_ms());
        let rpc_timeout = WORKER_REQUEST_TIMEOUT_MS.min(remaining).max(1);
        let snapshots = join_all(residents.iter().map(|resident| {
            let resident = Arc::clone(resident);
            async move {
                let response = self
                    .route_command_typed(
                        &resident,
                        "update_snapshot",
                        json!({}),
                        rpc_timeout,
                        RouteAdmission::SupervisorInternal,
                    )
                    .await?;
                if !response.success {
                    anyhow::bail!(
                        "worker {} refused its snapshot: {}",
                        resident.worker_id,
                        response.error.unwrap_or_default()
                    );
                }
                let data = response
                    .data
                    .clone()
                    .ok_or_else(|| anyhow!("worker {} returned no snapshot", resident.worker_id))?;
                let descriptor = resident.descriptor.lock().await.clone();
                Ok(WorkerSnapshot {
                    worker_id: resident.worker_id.clone(),
                    descriptor,
                    snapshot: data,
                })
            }
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<WorkerSnapshot>>>()?;
        let ledger = self.rlm_spawn_ledger_for(None).await?;
        build_update_roster(
            RosterInputs {
                update_id: &update_id,
                socket_path: self.options.socket_path.to_str().unwrap_or_default(),
                agent_dir: &self.options.agent_dir,
                supervisor: process_identity(format!("sup:{}", std::process::id())),
                from_version: app_version(),
                to_version,
                created_at_ms: now_ms(),
                ledger: &ledger,
            },
            &snapshots,
        )
    }

    /// `commit_update_restart` (spec §5 `Prepared -> Stopping`): stop every worker gracefully
    /// in budget; all stopped — the supervisor exits for the update (descriptors survive
    /// for the successor). Any refusal ABANDONS: back to `Serving`, refused sessions keep
    /// running, already-stopped workers relaunch (invariant I3 - never a kill).
    pub(super) async fn handle_commit_update_restart(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        command: &DaemonCommand,
    ) -> (Vec<Value>, bool) {
        let DaemonCommand::CommitUpdateRestart { update_id, .. } = command else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "not a commit_update_restart command",
                    None,
                ))],
                false,
            );
        };
        let update_id = match self.update.commit(update_id.clone()) {
            Ok(update_id) => update_id,
            Err(refusal) => {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &refusal.message,
                        refusal.info,
                    ))],
                    false,
                );
            }
        };
        let residents = self.registry.list().await;
        let verdicts = stop_workers_gracefully(self, &residents, self.update.budget()).await;
        let refused: Vec<&str> = verdicts
            .iter()
            .filter(|(_, verdict)| *verdict == WorkerStopVerdict::Refused)
            .map(|(worker_id, _)| worker_id.as_str())
            .collect();
        if !refused.is_empty() {
            // Abandon (I3): the sessions that refused keep running; the ones
            // that already stopped relaunch over their own session files.
            self.update.abandon(&update_id);
            for (resident, (_, verdict)) in residents.iter().zip(&verdicts) {
                match verdict {
                    WorkerStopVerdict::Stopped => {
                        // The old monitor may only now observe its child's
                        // exit. Retire that watch before reopening
                        // supervision: the bump lands before the stop flag is
                        // cleared, so a watch woken at the exit reads either
                        // the intentional stop or the new generation.
                        resident.monitor_epoch.fetch_add(1, Ordering::SeqCst);
                        resident.transition(WorkerTransition::ResumeStopped);
                        match self.relaunch_supervised(resident).await {
                            Ok(()) => {
                                resident.consecutive_failures.store(0, Ordering::SeqCst);
                            }
                            Err(error) => {
                                self.log_line(&format!(
                                    "worker {} abandon relaunch failed: {error:#}",
                                    resident.worker_id
                                ));
                            }
                        }
                    }
                    WorkerStopVerdict::Refused => {
                        // Restore normal supervision: the stop request may still land late,
                        // in which case the monitor treats the exit as a crash and
                        // relaunches with backoff (the session file is the truth).
                        resident.transition(WorkerTransition::ResumeStopped);
                    }
                }
            }
            self.log_line(&format!(
                "update {update_id} abandoned: worker(s) {refused:?} did not stop in budget; sessions untouched"
            ));
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    &format!(
                        "Update abandoned: session worker(s) {refused:?} did not stop within the budget; sessions are untouched and the daemon keeps serving"
                    ),
                    None,
                ))],
                false,
            );
        }
        let stopped = verdicts.len();
        self.log_line(&format!(
            "update {update_id}: all {stopped} worker(s) stopped; exiting for the update"
        ));
        // Spec §10.1: every client learns the update resume contract BEFORE the
        // sockets close.
        let mut sessions: Vec<Value> = Vec::new();
        for resident in &residents {
            let descriptor = resident.descriptor.lock().await;
            let session_id = descriptor
                .root_session_id
                .clone()
                .or_else(|| {
                    descriptor.session_file.as_deref().and_then(|file| {
                        Path::new(file)
                            .file_stem()
                            .map(|stem| stem.to_string_lossy().to_string())
                    })
                })
                .unwrap_or_else(|| resident.worker_id.clone());
            sessions.push(json!({
                "sessionId": session_id,
                "name": descriptor
                    .create_command
                    .rest
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            }));
        }
        let mut closing = self.update.closing_payload(&update_id, &sessions);
        closing["type"] = json!("daemon_closing");
        let _ = self
            .events
            .send((ClientRouting::Broadcast, std::sync::Arc::new(closing)));
        // The response is written before the accept loop exits; the 100ms drain only
        // orders the exit behind it — the coordinator's Booting phase recovers a lost
        // ack by design.
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            supervisor.exit_for_update();
        });
        (
            vec![response_line(&response_success(
                Some(command_id),
                type_name,
                Some(UpdateRestart::commit_reply(&update_id, stopped)),
            ))],
            true,
        )
    }

    /// The update's exit path (spec §5 Stopping): set the shutdown flag, but KEEP the worker
    /// descriptors on disk — the successor's restore relaunches from them. Contrast
    /// `begin_shutdown`, which deletes descriptors for a terminal stop.
    fn exit_for_update(self: &Arc<Self>) {
        // The successor is started by the update, with its own restart owner: this daemon's
        // owner must not answer the handoff with a restart of the old binary.
        self.note_deliberate_stop();
        // Publish the accept-loop exit before the general shutdown gate, so a client
        // disconnect never observes the transient `shutting_down && !accept_exit` window
        // and mistakes the update restart for a terminal stop pass.
        self.accept_exit.store(true, Ordering::SeqCst);
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_one();
    }
}

/// TS #2515 `prepareUpdateRestartFenced`'s refusal: the message names the blocking
/// session, so the refused prepare ties to a specific session.
fn update_prepare_resident_refusal(worker_id: &str, state: &str, session: &str) -> String {
    format!(
        "Cannot prepare update restart while resident worker {worker_id} is {state} (session {session})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_prepare_refusal_names_the_blocking_session() {
        assert_eq!(
            update_prepare_resident_refusal(
                "resident-1",
                "disconnected",
                "/sessions/blocked.jsonl"
            ),
            "Cannot prepare update restart while resident worker resident-1 is disconnected (session /sessions/blocked.jsonl)"
        );
        // The root active session id is the fallback (a worker whose
        // descriptor carries no session file yet).
        assert_eq!(
            update_prepare_resident_refusal("resident-1", "stopping", "blocked-root"),
            "Cannot prepare update restart while resident worker resident-1 is stopping (session blocked-root)"
        );
    }
}
