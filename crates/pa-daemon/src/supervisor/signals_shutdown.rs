//! Shutdown and signal handling: the drain arms, the shutdown entry, and the
//! daemon-closing shutdown event.
use super::{
    json, Arc, Ordering, RouteAdmission, Supervisor, Value, WorkerTransition, ROUTE_TIMEOUT_MS,
};
// The only use is the broadcast inside the unix begin_signal_drain arm.
#[cfg(unix)]
use super::ClientRouting;

/// The non-update `daemon_closing` frame: every connected client learns the daemon is
/// going down for a shutdown.
pub(super) fn daemon_closing_shutdown_event() -> Value {
    json!({ "type": "daemon_closing", "reason": "shutdown" })
}

impl Supervisor {
    /// Run the one terminal stop pass, whichever connection first reaches it: the stop
    /// must still start even if the initiating client disconnects. `shutdown_started`
    /// is the one-owner gate.
    pub(super) async fn ensure_shutdown_started(self: &Arc<Self>) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.begin_shutdown().await;
    }

    /// Tell this daemon's restart owner, when it has one, that the stop now starting is
    /// deliberate: it must not be answered with a restart even if it ends in a kill (a forced
    /// shutdown's escalation, a second signal). An intent that cannot be written is logged; the
    /// exit status of a stop that completes says the same thing.
    pub(super) fn note_deliberate_stop(&self) {
        if pa_runtime::daemon::keeper::keeper_pid().is_none() {
            return;
        }
        if let Err(error) = pa_runtime::daemon::keeper::record_stop_intent(
            &self.options.agent_dir,
            &self.options.socket_path,
        ) {
            self.log_line(&format!(
                "the stop intent for the restart owner could not be recorded: {error:#}"
            ));
        }
    }

    pub(super) async fn begin_shutdown(self: &Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.note_deliberate_stop();
        for resident in self.registry.list().await {
            resident.transition(WorkerTransition::RetireStopped);
            // The stop tombstone persists before the worker is even told (TS
            // `stopWorkerUntracked`): a supervisor that dies between here and the worker's
            // exit leaves durable stop intent, and the next boot finishes the stop. The
            // tombstone is the resumable stop variant: a lost step after it keeps the
            // session instead of archiving it.
            if self.persist_stop_tombstone_stop(&resident).await.is_err() {
                self.log_line(&format!(
                    "session worker {} stop tombstone could not persist; leaving the worker untouched (the next boot retries the stop)",
                    resident.worker_id
                ));
                continue;
            }
            let _ = self
                .route_command_typed(
                    &resident,
                    "shutdown",
                    json!({ "daemonShutdown": true }),
                    ROUTE_TIMEOUT_MS,
                    RouteAdmission::SupervisorInternal,
                )
                .await;
            self.retire_worker_after_stop(&resident, true).await;
        }
        self.registry.clear().await;
        // The workers are all stopped now, so the accept loop may exit; the gate alone
        // is not enough — an inbound connection could fall the loop out mid-stop.
        self.accept_exit.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_one();
    }

    /// The OS-signal drain step (the loop in `crate::signal_drain` runs this once per
    /// signal): the first signal enters the graceful drain; any later signal, or one
    /// that finds a shutdown or update exit already committed, force-exits. Returns
    /// `true` when this call started the drain; `false` when one was already in flight.
    /// A signal that finds `Stopping` or `accept_exit` published never flips the gate.
    #[cfg(unix)]
    pub(crate) fn begin_signal_drain(self: &Arc<Self>) -> bool {
        if self.update.stopping()
            || self.accept_exit.load(Ordering::SeqCst)
            || self.shutting_down.swap(true, Ordering::SeqCst)
        {
            return false;
        }
        self.note_deliberate_stop();
        self.log_line(
            "received shutdown signal; entering graceful drain: new client commands refused, running turns settle through the workers' routed shutdown",
        );
        let _ = self.events.send((
            ClientRouting::Broadcast,
            std::sync::Arc::new(daemon_closing_shutdown_event()),
        ));
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            supervisor.ensure_shutdown_started().await;
        });
        true
    }
}
