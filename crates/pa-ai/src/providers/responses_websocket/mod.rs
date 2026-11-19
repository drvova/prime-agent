//! The Responses WebSocket mode shared by every provider that speaks it (the `ChatGPT` Codex
//! backend and the Responses API): one persistent connection per session, `response.create`
//! requests that continue the connection's previous response with only the new input items, the
//! SSE fallback bookkeeping, the per-request transport choice, and the learned per-endpoint
//! capability. Providers keep their own URL, headers, and event mapping.

mod capability;
mod connection;
mod continuation;
pub(crate) mod errors;
mod plan;
mod sessions;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub use capability::set_capability_cache_path;
pub(crate) use capability::{endpoint_key, learned_capability, record_learned_capability};
pub(crate) use connection::{AcquiredConnection, WireProfile, WorkerEvent};
pub(crate) use continuation::{build_cached_websocket_request_body, ContinuationState};
pub(crate) use plan::{
    is_visible_response_event, note_dispatch, note_plan, plan_transport, transport_debug_name,
    Capability, PlanInput, TransportPlan,
};
#[cfg(test)]
pub(crate) use sessions::SessionState;
pub use sessions::WebSocketDebugStats;
pub(crate) use sessions::WebSocketSessions;

/// Per-session Responses transport counts for the adoption telemetry (primitives only: never an
/// endpoint, a body, or an id).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponsesTransportCounts {
    /// `response.create` requests sent over a WebSocket.
    pub websocket_requests: u64,
    /// WebSocket requests that continued the previous response with only the new input.
    pub delta_requests: u64,
    /// WebSocket requests that sent the full context.
    pub full_context_requests: u64,
    /// Requests served over SSE after the session's WebSocket failed.
    pub sse_fallbacks: u64,
    /// Full-context resends after the server no longer held the previous response.
    pub stale_continuation_retries: u64,
    /// `auto` requests that chose the WebSocket.
    pub auto_websocket: u64,
    /// `auto` requests that chose SSE because nothing can continue them (no session).
    pub auto_sse_one_shot: u64,
    /// `auto` requests on a provider whose API has no WebSocket mode.
    pub auto_sse_no_websocket_wire: u64,
    /// `auto` requests on an endpoint declared or learned not to speak the WebSocket mode.
    pub auto_sse_unsupported: u64,
    /// `auto` requests on a session demoted to SSE after WebSocket failures.
    pub auto_sse_demoted: u64,
    /// Requests that probed an endpoint whose WebSocket capability was unknown.
    pub probes: u64,
    /// Probes that found no WebSocket mode.
    pub probe_failures: u64,
    /// Requests that asked for a WebSocket on a provider whose API has no WebSocket mode.
    pub websocket_requested_without_wire: u64,
}

fn counts_state() -> &'static Mutex<HashMap<String, ResponsesTransportCounts>> {
    static COUNTS: OnceLock<Mutex<HashMap<String, ResponsesTransportCounts>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Count into one session's transport counts (fire-and-forget: a poisoned lock drops the count).
pub(crate) fn note_counts(session_id: &str, update: impl FnOnce(&mut ResponsesTransportCounts)) {
    if let Ok(mut counts) = counts_state().lock() {
        update(counts.entry(session_id.to_string()).or_default());
    }
}

/// The session's Responses transport counts so far; `None` when it sent no counted request.
#[must_use]
pub fn session_counts(session_id: &str) -> Option<ResponsesTransportCounts> {
    counts_state().lock().ok()?.get(session_id).copied()
}
