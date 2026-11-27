//! The daemon's own end, asked for by a client: `daemon.shutdown` and
//! `daemon.restart`, the old supervisor's `shutdown` and `restart`, which
//! end it the same way (the client that asked for a restart starts the
//! next daemon); an update's commit (`handlers/update.rs`) ends it the same
//! way too, its `system/closing` carrying the update's payload.
//!
//! The call is answered first, then the stop is requested
//! (`crate::wire::after_reply`), so the client that asked reads its reply
//! before the `system/closing` every connection is told the end with. From
//! the call on, every query and call is refused with the old supervisor's
//! [`SHUTTING_DOWN`]. The stop itself is the daemon's drain (`super::super::run`):
//! the keeper's stop intent, `system/closing`, [`RuntimeHandlers::cut_off_turns`],
//! the connections closed, the sessions closed, a clean exit. A `force`
//! argument is accepted and, as on the old supervisor, changes nothing: a
//! client that wants the daemon gone whatever it does kills it after.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;
use pa_types::event_log::{AgentInput, Priority};
use serde_json::Value;
use tokio::sync::Notify;

use super::RuntimeHandlers;
use crate::session::SessionBuilder;
use crate::wire::ConnectionId;

/// What every query and call is refused with once the daemon is stopping
/// (the old supervisor's refusal, verbatim).
pub(in crate::daemon) const SHUTTING_DOWN: &str = "Supervisor is shutting down";

/// The hold the drain keeps on every session's lane, so nothing queued
/// starts while the daemon stops; the held tasks stay queued for the next
/// daemon.
const DRAIN_HOLD: &str = "daemon-stop";

/// The daemon's deliberate end: requested by `daemon.shutdown` or
/// `daemon.restart` once its reply is queued, or begun by the embedder's
/// stop signal.
#[derive(Default)]
pub(in crate::daemon) struct DaemonStop {
    requested: Notify,
    stopping: AtomicBool,
    /// What `system/closing` says when it is not a plain shutdown: an
    /// update's commit sets its `daemon_closing` payload.
    closing: std::sync::Mutex<Option<Value>>,
}

impl DaemonStop {
    /// Resolves once a client asked the daemon to stop.
    pub(in crate::daemon) async fn requested(&self) {
        self.requested.notified().await;
    }

    /// The daemon is stopping, whoever asked: every query and call is
    /// refused from now on.
    pub(in crate::daemon) fn begin(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether the daemon is stopping.
    pub(in crate::daemon) fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// What the drain's `system/closing` carries: the old `daemon_closing`
    /// frame without its `type`.
    pub(in crate::daemon) fn closing(&self) -> Value {
        crate::lock(&self.closing)
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "reason": "shutdown" }))
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `daemon.shutdown` and `daemon.restart` (`call` names which): the
    /// stop is begun now and requested once the reply is queued; the reply
    /// is the old supervisor's, without `data`.
    pub(super) fn stop_daemon(
        &self,
        args: &Value,
        call: &str,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        match args.get("force") {
            None | Some(Value::Null | Value::Bool(_)) => {}
            Some(_) => anyhow::bail!("the `force` argument must be a boolean"),
        }
        // Who asked lands in the daemon's log the moment the stop is begun;
        // a client name carrying a line break must not forge another line.
        let client = self
            .attached
            .client_of(connection)
            .replace(['\n', '\r'], " ");
        tracing::info!("{call} requested by client {client}");
        self.request_stop(serde_json::json!({ "reason": "shutdown" }));
        Ok(Value::Null)
    }

    /// Begins the stop now, `system/closing` to carry `closing`, and
    /// requests it once the reply being answered is queued.
    pub(super) fn request_stop(&self, closing: Value) {
        *crate::lock(&self.stop.closing) = Some(closing);
        self.stop.begin();
        let stop = Arc::clone(&self.stop);
        crate::wire::after_reply(move || stop.requested.notify_one());
    }

    /// The drain's cut: every open session's lane is held, so nothing queued
    /// starts; a running turn is aborted, and, in a session that keeps a
    /// transcript, [`UPDATE_RESTART_CONTINUATION_PROMPT`] is queued at the
    /// front of its lane for the next daemon, which revives the session for
    /// it (the old worker's daemon-wide `shutdown`). An ephemeral session's
    /// turn is aborted and nothing is queued: no daemon revives it.
    pub(in crate::daemon) async fn cut_off_turns(&self) {
        for id in self.open.paths().into_keys() {
            self.runtime.hold_input(&id, DRAIN_HOLD);
            if self.runtime.lane(&id).running.is_none() {
                continue;
            }
            if let Err(error) = self.runtime.interrupt(&id).await {
                tracing::warn!("session {id}'s turn was not cut off cleanly: {error:#}");
            }
            if self.open.ephemeral(&id) {
                continue;
            }
            let continuation = AgentInput {
                text: UPDATE_RESTART_CONTINUATION_PROMPT.to_string(),
                images: Vec::new(),
                priority: Some(Priority::Steer),
            };
            if let Err(error) = self.publish_input(&id, continuation).await {
                tracing::warn!("session {id}'s continuation was not queued: {error:#}");
            }
        }
    }
}
