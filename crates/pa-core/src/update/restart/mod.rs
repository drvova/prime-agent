//! The daemon side of the update restart (`docs/legacy-upgrade-contract.md`,
//! "Daemon and TUI handoff"), shared by both daemons: the supervisor serves it
//! on the old wire (`prepare_update_restart`, `commit_update_restart`), the
//! runtime daemon as wire2 calls of the same names.
//!
//! [`UpdateRestart`] is one daemon's update transaction: the prepare
//! (`Draining -> Fenced -> Snapshotted -> Prepared`, [`prepare`]), the
//! admission gate and mutation drain in front of every other command, the
//! commit (`Prepared -> Stopping`), its abandon, and the watchdog that aborts
//! an expired transaction. The daemon supplies what only it knows: the
//! snapshot of its sessions as an [`UpdateRoster`] (assembled with
//! [`roster`]), and the stop that follows a commit. The replies and refusals
//! are the old supervisor's, verbatim.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pa_types::daemon::{
    DaemonErrorInfo, UpdateId, UpdatePreparedMarker, UpdateRoster, UpdateTimeoutBudget,
    UPDATE_RESTART_PREPARING_MESSAGE,
};
use serde_json::{json, Map, Value};

use crate::session::manager::iso_from_unix_ms;

mod prepare;
pub mod roster;

use prepare::{marker_expires_at_iso, update_gate_refuses, write_prepared_artifacts};
pub use prepare::{
    now_ms, AbortOutcome, AbortReason, BeginOutcome, MutationDrainLatch, PrepareCoordinator,
    PrepareOp, PrepareState,
};

/// What the drain refuses with when its mutations do not drain in budget (TS
/// verbatim); the watchdog's abort mid-drain answers the same.
const DRAIN_TIMEOUT_MESSAGE: &str = "Timed out draining daemon mutations for update restart";

/// A refused update call: the old supervisor's message and `errorInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateRefusal {
    pub message: String,
    pub info: Option<DaemonErrorInfo>,
}

impl UpdateRefusal {
    fn plain(message: impl Into<String>) -> Self {
        UpdateRefusal {
            message: message.into(),
            info: None,
        }
    }
}

impl std::fmt::Display for UpdateRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpdateRefusal {}

/// A mutating command in flight: the prepare's drain waits until every
/// guard is dropped.
pub struct MutationGuard(MutationDrainLatch);

impl Drop for MutationGuard {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// One daemon's update transaction (see the module documentation).
pub struct UpdateRestart {
    prepare: PrepareCoordinator,
    drain: MutationDrainLatch,
    budget: UpdateTimeoutBudget,
    agent_dir: PathBuf,
    socket_path: PathBuf,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl UpdateRestart {
    /// The transaction of the daemon serving `socket_path` over `agent_dir`
    /// (its prepared artifacts go under the socket's update dir), with the
    /// update `budget`; `log` takes the daemon log's lines.
    #[must_use]
    pub fn new(
        agent_dir: PathBuf,
        socket_path: PathBuf,
        budget: UpdateTimeoutBudget,
        log: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        UpdateRestart {
            prepare: PrepareCoordinator::new(),
            drain: MutationDrainLatch::new(),
            budget,
            agent_dir,
            socket_path,
            log,
        }
    }

    /// The update budget the transaction runs on.
    #[must_use]
    pub fn budget(&self) -> &UpdateTimeoutBudget {
        &self.budget
    }

    /// The socket of the daemon the transaction is for.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The transaction's state machine.
    #[must_use]
    pub fn coordinator(&self) -> &PrepareCoordinator {
        &self.prepare
    }

    /// The prepared-artifact directory of one update under this socket's
    /// scratch dir (swept at boot; created only by the prepare).
    #[must_use]
    pub fn prepared_dir(&self, update_id: &UpdateId) -> PathBuf {
        prepare::prepared_dir(&self.agent_dir, &socket_hash(&self.socket_path), update_id)
    }

    /// The gate in front of command `command_type` (an old wire command type;
    /// the update calls themselves never pass through it): an expired
    /// transaction aborts first (spec §5), then a mutating command is refused
    /// while a transaction is active (the drain commands pass while
    /// `Draining`), and an admitted mutating command counts against the drain
    /// until its guard drops.
    ///
    /// # Errors
    ///
    /// The command is refused: [`UPDATE_RESTART_PREPARING_MESSAGE`] with the
    /// typed `update_restarting` info.
    pub fn admit(&self, command_type: &str) -> Result<Option<MutationGuard>, UpdateRefusal> {
        self.expire();
        if let Some(state) = self.prepare.active_state() {
            if update_gate_refuses(state, command_type) {
                return Err(UpdateRefusal {
                    message: UPDATE_RESTART_PREPARING_MESSAGE.to_string(),
                    // Beside the plain message, so clients can recognize the
                    // transient state and wait through it.
                    info: Some(DaemonErrorInfo::UpdateRestarting),
                });
            }
        }
        if !pa_types::daemon::is_daemon_mutating_command(command_type) {
            return Ok(None);
        }
        self.drain.begin();
        Ok(Some(MutationGuard(self.drain.clone())))
    }

    /// `prepare_update_restart {updateId}`: a new id starts the transaction,
    /// waits for the mutation drain, has `snapshot` collect the daemon's
    /// roster (by the deadline it is given, epoch ms), writes the prepared
    /// artifacts durably and answers `prepared`; a repeat answers the current
    /// state; another id is refused. Any failure aborts: nothing is left half
    /// prepared.
    ///
    /// # Errors
    ///
    /// The old supervisor's refusals: no id, another update active, the drain
    /// or the snapshot failed or timed out.
    pub async fn prepare<F, Fut>(
        &self,
        update_id: Option<String>,
        snapshot: F,
    ) -> Result<Value, UpdateRefusal>
    where
        F: FnOnce(UpdateId, u64) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<UpdateRoster>>,
    {
        self.expire();
        let Some(update_id) = update_id.map(UpdateId::from) else {
            return Err(UpdateRefusal::plain(
                "prepare_update_restart requires an updateId",
            ));
        };
        let now = now_ms();
        let (accepted_at_ms, prepare_deadline_ms) =
            match self.prepare.begin(update_id.clone(), now, &self.budget) {
                BeginOutcome::Refused { active_update_id } => {
                    return Err(UpdateRefusal {
                        message: "Daemon is already preparing an update restart".to_string(),
                        info: Some(DaemonErrorInfo::UpdatePrepareRefused {
                            active_update_id: active_update_id.0,
                        }),
                    });
                }
                BeginOutcome::AlreadyActive {
                    state,
                    accepted_at_ms,
                } => {
                    return Ok(json!({
                        "updateId": update_id,
                        "state": state.wire_name(),
                        "acceptedAt": iso_from_unix_ms(accepted_at_ms),
                    }));
                }
                BeginOutcome::Started {
                    accepted_at_ms,
                    prepare_deadline_ms,
                } => (accepted_at_ms, prepare_deadline_ms),
            };
        // Draining: wait for in-flight mutations within the hard deadline.
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(prepare_deadline_ms.saturating_sub(now));
        if let Err(error) = self.drain.wait_for_drain(0, deadline).await {
            self.abort(&update_id);
            return Err(UpdateRefusal::plain(error.to_string()));
        }
        match self.prepare.drain_complete(&update_id) {
            PrepareOp::Applied(_) => {}
            // The watchdog aborted the transaction while it drained (the
            // same deadline): the daemon is serving again.
            PrepareOp::NotActive => return Err(UpdateRefusal::plain(DRAIN_TIMEOUT_MESSAGE)),
        }
        // Fenced: snapshot, write the prepared artifacts (fsync), and reach
        // Prepared inside the remaining budget (spec §5). Rollback is the
        // default on any failure: abort, delete the artifacts, Serving.
        let prepared = async {
            let roster = snapshot(update_id.clone(), prepare_deadline_ms).await?;
            let now = now_ms();
            let marker = UpdatePreparedMarker {
                update_id: update_id.clone(),
                expires_at: marker_expires_at_iso(now, &self.budget),
                supervisor: roster.supervisor.clone(),
                rest: Map::default(),
            };
            write_prepared_artifacts(&self.prepared_dir(&update_id), &roster, &marker)?;
            match self.prepare.snapshot_written(&update_id, now, &self.budget) {
                PrepareOp::Applied(_) => {}
                PrepareOp::NotActive => anyhow::bail!("update prepare aborted during the snapshot"),
            }
            match self.prepare.prepare_acked(&update_id) {
                PrepareOp::Applied(_) => {}
                PrepareOp::NotActive => anyhow::bail!("update prepare aborted before the ack"),
            }
            anyhow::Ok(marker.expires_at)
        }
        .await;
        match prepared {
            Ok(expires_at) => Ok(json!({
                "updateId": update_id,
                "state": "prepared",
                "acceptedAt": iso_from_unix_ms(accepted_at_ms),
                "expiresAt": expires_at,
            })),
            Err(error) => {
                self.abort(&update_id);
                Err(UpdateRefusal::plain(format!("{error:#}")))
            }
        }
    }

    /// `commit_update_restart {updateId}` (spec §5 `Prepared -> Stopping`):
    /// the only consumption of the prepared artifact; idempotent while
    /// stopping. The daemon stops its sessions next, and either exits for the
    /// update ([`Self::closing_payload`], [`Self::commit_reply`]) or abandons
    /// ([`Self::abandon`]).
    ///
    /// # Errors
    ///
    /// No id, or no prepared update with that id.
    pub fn commit(&self, update_id: Option<String>) -> Result<UpdateId, UpdateRefusal> {
        self.expire();
        let Some(update_id) = update_id.map(UpdateId::from) else {
            return Err(UpdateRefusal::plain(
                "commit_update_restart requires an updateId",
            ));
        };
        match self.prepare.commit(&update_id) {
            PrepareOp::Applied(_) => Ok(update_id),
            PrepareOp::NotActive => Err(UpdateRefusal::plain(
                "No prepared update restart is active for that update id",
            )),
        }
    }

    /// The commit's answer once every session stopped: `stopped` of them.
    #[must_use]
    pub fn commit_reply(update_id: &UpdateId, stopped: usize) -> Value {
        json!({ "updateId": update_id, "state": "stopping", "stopped": stopped })
    }

    /// What every client is told before the sockets close for the update
    /// (spec §10.1, the `daemon_closing` frame without its `type`): the
    /// close is an instruction to reattach by durable id after the restart,
    /// not an error. `sessions` are the stopped sessions' `{sessionId,
    /// name}`.
    #[must_use]
    pub fn closing_payload(&self, update_id: &UpdateId, sessions: &[Value]) -> Value {
        let est_seconds = (self.budget.boot_ms + self.budget.restore_overall_ms) / 1000;
        json!({
            "reason": "update",
            "payload": {
                "updateId": update_id.to_string(),
                "resume": true,
                "estSeconds": est_seconds,
                "sessions": sessions,
            },
        })
    }

    /// The stop missed its budget (spec §5 `Stopping -> Aborted`): the
    /// transaction is abandoned and its artifacts deleted; the daemon keeps
    /// serving.
    pub fn abandon(&self, update_id: &UpdateId) {
        if let Some(abort) = self.prepare.abandon_stopping(update_id) {
            self.finish_abort(&abort);
        }
    }

    /// Whether a committed update is stopping the daemon.
    #[must_use]
    pub fn stopping(&self) -> bool {
        self.prepare.active_state() == Some(PrepareState::Stopping)
    }

    /// The watchdog (spec §5): aborts a transaction whose deadline or marker
    /// self-expiry passes even when no command arrives to re-check, so a
    /// coordinator that dies mid-prepare never wedges the daemon (invariant
    /// I1). Parks with no timer while no update is in flight; runs until
    /// dropped.
    pub async fn watchdog(&self) {
        loop {
            let abort = self.prepare.wait_for_expiry().await;
            self.finish_abort(&abort);
        }
    }

    /// Aborts an active transaction whose budget expired (spec §5: checked
    /// in front of every command, beside the watchdog).
    fn expire(&self) {
        if let Some(abort) = self.prepare.abort_if_expired(now_ms()) {
            self.finish_abort(&abort);
        }
    }

    /// The driver's own abort of its prepare phases.
    fn abort(&self, update_id: &UpdateId) {
        if let Some(abort) = self.prepare.abort(update_id) {
            self.finish_abort(&abort);
        }
    }

    /// Applies one abort: its prepared artifacts deleted, if any, and the
    /// return to `Serving` logged.
    fn finish_abort(&self, abort: &AbortOutcome) {
        if abort.delete_prepared {
            let prepared_dir = self.prepared_dir(&abort.update_id);
            if let Err(error) = prepare::delete_prepared_dir(&prepared_dir) {
                (self.log)(&format!(
                    "update prepare cleanup failed for {}: {error:#}",
                    abort.update_id
                ));
            }
        }
        (self.log)(&format!(
            "update prepare aborted ({}): {}",
            abort.update_id,
            abort.reason.as_str()
        ));
    }
}

/// The sha256 hex of a daemon socket's path: the key of its update dir and
/// of the coordinator's intent and status files.
#[must_use]
pub fn socket_hash(socket_path: &Path) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(socket_path.to_string_lossy().as_bytes())
        .iter()
        .fold(String::new(), |mut key, byte| {
            use std::fmt::Write;
            write!(key, "{byte:02x}").expect("write to a String");
            key
        })
}
