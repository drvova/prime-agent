//! Responses WebSocket error surface shared by every provider that speaks the Responses
//! WebSocket mode: API and protocol errors, the transport (close/runtime) errors with the TS
//! runtime's texts, the unified stream error, and the transport-failure diagnostic.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::types::AssistantMessage;
use crate::utils_inner::stream_failure::ProviderError;

/// A Responses API error event (the class name it records is the Codex provider's, its only
/// converter).
#[derive(Debug, Clone)]
#[expect(
    dead_code,
    reason = "the struct keeps the TS error surface; retry plumbing reads only some fields"
)]
pub struct ResponsesApiError {
    pub message: String,
    pub code: Option<String>,
    pub status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    pub payload: Option<Value>,
}

impl ResponsesApiError {
    #[expect(
        dead_code,
        reason = "a parity helper; the transport loops build the struct literally"
    )]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            status: None,
            retry_after_ms: None,
            payload: None,
        }
    }

    /// The TS provider surfaces `ResponsesApiError.message` verbatim (the usage-limit friendly text
    /// included) instead of the classified rewrite.
    pub fn into_provider_error(self) -> ProviderError {
        ProviderError::Http(crate::utils_inner::stream_failure::ProviderHttpError {
            message: self.message,
            status: self.status,
            // The TS error carries its wire `code` and no body/error field: the classification
            // reads the code, not a parsed body.
            body: None,
            headers: HashMap::new(),
            request_id: None,
            sdk_name: Some("CodexApiError".to_string()),
            retry_after_ms: self.retry_after_ms,
            provider_error_type: self.code,
        })
    }
}

impl std::fmt::Display for ResponsesApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A Responses protocol (framing/JSON) error (recorded under the Codex class name, its only
/// converter).
#[derive(Debug, Clone)]
pub struct ResponsesProtocolError {
    pub message: String,
    /// The invalid frame the TS error keeps for debugging; user-facing text never embeds it.
    #[expect(
        dead_code,
        reason = "the TS error keeps the invalid frame for debugging; user-facing text never reads it"
    )]
    pub payload: Option<Value>,
}

impl ResponsesProtocolError {
    pub fn into_provider_error(self) -> ProviderError {
        // TS surfaces `ResponsesProtocolError.message` verbatim; the payload stays non-user-facing and
        // the class name is recorded for the diagnostic.
        ProviderError::Http(crate::utils_inner::stream_failure::ProviderHttpError {
            message: self.message,
            status: None,
            body: None,
            headers: HashMap::new(),
            request_id: None,
            sdk_name: Some("CodexProtocolError".to_string()),
            retry_after_ms: None,
            provider_error_type: None,
        })
    }
}

/// The TS WebSocket transport-error surface, probe-verified against the TS binary (bun runtime):
/// close-event failures compose `WebSocket closed {code} {reason}` and record the
/// `WebSocketCloseError` class name; every other runtime failure is a plain `Error` with no close
/// code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketTransportError {
    /// A close-event failure: the composed close message plus the numeric close code.
    Close { message: String, code: u16 },
    /// A connect- or send-phase runtime failure.
    Runtime { message: String },
}

/// Close code the TS runtime reports for a close frame that carries none (WHATWG `Status`;
/// probe-pinned as "WebSocket closed 1005").
pub const WEBSOCKET_CLOSE_CODE_STATUS: u16 = 1005;
/// Close code the runtime reports for a socket death without a close frame.
pub const WEBSOCKET_CLOSE_CODE_ABNORMAL: u16 = 1006;
/// Close code the runtime reports for frames it cannot parse.
pub const WEBSOCKET_CLOSE_CODE_PROTOCOL: u16 = 1002;
/// Close code for a message the runtime refuses as too big.
pub const WEBSOCKET_CLOSE_CODE_TOO_BIG: u16 = 1009;
/// The runtime's close-event reason for an abrupt socket death.
pub const WEBSOCKET_CONNECTION_ENDED_REASON: &str = "Connection ended";

impl WebSocketTransportError {
    /// `extractWebSocketCloseError`'s composition: `WebSocket closed {code} {reason}` (trimmed),
    /// with the 1009 no-reason special case (`"message too big"`).
    pub fn close(code: u16, reason: &str) -> Self {
        let mut reason_text = if reason.is_empty() {
            String::new()
        } else {
            format!(" {reason}")
        };
        if reason_text.is_empty() && code == WEBSOCKET_CLOSE_CODE_TOO_BIG {
            reason_text = " message too big".to_string();
        }
        let message = format!("WebSocket closed {code}{reason_text}")
            .trim()
            .to_string();
        Self::Close { message, code }
    }

    /// A plain runtime failure (`Error` in the TS) with the given text.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::Runtime {
            message: message.into(),
        }
    }

    /// The TS error class name the diagnostics record (`error.name`).
    pub fn error_name(&self) -> &'static str {
        match self {
            Self::Close { .. } => "WebSocketCloseError",
            Self::Runtime { .. } => "Error",
        }
    }

    /// The numeric close code the TS diagnostic records (`error.code`);
    /// plain runtime failures carry none.
    pub fn close_code(&self) -> Option<u16> {
        match self {
            Self::Close { code, .. } => Some(*code),
            Self::Runtime { .. } => None,
        }
    }

    /// The user-facing text (TS `error.message`).
    pub fn message(&self) -> &str {
        match self {
            Self::Close { message, .. } | Self::Runtime { message } => message,
        }
    }
}

impl std::fmt::Display for WebSocketTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Unified in-band stream error used by the transport loops.
#[derive(Debug, Clone)]
pub enum ResponsesStreamError {
    Api(ResponsesApiError),
    Protocol(ResponsesProtocolError),
    /// Transport-level failure with the TS runtime's error surface.
    Transport(WebSocketTransportError),
    /// Abort requested by the cancellation token.
    Aborted,
}

impl From<ProviderError> for ResponsesStreamError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Aborted => ResponsesStreamError::Aborted,
            other => ResponsesStreamError::Api(ResponsesApiError {
                message: other.to_string(),
                code: None,
                status: None,
                retry_after_ms: None,
                payload: None,
            }),
        }
    }
}

impl ResponsesStreamError {
    pub fn into_provider_error(self) -> ProviderError {
        match self {
            ResponsesStreamError::Api(error) => error.into_provider_error(),
            ResponsesStreamError::Protocol(error) => error.into_provider_error(),
            ResponsesStreamError::Transport(error) => ProviderError::Transport(
                crate::utils_inner::stream_failure::ProviderWsTransportError {
                    message: error.message().to_string(),
                    close_code: error.close_code(),
                },
            ),
            ResponsesStreamError::Aborted => ProviderError::Aborted,
        }
    }

    pub fn is_non_transport_error(&self) -> bool {
        matches!(
            self,
            ResponsesStreamError::Api(_) | ResponsesStreamError::Protocol(_)
        )
    }
}

impl std::fmt::Display for ResponsesStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponsesStreamError::Api(error) => f.write_str(&error.message),
            ResponsesStreamError::Protocol(error) => f.write_str(&error.message),
            ResponsesStreamError::Transport(error) => f.write_str(error.message()),
            ResponsesStreamError::Aborted => f.write_str("Request was aborted"),
        }
    }
}

/// The transport-failure diagnostic payload (TS-binary probe-verified): the thrown error's runtime
/// class name (`WebSocketCloseError` for close events, plain `Error` otherwise) and, for close
/// events, the numeric close code as `error.code`.
pub fn append_transport_failure_diagnostic(
    output: &mut AssistantMessage,
    error: &WebSocketTransportError,
    configured_transport: &str,
    events_emitted: bool,
    request_bytes: usize,
) {
    let diagnostic = crate::utils_inner::diagnostics::create_assistant_message_diagnostic(
        "provider_transport_failure",
        Some(crate::types::DiagnosticErrorInfo {
            name: Some(error.error_name().to_string()),
            message: error.message().to_string(),
            stack: None,
            code: error.close_code().map(|code| {
                crate::types::DiagnosticCode::Num(crate::types::JsNumber::from(u64::from(code)))
            }),
            rest: Map::default(),
        }),
        Some(transport_failure_details(
            configured_transport,
            events_emitted,
            request_bytes,
        )),
    );
    crate::utils_inner::diagnostics::append_assistant_message_diagnostic(output, diagnostic);
}

/// The diagnostic's `details`: after start the TS leaves `fallbackTransport` undefined, and
/// `JSON.stringify` omits the key entirely.
fn transport_failure_details(
    configured_transport: &str,
    events_emitted: bool,
    request_bytes: usize,
) -> Value {
    let mut details = serde_json::Map::new();
    details.insert(
        "configuredTransport".to_string(),
        Value::String(configured_transport.to_string()),
    );
    if !events_emitted {
        details.insert(
            "fallbackTransport".to_string(),
            Value::String("sse".to_string()),
        );
    }
    details.insert("eventsEmitted".to_string(), Value::Bool(events_emitted));
    details.insert(
        "phase".to_string(),
        Value::String(
            if events_emitted {
                "after_message_stream_start"
            } else {
                "before_message_stream_start"
            }
            .to_string(),
        ),
    );
    details.insert("requestBytes".to_string(), json!(request_bytes));
    Value::Object(details)
}
