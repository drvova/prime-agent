//! Streaming provider implementations ported from `packages/ai/src/providers`.

pub mod anthropic;
pub mod azure_openai_responses;
pub mod bedrock;
pub mod faux;
pub mod google;
pub mod google_shared;
pub mod google_stream;
pub mod google_vertex;
pub mod mistral;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;
pub mod openai_responses_hooks;
pub mod openai_responses_shared;
pub mod openai_responses_stream;
mod openai_responses_transport;
mod opencode_headers;
pub(crate) mod responses_websocket;
pub mod simple_options;
pub mod transform_messages;

/// Close every cached Responses WebSocket of a session (the Codex and the Responses API
/// providers each keep their own); the session layer calls it when a session ends.
pub fn close_session_websockets(session_id: &str) {
    openai_codex_responses::session::close_websocket_sessions(Some(session_id));
    openai_responses_transport::close_websocket_session(session_id);
}
