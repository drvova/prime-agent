//! Codex WebSocket transport: the Codex wire profile and the Codex session wrappers over the
//! shared Responses WebSocket connections.

use tokio_util::sync::CancellationToken;

use crate::providers::openai_codex_responses::errors::CodexStreamError;
use crate::providers::openai_codex_responses::session::CODEX_SESSIONS;
use crate::providers::responses_websocket::{AcquiredConnection, WireProfile};

pub(crate) use crate::providers::responses_websocket::{
    build_cached_websocket_request_body, ContinuationState, WorkerEvent,
};

#[cfg(test)]
use crate::providers::responses_websocket::errors::WEBSOCKET_CLOSE_CODE_TOO_BIG;
#[cfg(test)]
use serde_json::Value;

pub const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";

/// Codex reports invalid frames under its own name and ends a request at its completion events.
pub(crate) const CODEX_WIRE: WireProfile = WireProfile {
    label: "Codex",
    terminal_events: &["response.completed", "response.done", "response.incomplete"],
};

/// Reuse the session's idle connection when possible, otherwise open a fresh connection (cached per
/// session).
pub async fn acquire_websocket(
    url: &str,
    headers: &[(String, String)],
    session_id: Option<&str>,
    signal: Option<CancellationToken>,
) -> Result<AcquiredConnection, CodexStreamError> {
    CODEX_SESSIONS
        .acquire(url, headers, session_id, signal, None)
        .await
}

pub async fn release_connection(
    connection: AcquiredConnection,
    keep: bool,
    continuation: Option<ContinuationState>,
) {
    CODEX_SESSIONS.release(connection, keep, continuation).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn continuation(last_body: Value, items: Vec<Value>) -> ContinuationState {
        ContinuationState {
            last_request_body: last_body,
            last_response_id: "resp_1".to_string(),
            last_response_items: items,
            connection_id: 42,
        }
    }

    #[test]
    fn computes_input_delta_for_matching_bodies() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let items = vec![json!({ "type": "assistant_item" })];
        let cont = continuation(base, items);
        let next = json!({
            "model": "gpt",
            "input": [ { "type": "a" }, { "type": "assistant_item" }, { "type": "new" } ],
        });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert_eq!(request["previous_response_id"], "resp_1");
        assert_eq!(request["input"], json!([{ "type": "new" }]));
    }

    #[test]
    fn rejects_delta_when_body_differs() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let cont = continuation(base, vec![]);
        let next = json!({ "model": "other", "input": [ { "type": "a" }, { "type": "b" } ] });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert!(request.get("previous_response_id").is_none());
        assert_eq!(request["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn rejects_delta_when_prefix_differs() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let cont = continuation(base, vec![json!({ "type": "x" })]);
        let next = json!({ "model": "gpt", "input": [ { "type": "a" }, { "type": "y" } ] });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert!(request.get("previous_response_id").is_none());
    }

    #[test]
    fn rejects_continuation_from_other_connection() {
        let base = json!({ "model": "gpt", "input": [] });
        let cont = continuation(base.clone(), vec![]);
        let request = build_cached_websocket_request_body(Some(&cont), &base, 7);
        assert!(request.get("previous_response_id").is_none());
    }
}

/// Raw-socket WebSocket mocks for the transport tests: one connection, one scripted wire sequence
/// per scenario, pinning the TS-binary-verified texts and the diagnostic error surface in-crate.
#[cfg(test)]
mod ws_wire_tests {
    use super::*;
    use crate::providers::openai_codex_responses::errors::{
        WebSocketTransportError, WEBSOCKET_CLOSE_CODE_ABNORMAL,
    };
    use base64::Engine as _;
    use sha1::Digest as _;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    const OP_TEXT: u8 = 0x1;
    const OP_CLOSE: u8 = 0x8;

    /// The scripted wire sequence the mock serves after the handshake request is read.
    enum MockAction {
        /// Answer the upgrade request with a plain HTTP error.
        RejectHttp(u16),
        /// Complete the 101 handshake with a wrong Sec-WebSocket-Accept.
        BadAcceptKey,
        /// Send a close frame with code + reason, then FIN.
        CloseCode { code: u16, reason: &'static str },
        /// Send a close frame with no status code, then FIN.
        CloseNoCode,
        /// Close the TCP connection without a close frame.
        Fin,
        /// Send a reserved data-opcode frame (0x3).
        ReservedDataOpcode,
        /// Send a reserved control-opcode frame (0xB).
        ReservedControlOpcode,
        /// Send a text frame with RSV1 set.
        RsvBits,
        /// Send a non-JSON text frame.
        InvalidJson,
    }

    async fn read_http_head(socket: &mut TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let mut chunk = [0u8; 512];
            let read = socket.read(&mut chunk).await.expect("mock read");
            assert!(read > 0, "client closed before the request head");
            buf.extend_from_slice(&chunk[..read]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8_lossy(&buf).to_string();
            }
        }
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    fn ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x80 | opcode];
        assert!(payload.len() < 126, "mock frames are small");
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the assert bounds the mock frame (< 126); the small-frame length field is u8"
        )]
        frame.push(payload.len() as u8);
        frame.extend_from_slice(payload);
        frame
    }

    async fn spawn_ws_mock(action: MockAction) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("mock accept");
            let head = read_http_head(&mut socket).await;
            let key = header_value(&head, "sec-websocket-key").expect("upgrade key");
            if let MockAction::RejectHttp(status) = action {
                let reason = http::StatusCode::from_u16(status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .unwrap_or("Error");
                socket
                    .write_all(
                        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .expect("mock write");
                socket.shutdown().await.ok();
                return;
            }
            let accept = if matches!(action, MockAction::BadAcceptKey) {
                "aW52YWxpZA==".to_string()
            } else {
                base64::engine::general_purpose::STANDARD
                    .encode(sha1::Sha1::digest(format!("{key}{WS_GUID}").as_bytes()))
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("mock write");
            // The client sends its response.create; give it time to arrive.
            tokio::time::sleep(Duration::from_millis(50)).await;
            match action {
                MockAction::CloseCode { code, reason } => {
                    let mut payload = code.to_be_bytes().to_vec();
                    payload.extend_from_slice(reason.as_bytes());
                    socket
                        .write_all(&ws_frame(OP_CLOSE, &payload))
                        .await
                        .unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::CloseNoCode => {
                    socket.write_all(&ws_frame(OP_CLOSE, &[])).await.unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::Fin => {
                    socket.shutdown().await.ok();
                }
                MockAction::ReservedDataOpcode => {
                    socket.write_all(&[0x83, 0x00]).await.unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::ReservedControlOpcode => {
                    socket.write_all(&[0x8B, 0x00]).await.unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::RsvBits => {
                    socket.write_all(&[0xC1, 0x00]).await.unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::InvalidJson => {
                    socket
                        .write_all(&ws_frame(OP_TEXT, b"not json"))
                        .await
                        .unwrap();
                    socket.shutdown().await.ok();
                }
                MockAction::RejectHttp(_) | MockAction::BadAcceptKey => {}
            }
            // Hold the socket so the failure reaches the client cleanly.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        format!("ws://127.0.0.1:{port}/codex/responses")
    }

    /// Drive one acquire+send+read cycle and return the terminal transport error (the read loop
    /// surfaces it after the scripted wire sequence).
    async fn request_terminal_error(action: MockAction) -> CodexStreamError {
        let url = spawn_ws_mock(action).await;
        let connection = acquire_websocket(&url, &[], None, None)
            .await
            .expect("handshake succeeds");
        let mut events = connection
            .send_request(&serde_json::json!({ "model": "gpt" }), None)
            .await
            .expect("request sent");
        loop {
            match events.recv().await.expect("worker stays alive") {
                WorkerEvent::Event(_) => {}
                WorkerEvent::Terminal(result) => return result.expect_err("scripted failure"),
            }
        }
    }

    /// Drive one acquire against a mock that fails before the handshake completes and return the
    /// connect-phase error.
    async fn acquire_connect_error(action: MockAction) -> WebSocketTransportError {
        let url = spawn_ws_mock(action).await;
        match acquire_websocket(&url, &[], None, None).await {
            Ok(_) => panic!("expected the connect phase to fail"),
            Err(CodexStreamError::Transport(error)) => error,
            Err(other) => panic!("expected a transport error, got {other}"),
        }
    }

    #[tokio::test]
    async fn refused_connect_text() {
        let url = "ws://127.0.0.1:1/codex/responses".to_string();
        let error = match acquire_websocket(&url, &[], None, None).await {
            Ok(_) => panic!("expected the dead port to fail"),
            Err(CodexStreamError::Transport(error)) => error,
            Err(other) => panic!("expected a transport error, got {other}"),
        };
        assert_eq!(
            error.to_string(),
            "WebSocket connection to 'ws://127.0.0.1:1/codex/responses' failed: Failed to connect"
        );
        assert_eq!(error.error_name(), "Error");
        assert_eq!(error.close_code(), None);
    }

    /// A non-101 upgrade answer: the "Expected 101 status code" cause (probed for 401 and 500).
    #[tokio::test]
    async fn handshake_rejection_text() {
        for status in [401u16, 500u16] {
            let error = acquire_connect_error(MockAction::RejectHttp(status)).await;
            assert!(
                error
                    .to_string()
                    .starts_with("WebSocket connection to 'ws://127.0.0.1:"),
                "unexpected text {error}"
            );
            assert!(
                error
                    .to_string()
                    .ends_with("/codex/responses' failed: Expected 101 status code"),
                "unexpected text {error}"
            );
            assert_eq!(error.error_name(), "Error");
        }
    }

    #[tokio::test]
    async fn bad_accept_key_text() {
        let error = acquire_connect_error(MockAction::BadAcceptKey).await;
        assert!(error
            .to_string()
            .ends_with("/codex/responses' failed: Mismatch websocket accept header"));
        assert_eq!(error.error_name(), "Error");
    }

    #[tokio::test]
    async fn close_frame_with_reason_text() {
        let error = request_terminal_error(MockAction::CloseCode {
            code: 1011,
            reason: "mock server reason",
        })
        .await;
        let CodexStreamError::Transport(transport) = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(
            transport.to_string(),
            "WebSocket closed 1011 mock server reason"
        );
        assert_eq!(transport.error_name(), "WebSocketCloseError");
        assert_eq!(transport.close_code(), Some(1011));
    }

    /// The 1009 no-reason special case composes "message too big".
    #[tokio::test]
    async fn close_1009_no_reason_text() {
        let error = request_terminal_error(MockAction::CloseCode {
            code: WEBSOCKET_CLOSE_CODE_TOO_BIG,
            reason: "",
        })
        .await;
        let CodexStreamError::Transport(transport) = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(
            transport.to_string(),
            "WebSocket closed 1009 message too big"
        );
        assert_eq!(transport.close_code(), Some(1009));
    }

    #[tokio::test]
    async fn close_no_code_text() {
        let error = request_terminal_error(MockAction::CloseNoCode).await;
        let CodexStreamError::Transport(transport) = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(transport.to_string(), "WebSocket closed 1005");
        assert_eq!(transport.close_code(), Some(1005));
    }

    #[tokio::test]
    async fn fin_without_close_frame_text() {
        let error = request_terminal_error(MockAction::Fin).await;
        let CodexStreamError::Transport(transport) = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(
            transport.to_string(),
            "WebSocket closed 1006 Connection ended"
        );
        assert_eq!(transport.error_name(), "WebSocketCloseError");
        assert_eq!(transport.close_code(), Some(WEBSOCKET_CLOSE_CODE_ABNORMAL));
    }

    #[tokio::test]
    async fn reserved_opcode_text() {
        for action in [
            MockAction::ReservedDataOpcode,
            MockAction::ReservedControlOpcode,
        ] {
            let error = request_terminal_error(action).await;
            let CodexStreamError::Transport(transport) = &error else {
                panic!("expected a transport error, got {error}");
            };
            assert_eq!(
                transport.to_string(),
                "WebSocket closed 1002 Protocol error - unsupported control frame"
            );
            assert_eq!(transport.close_code(), Some(1002));
        }
    }

    #[tokio::test]
    async fn rsv_bits_text() {
        let error = request_terminal_error(MockAction::RsvBits).await;
        let CodexStreamError::Transport(transport) = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(
            transport.to_string(),
            "WebSocket closed 1011 Compression not implemented yet"
        );
        assert_eq!(transport.close_code(), Some(1011));
    }

    #[tokio::test]
    async fn wss_to_plain_tls_text() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("mock accept");
            // Read the TLS ClientHello then answer plain HTTP: the handshake fails against a
            // non-TLS peer.
            let mut buf = [0u8; 512];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("mock write");
        });
        let url = format!("wss://127.0.0.1:{port}/codex/responses");
        let error = match acquire_websocket(&url, &[], None, None).await {
            Ok(_) => panic!("expected the TLS handshake to fail"),
            Err(CodexStreamError::Transport(error)) => error,
            Err(other) => panic!("expected a transport error, got {other}"),
        };
        assert_eq!(
            error.to_string(),
            format!("WebSocket connection to 'wss://127.0.0.1:{port}/codex/responses' failed: TLS handshake failed")
        );
        assert_eq!(error.error_name(), "Error");
    }

    /// An invalid JSON frame is a non-transport protocol error: thrown without the SSE fallback.
    #[tokio::test]
    async fn invalid_json_is_non_transport_error() {
        let error = request_terminal_error(MockAction::InvalidJson).await;
        assert!(error.is_non_transport_error(), "expected a protocol error");
        assert!(
            error
                .to_string()
                .starts_with("Invalid Codex WebSocket JSON: "),
            "unexpected text {error}"
        );
    }
}
