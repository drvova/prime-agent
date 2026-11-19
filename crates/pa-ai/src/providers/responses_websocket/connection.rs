//! Responses WebSocket connections: the per-connection worker task and the per-request event loop.
//!
//! Each connection owns a worker task that holds the socket; requests talk to it over a command
//! channel and receive events over a fresh channel per request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{CapacityError, ProtocolError};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use tokio_util::sync::CancellationToken;

use super::errors::{
    ResponsesProtocolError, ResponsesStreamError, WebSocketTransportError,
    WEBSOCKET_CLOSE_CODE_ABNORMAL, WEBSOCKET_CLOSE_CODE_PROTOCOL, WEBSOCKET_CLOSE_CODE_STATUS,
    WEBSOCKET_CLOSE_CODE_TOO_BIG, WEBSOCKET_CONNECTION_ENDED_REASON,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What differs between the servers that speak the Responses WebSocket mode: the label invalid
/// frames are reported under and the event types that end one request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WireProfile {
    /// Provider label in the invalid-frame text (`Invalid {label} WebSocket JSON: ...`).
    pub(crate) label: &'static str,
    /// Event types after which the server sends nothing more for the request.
    pub(crate) terminal_events: &'static [&'static str],
}

/// Command sent to a connection's worker task.
pub(crate) enum WorkerCommand {
    /// Send one `response.create` request; events flow over `events`.
    Send {
        body: String,
        events: mpsc::Sender<WorkerEvent>,
        /// This request's own abort signal. A reused connection outlives the request that opened
        /// it, so the worker races each read against the current request's token, never the
        /// opener's (which fires when that first request's stream is dropped).
        signal: Option<CancellationToken>,
    },
    Close,
}

/// Events a worker forwards to the active request.
pub(crate) enum WorkerEvent {
    Event(Value),
    /// Terminal marker after the completion event (Ok) or an error (Err).
    Terminal(Result<(), ResponsesStreamError>),
}

fn next_connection_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// Port of the TS runtime's (bun) WebSocket connect-failure surface, probe-verified against the TS
/// binary: every connect-phase failure is a plain `Error` whose message is `WebSocket connection to
/// '<url>' failed: <cause>` with the runtime's own cause texts.
fn bun_connect_failure(url: &str, error: &WsError) -> WebSocketTransportError {
    let cause = match error {
        WsError::Io(io) => {
            // tokio-tungstenite surfaces the rustls handshake failure as a plain io error, so on
            // wss URLs any socket failure after the refused connect maps to the runtime's
            // TLS-handshake text.
            if url.starts_with("wss://")
                && !matches!(
                    io.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
                )
            {
                "TLS handshake failed".to_string()
            } else {
                "Failed to connect".to_string()
            }
        }
        WsError::Tls(_) => "TLS handshake failed".to_string(),
        WsError::Http(_) => "Expected 101 status code".to_string(),
        // The runtime checks the status code before the HTTP shape, so a response the transport
        // rejects for its version or method carries the same non-101 text.
        WsError::Protocol(ProtocolError::WrongHttpVersion | ProtocolError::WrongHttpMethod) => {
            "Expected 101 status code".to_string()
        }
        WsError::Protocol(ProtocolError::SecWebSocketAcceptKeyMismatch) => {
            "Mismatch websocket accept header".to_string()
        }
        other => other.to_string(),
    };
    WebSocketTransportError::runtime(format!("WebSocket connection to '{url}' failed: {cause}"))
}

/// Port of the TS runtime's (bun) WebSocket read-failure surface, probe-verified against the TS
/// binary: every socket or frame failure surfaces as a close event composing `WebSocket closed
/// {code} {reason}`. Unprobed violations compose the runtime's protocol-error envelope with the
/// transport's own detail text.
fn bun_read_failure(error: &WsError) -> WebSocketTransportError {
    match error {
        // The transport's EOF-without-close-frame error is the same socket death the runtime
        // reports as the 1006 close event.
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            WebSocketTransportError::close(
                WEBSOCKET_CLOSE_CODE_ABNORMAL,
                WEBSOCKET_CONNECTION_ENDED_REASON,
            )
        }
        WsError::Capacity(CapacityError::MessageTooLong { .. }) => {
            WebSocketTransportError::close(WEBSOCKET_CLOSE_CODE_TOO_BIG, "")
        }
        WsError::Io(_)
        | WsError::ConnectionClosed
        | WsError::AlreadyClosed
        | WsError::WriteBufferFull(_)
        | WsError::Capacity(_) => WebSocketTransportError::close(
            WEBSOCKET_CLOSE_CODE_ABNORMAL,
            WEBSOCKET_CONNECTION_ENDED_REASON,
        ),
        WsError::Protocol(ProtocolError::NonZeroReservedBits) => {
            WebSocketTransportError::close(1011, "Compression not implemented yet")
        }
        WsError::Protocol(
            ProtocolError::InvalidOpcode(_)
            | ProtocolError::UnknownDataFrameType(_)
            | ProtocolError::UnknownControlFrameType(_),
        ) => WebSocketTransportError::close(
            WEBSOCKET_CLOSE_CODE_PROTOCOL,
            "Protocol error - unsupported control frame",
        ),
        WsError::Protocol(other) => WebSocketTransportError::close(
            WEBSOCKET_CLOSE_CODE_PROTOCOL,
            &format!("Protocol error - {other}"),
        ),
        other => WebSocketTransportError::close(
            WEBSOCKET_CLOSE_CODE_PROTOCOL,
            &format!("Protocol error - {other}"),
        ),
    }
}

/// Open one connection and park its worker. `connect_timeout` bounds the handshake (a capability
/// probe of an endpoint not yet known to speak the WebSocket mode); `None` waits for the runtime.
pub(crate) async fn spawn_connection_worker(
    url: &str,
    headers: &[(String, String)],
    signal: Option<CancellationToken>,
    profile: WireProfile,
    connect_timeout: Option<Duration>,
) -> Result<(mpsc::Sender<WorkerCommand>, u64), ResponsesStreamError> {
    let mut request = url.into_client_request().map_err(|error| {
        ResponsesStreamError::Transport(WebSocketTransportError::runtime(format!(
            "WebSocket connection to '{url}' failed: {error}"
        )))
    })?;
    for (name, value) in headers {
        let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ResponsesStreamError::Transport(WebSocketTransportError::runtime(format!(
                "WebSocket connection to '{url}' failed: Invalid WebSocket header {name}"
            )))
        })?;
        let value = http::HeaderValue::from_str(value).map_err(|_| {
            ResponsesStreamError::Transport(WebSocketTransportError::runtime(format!(
                "WebSocket connection to '{url}' failed: Invalid WebSocket header value"
            )))
        })?;
        request.headers_mut().insert(name, value);
    }

    if signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(ResponsesStreamError::Aborted);
    }

    // Boxed: the TLS handshake future is ~20 KB, and every caller's future would carry it inline.
    let connect = Box::pin(tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(crate::utils_inner::tls::websocket_connector()),
    ));
    let bounded = async {
        match connect_timeout {
            Some(limit) => tokio::time::timeout(limit, connect).await.map_err(|_| {
                ResponsesStreamError::Transport(WebSocketTransportError::runtime(format!(
                    "WebSocket connection to '{url}' failed: Connection timed out"
                )))
            })?,
            None => connect.await,
        }
        .map_err(|error| ResponsesStreamError::Transport(bun_connect_failure(url, &error)))
    };
    let (stream, _response) = match signal.as_ref() {
        Some(signal) => {
            tokio::select! {
                () = signal.cancelled() => return Err(ResponsesStreamError::Aborted),
                result = bounded => result,
            }
        }
        None => bounded.await,
    }?;

    let connection_id = next_connection_id();
    let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>(4);
    tokio::spawn(connection_worker(stream, command_rx, profile));
    Ok((command_tx, connection_id))
}

/// The worker parks between requests so a session can reuse one connection.
async fn connection_worker(
    stream: WsStream,
    mut commands: mpsc::Receiver<WorkerCommand>,
    profile: WireProfile,
) {
    let (mut sink, mut stream) = stream.split();
    loop {
        let Some(command) = commands.recv().await else {
            return;
        };
        match command {
            WorkerCommand::Close => {
                let _ = sink.close().await;
                return;
            }
            WorkerCommand::Send {
                body,
                events,
                signal,
            } => {
                if sink.send(Message::Text(body.into())).await.is_err() {
                    // A dead socket surfaces through its close event (the TS runtime's send on a
                    // closed socket is a silent no-op; the close event carries the failure).
                    let _ = events
                        .send(WorkerEvent::Terminal(Err(ResponsesStreamError::Transport(
                            WebSocketTransportError::close(
                                WEBSOCKET_CLOSE_CODE_ABNORMAL,
                                WEBSOCKET_CONNECTION_ENDED_REASON,
                            ),
                        ))))
                        .await;
                    let _ = sink.close().await;
                    return;
                }
                let terminal =
                    read_request_events(&mut stream, &events, signal.as_ref(), profile).await;
                if terminal.is_err() {
                    let _ = events.send(WorkerEvent::Terminal(terminal)).await;
                    let _ = sink.close().await;
                    return;
                }
                let _ = events.send(WorkerEvent::Terminal(terminal)).await;
            }
        }
    }
}

/// Read one request's events until completion, close, or error.
async fn read_request_events(
    stream: &mut SplitStream<WsStream>,
    events: &mpsc::Sender<WorkerEvent>,
    signal: Option<&CancellationToken>,
    profile: WireProfile,
) -> Result<(), ResponsesStreamError> {
    let mut saw_completion = false;
    loop {
        if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return Err(ResponsesStreamError::Aborted);
        }
        let next = stream.next();
        let message = match signal {
            Some(signal) => {
                tokio::select! {
                    () = signal.cancelled() => return Err(ResponsesStreamError::Aborted),
                    message = next => message,
                }
            }
            None => next.await,
        };
        match message {
            // Peer EOF without a close frame: the runtime fires a close event with code 1006
            // ("Connection ended" in the TS runtime).
            None => {
                return Err(ResponsesStreamError::Transport(
                    WebSocketTransportError::close(
                        WEBSOCKET_CLOSE_CODE_ABNORMAL,
                        WEBSOCKET_CONNECTION_ENDED_REASON,
                    ),
                ));
            }
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str().to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(event) => {
                        let event_type = event.get("type").and_then(Value::as_str);
                        if event_type
                            .is_some_and(|event_type| profile.terminal_events.contains(&event_type))
                        {
                            saw_completion = true;
                        }
                        if events.send(WorkerEvent::Event(event)).await.is_err() {
                            // The request consumer is gone (its receiver was dropped); nobody
                            // observes this text.
                            return Err(ResponsesStreamError::Transport(
                                WebSocketTransportError::runtime("WebSocket request cancelled"),
                            ));
                        }
                        if saw_completion {
                            return Ok(());
                        }
                    }
                    // Port of `parseWebSocket`'s JSON failure: a non-transport protocol error,
                    // thrown without SSE fallback.
                    Err(error) => {
                        return Err(ResponsesStreamError::Protocol(ResponsesProtocolError {
                            message: format!("Invalid {} WebSocket JSON: {error}", profile.label),
                            payload: Some(Value::String(text)),
                        }));
                    }
                }
            }
            Some(Ok(Message::Binary(bytes))) => {
                // Responses events are JSON text; binary frames decode as UTF-8.
                let text = String::from_utf8_lossy(&bytes).to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(event) => {
                        if events.send(WorkerEvent::Event(event)).await.is_err() {
                            return Err(ResponsesStreamError::Transport(
                                WebSocketTransportError::runtime("WebSocket request cancelled"),
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(ResponsesStreamError::Protocol(ResponsesProtocolError {
                            message: format!("Invalid {} WebSocket JSON: {error}", profile.label),
                            payload: Some(Value::String(text)),
                        }));
                    }
                }
            }
            Some(Ok(Message::Close(close))) => {
                if saw_completion {
                    return Ok(());
                }
                // A close frame without a status code surfaces as the runtime's 1005 close event
                // (probe-verified).
                let error = match close {
                    Some(frame) => {
                        WebSocketTransportError::close(u16::from(frame.code), frame.reason.as_str())
                    }
                    None => WebSocketTransportError::close(WEBSOCKET_CLOSE_CODE_STATUS, ""),
                };
                return Err(ResponsesStreamError::Transport(error));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Err(error)) => {
                return Err(ResponsesStreamError::Transport(bun_read_failure(&error)));
            }
        }
    }
}

/// A session-acquired connection handle.
pub(crate) struct AcquiredConnection {
    pub(crate) worker: mpsc::Sender<WorkerCommand>,
    pub session_id: Option<String>,
    pub reused: bool,
    /// A cache entry backs this connection (continuation is possible).
    pub cached: bool,
    pub connection_id: u64,
}

impl AcquiredConnection {
    /// Send the `response.create` request and return the event channel.
    pub async fn send_request(
        &self,
        body: &Value,
        signal: Option<CancellationToken>,
    ) -> Result<mpsc::Receiver<WorkerEvent>, ResponsesStreamError> {
        if signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(ResponsesStreamError::Aborted);
        }
        let mut request = body.clone();
        request["type"] = Value::String("response.create".to_string());
        let (event_tx, event_rx) = mpsc::channel(64);
        self.worker
            .send(WorkerCommand::Send {
                body: request.to_string(),
                events: event_tx,
                signal,
            })
            .await
            .map_err(|_| {
                // The worker (socket) is gone; in the TS the socket's close event would surface
                // with the 1006 close text.
                ResponsesStreamError::Transport(WebSocketTransportError::close(
                    WEBSOCKET_CLOSE_CODE_ABNORMAL,
                    WEBSOCKET_CONNECTION_ENDED_REASON,
                ))
            })?;
        Ok(event_rx)
    }

    /// Ask the worker to close the socket.
    pub async fn close(&self) {
        let _ = self.worker.send(WorkerCommand::Close).await;
    }
}
