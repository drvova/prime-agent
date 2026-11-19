//! Codex WebSocket session state: the Codex instance of the shared Responses WebSocket sessions
//! (connection cache, SSE-fallback pinning, debug stats, continuation bookkeeping, idle expiry).

use serde_json::Value;

use crate::providers::openai_codex_responses::errors::CodexStreamError;
use crate::providers::responses_websocket::{ContinuationState, WebSocketSessions};

pub use crate::providers::responses_websocket::WebSocketDebugStats;

/// The Codex provider's sessions. Codex connections live until idle expiry (no maximum age).
pub(crate) static CODEX_SESSIONS: WebSocketSessions =
    WebSocketSessions::new(super::websocket::CODEX_WIRE, None);

/// The Codex session state (the transport tests inspect a session's cached connection).
#[cfg(test)]
pub fn session_state(
) -> &'static std::sync::Mutex<crate::providers::responses_websocket::SessionState> {
    CODEX_SESSIONS.state()
}

pub fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    CODEX_SESSIONS.is_sse_fallback_active(session_id)
}

pub fn record_websocket_sse_fallback(session_id: Option<&str>) {
    CODEX_SESSIONS.record_sse_fallback(session_id);
}

/// Pins the session to SSE fallback and records the failure in the debug stats.
pub fn record_websocket_failure(session_id: Option<&str>, error: &CodexStreamError) {
    CODEX_SESSIONS.record_failure(session_id, error);
}

#[must_use]
pub fn get_debug_stats(session_id: &str) -> Option<WebSocketDebugStats> {
    CODEX_SESSIONS.debug_stats(session_id)
}

pub fn reset_debug_stats(session_id: Option<&str>) {
    CODEX_SESSIONS.reset_debug_stats(session_id);
}

/// Close the cached connection for one session or all of them; the agent layer calls it when a
/// session ends (the TS registers a session resource cleanup).
pub fn close_websocket_sessions(session_id: Option<&str>) {
    CODEX_SESSIONS.close(session_id);
}

/// Record debug stats for a request (mirrors the stats block in `processWebSocketStream`).
pub fn record_request_stats(
    session_id: &str,
    reused: bool,
    use_cached_context: bool,
    request_body: &Value,
) {
    CODEX_SESSIONS.record_request_stats(session_id, reused, use_cached_context, request_body);
}

/// Take the continuation for `connection_id`, if one is anchored to it (part of
/// `buildCachedWebSocketRequestBody` in the TS).
pub fn take_continuation_for(session_id: &str, connection_id: u64) -> Option<ContinuationState> {
    CODEX_SESSIONS.take_continuation_for(session_id, connection_id)
}

/// Restore (or clear) the continuation after a failed continuation attempt.
pub fn clear_continuation(session_id: &str, connection_id: u64) {
    CODEX_SESSIONS.clear_continuation(session_id, connection_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::openai_codex_responses::errors::WebSocketTransportError;

    #[test]
    fn sse_fallback_lifecycle() {
        let session = format!("test-session-{}", std::process::id());
        reset_debug_stats(Some(&session));
        assert!(!is_websocket_sse_fallback_active(Some(&session)));
        // Recording a fallback bump does not pin the session (only a transport failure does,
        // matching the TS semantics).
        record_websocket_sse_fallback(Some(&session));
        assert!(!is_websocket_sse_fallback_active(Some(&session)));
        let stats = get_debug_stats(&session).expect("stats recorded");
        assert_eq!(stats.sse_fallbacks, 1);
        assert_eq!(stats.websocket_fallback_active, Some(false));
        reset_debug_stats(Some(&session));
        assert!(get_debug_stats(&session).is_none());
        assert!(!is_websocket_sse_fallback_active(Some(&session)));
    }

    #[tokio::test]
    async fn records_failure_and_pins_fallback() {
        let session = format!("failure-session-{}", std::process::id());
        reset_debug_stats(Some(&session));
        record_websocket_failure(
            Some(&session),
            &CodexStreamError::Transport(WebSocketTransportError::runtime("boom")),
        );
        assert!(is_websocket_sse_fallback_active(Some(&session)));
        let stats = get_debug_stats(&session).expect("stats recorded");
        assert_eq!(stats.websocket_failures, 1);
        assert_eq!(stats.last_websocket_error.as_deref(), Some("boom"));
        close_websocket_sessions(Some(&session));
        reset_debug_stats(Some(&session));
    }
}
