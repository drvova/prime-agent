//! The Responses API's WebSocket mode for the `openai-responses` provider: the per-request
//! transport choice and the WebSocket attempt (connection-anchored continuation deltas, one
//! full-context resend for a dropped continuation, SSE fallback and session demotion).
//!
//! Verified handshake (openai-node `ResponsesWS`, openai-python `responses.connect()`):
//! `GET wss://<base>/responses` with `Authorization: Bearer <key>` and no beta header; each turn
//! sends `{"type":"response.create", ...}` with the HTTP create body minus `stream`/`background`.

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventWriter};
use crate::providers::openai_responses_shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions, ResponsesStreamHooks,
    ResponsesStreamProcessor, OPENAI_TOOL_CALL_PROVIDERS,
};
use crate::providers::responses_websocket::errors::{
    append_transport_failure_diagnostic, ResponsesStreamError,
};
use crate::providers::responses_websocket::{
    build_cached_websocket_request_body, endpoint_key, is_verified_endpoint,
    is_visible_response_event, learned_capability, note_counts, note_plan, plan_transport,
    record_learned_capability, transport_debug_name, AcquiredConnection, Capability,
    ContinuationState, PlanInput, TransportPlan, WebSocketSessions, WireProfile, WorkerEvent,
};
use crate::types::{
    AssistantMessage, Context, Message, Model, StopReason, StreamOptions, Transport,
};
use crate::utils_inner::stream_failure::{stream_failure_from_stop_reason, ProviderError};

/// The Responses API ends a request at its terminal response events or an `error` event, and
/// keeps the connection open after either.
const OPENAI_WIRE: WireProfile = WireProfile {
    label: "OpenAI",
    terminal_events: &[
        "response.completed",
        "response.incomplete",
        "response.failed",
        "error",
    ],
};

/// The server closes a connection after 60 minutes (xAI: 59); a cached connection is replaced
/// before that instead of failing a turn at the limit.
const MAX_CONNECTION_AGE: Duration = Duration::from_mins(55);

static OPENAI_SESSIONS: WebSocketSessions =
    WebSocketSessions::new(OPENAI_WIRE, Some(MAX_CONNECTION_AGE));

/// A probe of an endpoint whose WebSocket mode is unknown gives the handshake this long before
/// the request falls back to SSE.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Mid-stream WebSocket failures in a row after which a session is demoted to SSE.
const MID_STREAM_FAILURE_LIMIT: u32 = 2;

/// The continuation error codes after which the request is resent once with the full context:
/// the connection no longer holds the previous response, or the connection hit its lifetime.
const STALE_CONTINUATION_CODE: &str = "previous_response_not_found";
const CONNECTION_LIMIT_CODE: &str = "websocket_connection_limit_reached";

/// Close the session's cached Responses WebSocket.
pub(crate) fn close_websocket_session(session_id: &str) {
    OPENAI_SESSIONS.close(Some(session_id));
}

/// What is known about the model's endpoint: the docs-verified base URLs, then a learned verdict.
fn endpoint_capability(model: &Model) -> Capability {
    let base_url = model.base_url.trim_end_matches('/');
    if is_verified_endpoint(base_url) {
        return Capability::Declared(true);
    }
    endpoint_key(base_url)
        .and_then(|key| learned_capability(&key))
        .map_or(Capability::Unknown, Capability::Learned)
}

/// Choose this request's transport (and count the choice).
pub(crate) fn plan_request(model: &Model, options: &StreamOptions) -> TransportPlan {
    let transport = options.transport.unwrap_or(Transport::Auto);
    let session_id = options.session_id.as_deref();
    let plan = plan_transport(&PlanInput {
        transport,
        has_websocket_wire: true,
        has_session: session_id.is_some(),
        demoted: OPENAI_SESSIONS.is_sse_fallback_active(session_id),
        capability: endpoint_capability(model),
    });
    note_plan(session_id, transport, plan);
    plan
}

/// How the WebSocket attempt ended for the caller.
pub(crate) enum WebSocketOutcome {
    /// The response streamed over the WebSocket.
    Done,
    /// Nothing was streamed; serve the request over SSE.
    FallBackToSse,
}

/// The pieces of one request the WebSocket attempt needs.
pub(crate) struct WebSocketRequest<'a> {
    pub(crate) model: &'a Model,
    pub(crate) options: &'a StreamOptions,
    /// The HTTP create body; `stream`/`background` are dropped for the socket.
    pub(crate) params: &'a Value,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) probe: bool,
    pub(crate) hooks: &'a (dyn Fn() -> ResponsesStreamHooks + Sync),
}

/// Why one attempt failed.
enum AttemptError {
    /// The server dropped the continuation (or hit the connection lifetime) before any output.
    ChainReset { code: &'static str },
    /// The stream itself failed (an error event or a failed response): surfaced as over SSE.
    Stream(ProviderError),
    /// The socket failed (transport, framing, or abort).
    Socket(ResponsesStreamError),
}

fn is_cancelled(signal: Option<&CancellationToken>) -> bool {
    signal.is_some_and(CancellationToken::is_cancelled)
}

pub(crate) async fn stream_over_websocket(
    request: &WebSocketRequest<'_>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<WebSocketOutcome, ProviderError> {
    let session_id = request.options.session_id.as_deref();
    let signal = request.options.signal.as_ref();
    let mut body = request.params.clone();
    if let Some(map) = body.as_object_mut() {
        map.remove("stream");
        map.remove("background");
    }
    let endpoint = endpoint_key(&request.model.base_url);
    let mut started = false;
    let mut chain_reset_retried = false;
    loop {
        let attempt = run_attempt(
            request,
            &body,
            output,
            writer,
            &mut started,
            !chain_reset_retried,
        )
        .await;
        let error = match attempt {
            Ok(()) => {
                OPENAI_SESSIONS.record_success(session_id);
                if request.probe {
                    if let Some(endpoint) = &endpoint {
                        record_learned_capability(endpoint, true);
                    }
                }
                if is_cancelled(signal) {
                    return Err(ProviderError::Aborted);
                }
                return Ok(WebSocketOutcome::Done);
            }
            Err(error) => error,
        };
        if is_cancelled(signal) {
            return Err(ProviderError::Aborted);
        }
        match error {
            AttemptError::ChainReset { code } => {
                // Resend once with the full context; nothing was streamed yet. A metadata event
                // may have supplied the dead response's id: do not keep it.
                chain_reset_retried = true;
                output.response_id = None;
                if let Some(session_id) = session_id {
                    note_counts(session_id, |counts| counts.stale_continuation_retries += 1);
                }
                tracing::debug!(code, "responses websocket continuation reset");
            }
            AttemptError::Stream(error) => {
                if request.probe && !started {
                    record_probe_failure(endpoint.as_deref(), session_id);
                    return Ok(WebSocketOutcome::FallBackToSse);
                }
                return Err(error);
            }
            AttemptError::Socket(ResponsesStreamError::Aborted) => {
                return Err(ProviderError::Aborted);
            }
            AttemptError::Socket(error) => {
                if started {
                    if let ResponsesStreamError::Transport(transport) = &error {
                        append_diagnostic(request, output, transport, true, &body);
                    }
                    OPENAI_SESSIONS.record_mid_stream_failure(
                        session_id,
                        &error,
                        MID_STREAM_FAILURE_LIMIT,
                    );
                    return Err(socket_error(error));
                }
                if request.probe {
                    // The endpoint does not speak the mode: learn it and serve SSE silently.
                    record_probe_failure(endpoint.as_deref(), session_id);
                    return Ok(WebSocketOutcome::FallBackToSse);
                }
                let ResponsesStreamError::Transport(transport) = &error else {
                    return Err(socket_error(error));
                };
                append_diagnostic(request, output, transport, false, &body);
                OPENAI_SESSIONS.record_failure(session_id, &error);
                OPENAI_SESSIONS.record_sse_fallback(session_id);
                return Ok(WebSocketOutcome::FallBackToSse);
            }
        }
    }
}

fn record_probe_failure(endpoint: Option<&str>, session_id: Option<&str>) {
    if let Some(endpoint) = endpoint {
        record_learned_capability(endpoint, false);
    }
    if let Some(session_id) = session_id {
        note_counts(session_id, |counts| counts.probe_failures += 1);
    }
}

fn append_diagnostic(
    request: &WebSocketRequest<'_>,
    output: &mut AssistantMessage,
    transport: &crate::providers::responses_websocket::errors::WebSocketTransportError,
    events_emitted: bool,
    body: &Value,
) {
    append_transport_failure_diagnostic(
        output,
        transport,
        transport_debug_name(request.options.transport.unwrap_or(Transport::Auto)),
        events_emitted,
        body.to_string().len(),
    );
}

/// The provider error for a socket failure: transport failures keep their close-event surface,
/// an invalid frame surfaces its own text.
fn socket_error(error: ResponsesStreamError) -> ProviderError {
    match error {
        ResponsesStreamError::Protocol(protocol) => ProviderError::Message(protocol.message),
        ResponsesStreamError::Api(_)
        | ResponsesStreamError::Transport(_)
        | ResponsesStreamError::Aborted => error.into_provider_error(),
    }
}

/// The `wss://` (or `ws://`) Responses endpoint for a base URL.
fn websocket_url(base_url: &str) -> String {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url
    }
}

/// Flatten a WebSocket `error` event (`{status, error: {code, message}}`) into the flat shape the
/// shared processor reads over SSE, and return its code.
fn flatten_error_event(event: &mut Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let nested = event.get("error").cloned().unwrap_or(Value::Null);
    for key in ["code", "message"] {
        if event.get(key).is_none() {
            if let Some(value) = nested.get(key) {
                event[key] = value.clone();
            }
        }
    }
    event
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[expect(
    clippy::too_many_lines,
    reason = "one function for the provider's SSE stream section"
)]
async fn run_attempt(
    request: &WebSocketRequest<'_>,
    body: &Value,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
    started: &mut bool,
    allow_chain_reset: bool,
) -> Result<(), AttemptError> {
    let options = request.options;
    let session_id = options.session_id.as_deref();
    let connect_timeout = request.probe.then_some(PROBE_CONNECT_TIMEOUT);
    let connection: AcquiredConnection = OPENAI_SESSIONS
        .acquire(
            &websocket_url(&request.model.base_url),
            request.headers,
            session_id,
            options.signal.clone(),
            connect_timeout,
        )
        .await
        .map_err(AttemptError::Socket)?;
    let use_cached_context = !matches!(options.transport, Some(Transport::Websocket));
    let continuation = match session_id {
        Some(session_id) if use_cached_context && connection.cached => {
            OPENAI_SESSIONS.take_continuation_for(session_id, connection.connection_id)
        }
        _ => None,
    };
    let request_body =
        build_cached_websocket_request_body(continuation.as_ref(), body, connection.connection_id);
    if let Some(session_id) = session_id {
        OPENAI_SESSIONS.record_request_stats(
            session_id,
            connection.reused,
            use_cached_context,
            &request_body,
        );
    }
    let sent = connection
        .send_request(&request_body, options.signal.clone())
        .await;

    let start_partial = output.clone();
    let result: Result<(), AttemptError> = async {
        let mut events = sent.map_err(AttemptError::Socket)?;
        let mut processor =
            ResponsesStreamProcessor::new(request.model, output, writer, (request.hooks)());
        let streamed: Result<(), AttemptError> =
            async {
                while let Some(event) = events.recv().await {
                    match event {
                        WorkerEvent::Event(mut event) => {
                            let code = flatten_error_event(&mut event);
                            if !*started && allow_chain_reset {
                                match code.as_deref() {
                                    Some(STALE_CONTINUATION_CODE) => {
                                        return Err(AttemptError::ChainReset {
                                            code: STALE_CONTINUATION_CODE,
                                        });
                                    }
                                    Some(CONNECTION_LIMIT_CODE) => {
                                        return Err(AttemptError::ChainReset {
                                            code: CONNECTION_LIMIT_CODE,
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            if !*started && is_visible_response_event(&event) {
                                *started = true;
                                writer.push(AssistantMessageEvent::Start {
                                    partial: start_partial.clone(),
                                });
                            }
                            let terminal = event.get("type").and_then(Value::as_str).is_some_and(
                                |event_type| OPENAI_WIRE.terminal_events.contains(&event_type),
                            );
                            processor
                                .handle_event(&event)
                                .map_err(AttemptError::Stream)?;
                            if terminal {
                                break;
                            }
                        }
                        WorkerEvent::Terminal(result) => {
                            result.map_err(AttemptError::Socket)?;
                            break;
                        }
                    }
                }
                processor.finish().map_err(AttemptError::Stream)?;
                if matches!(
                    processor.stop_reason(),
                    StopReason::Aborted | StopReason::Error
                ) {
                    return Err(AttemptError::Stream(ProviderError::StreamFailure(
                        stream_failure_from_stop_reason(processor.stop_reason_raw(), None),
                    )));
                }
                Ok(())
            }
            .await;
        if streamed.is_err() {
            processor.settle_partial_tool_calls();
        }
        streamed
    }
    .await;

    match result {
        Ok(()) => {
            let continuation =
                (use_cached_context && connection.cached && !is_cancelled(options.signal.as_ref()))
                    .then(|| output.response_id.clone())
                    .flatten()
                    .map(|last_response_id| ContinuationState {
                        last_request_body: body.clone(),
                        last_response_id,
                        last_response_items: response_items(request.model, output),
                        connection_id: connection.connection_id,
                    });
            let keep = !is_cancelled(options.signal.as_ref());
            OPENAI_SESSIONS
                .release(connection, keep, continuation)
                .await;
            Ok(())
        }
        Err(error) => {
            if let Some(session_id) = session_id {
                OPENAI_SESSIONS.clear_continuation(session_id, connection.connection_id);
            }
            // An error event leaves the connection usable (the dropped continuation is resent on
            // it in full); a lifetime limit or a socket failure ends it.
            let keep = match &error {
                AttemptError::ChainReset { code } => *code == STALE_CONTINUATION_CODE,
                AttemptError::Stream(_) => true,
                AttemptError::Socket(_) => false,
            };
            OPENAI_SESSIONS.release(connection, keep, None).await;
            Err(error)
        }
    }
}

/// The response's own output items as the next request's input will carry them (tool outputs
/// arrive with the next request's delta).
fn response_items(model: &Model, output: &AssistantMessage) -> Vec<Value> {
    convert_responses_messages(
        model,
        &Context {
            messages: vec![Message::Assistant(output.clone())],
            tools: None,
            system_prompt: None,
        },
        &OPENAI_TOOL_CALL_PROVIDERS,
        ConvertResponsesMessagesOptions {
            include_system_prompt: false,
        },
    )
    .into_iter()
    .filter(|item| item.get("type").and_then(Value::as_str) != Some("function_call_output"))
    .collect()
}

/// Local mock endpoints serving both the Responses WebSocket mode and HTTP+SSE on one port, so a
/// test can tell which transport each request rode.
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Map};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::providers::openai_responses::{stream_openai_responses, OpenAIResponsesOptions};
    use crate::providers::responses_websocket::{session_counts, ResponsesTransportCounts};
    use crate::types::{UserMessage, UserMessageContent};

    /// One accepted WebSocket upgrade: rejected, or answering each `response.create` in order.
    enum WsConnection {
        Reject,
        Serve(Vec<WsReply>),
    }

    enum WsReply {
        Events(Vec<Value>),
        /// Send the events, then drop the socket without a close frame.
        EventsThenDrop(Vec<Value>),
    }

    #[derive(Default)]
    struct Recorded {
        upgrades: AtomicUsize,
        ws_bodies: Mutex<Vec<Value>>,
        http_bodies: Mutex<Vec<Value>>,
    }

    struct MockEndpoint {
        base_url: String,
        recorded: Arc<Recorded>,
    }

    impl MockEndpoint {
        fn upgrades(&self) -> usize {
            self.recorded.upgrades.load(Ordering::SeqCst)
        }
        fn ws_bodies(&self) -> Vec<Value> {
            self.recorded.ws_bodies.lock().unwrap().clone()
        }
        fn http_bodies(&self) -> Vec<Value> {
            self.recorded.http_bodies.lock().unwrap().clone()
        }
    }

    async fn read_head(socket: &mut TcpStream) -> Option<String> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.ok()?;
            head.push(byte[0]);
        }
        Some(String::from_utf8_lossy(&head).to_string())
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    async fn read_client_frame(socket: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
        let mut header = [0u8; 2];
        socket.read_exact(&mut header).await.ok()?;
        let mut length = u64::from(header[1] & 0x7F);
        if length == 126 {
            let mut extended = [0u8; 2];
            socket.read_exact(&mut extended).await.ok()?;
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0u8; 8];
            socket.read_exact(&mut extended).await.ok()?;
            length = u64::from_be_bytes(extended);
        }
        let mut mask = [0u8; 4];
        socket.read_exact(&mut mask).await.ok()?;
        let mut payload = vec![0u8; usize::try_from(length).ok()?];
        socket.read_exact(&mut payload).await.ok()?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        Some((header[0] & 0x0F, payload))
    }

    async fn write_text_frame(socket: &mut TcpStream, payload: &Value) -> std::io::Result<()> {
        let body = payload.to_string();
        let mut frame = vec![0x81u8];
        match u16::try_from(body.len()) {
            Ok(length) if length < 126 => frame.push(u8::try_from(length).unwrap_or_default()),
            Ok(length) => {
                frame.push(126);
                frame.extend_from_slice(&length.to_be_bytes());
            }
            Err(_) => {
                frame.push(127);
                frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(body.as_bytes());
        socket.write_all(&frame).await
    }

    async fn serve_websocket(
        mut socket: TcpStream,
        head: &str,
        connection: WsConnection,
        recorded: &Recorded,
    ) -> std::io::Result<()> {
        use base64::Engine as _;
        use sha1::Digest as _;
        let WsConnection::Serve(replies) = connection else {
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return socket.shutdown().await;
        };
        let key = header(head, "sec-websocket-key").unwrap_or_default();
        let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
            format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
        ));
        socket
            .write_all(
                format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut replies: VecDeque<WsReply> = replies.into();
        while let Some((opcode, payload)) = read_client_frame(&mut socket).await {
            if opcode == 8 {
                socket.write_all(&[0x88, 0x02, 0x03, 0xE8]).await?;
                break;
            }
            let body: Value = serde_json::from_slice(&payload).unwrap_or_default();
            recorded.ws_bodies.lock().unwrap().push(body);
            match replies.pop_front() {
                Some(WsReply::Events(events)) => {
                    for event in &events {
                        write_text_frame(&mut socket, event).await?;
                    }
                }
                Some(WsReply::EventsThenDrop(events)) => {
                    for event in &events {
                        write_text_frame(&mut socket, event).await?;
                    }
                    return socket.shutdown().await;
                }
                None => {}
            }
        }
        Ok(())
    }

    async fn serve_sse(
        mut socket: TcpStream,
        head: &str,
        events: Vec<Value>,
        recorded: &Recorded,
    ) -> std::io::Result<()> {
        let length: usize = header(head, "content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        let mut body = vec![0u8; length];
        socket.read_exact(&mut body).await?;
        recorded
            .http_bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&body).unwrap_or_default());
        let mut response = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        response.extend(events.iter().map(|event| format!("data: {event}\n\n")));
        socket.write_all(response.as_bytes()).await?;
        socket.shutdown().await
    }

    /// Serve upgrades from `websocket` (in accept order; an exhausted list rejects) and HTTP
    /// requests from `sse` (an exhausted list streams nothing).
    async fn spawn_endpoint(websocket: Vec<WsConnection>, sse: Vec<Vec<Value>>) -> MockEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Recorded::default());
        let websocket = Arc::new(Mutex::new(VecDeque::from(websocket)));
        let sse = Arc::new(Mutex::new(VecDeque::from(sse)));
        let state = Arc::clone(&recorded);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(head) = read_head(&mut socket).await else {
                    continue;
                };
                let recorded = Arc::clone(&state);
                if header(&head, "upgrade")
                    .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
                {
                    recorded.upgrades.fetch_add(1, Ordering::SeqCst);
                    let connection = websocket
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(WsConnection::Reject);
                    tokio::spawn(async move {
                        let _ = serve_websocket(socket, &head, connection, &recorded).await;
                    });
                } else {
                    let events = sse.lock().unwrap().pop_front().unwrap_or_default();
                    tokio::spawn(async move {
                        let _ = serve_sse(socket, &head, events, &recorded).await;
                    });
                }
            }
        });
        MockEndpoint {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            recorded,
        }
    }

    fn response_events(response_id: &str, text: &str) -> Vec<Value> {
        let message_id = format!("msg_{response_id}");
        vec![
            json!({ "type": "response.created", "response": { "id": response_id } }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "message", "id": message_id, "role": "assistant", "status": "in_progress", "content": [] },
            }),
            json!({
                "type": "response.content_part.added",
                "output_index": 0,
                "content_index": 0,
                "part": { "type": "output_text", "text": "" },
            }),
            json!({ "type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text }),
            json!({
                "type": "response.output_item.done",
                "item": { "type": "message", "id": message_id, "role": "assistant", "status": "completed", "content": [{ "type": "output_text", "text": text }] },
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "status": "completed",
                    "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8 },
                },
            }),
        ]
    }

    fn model(base_url: &str) -> Model {
        serde_json::from_value(json!({
            "id": "gpt-5.4", "name": "gpt-5.4", "api": "openai-responses", "provider": "openai",
            "baseUrl": base_url, "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 400_000, "maxTokens": 128_000
        }))
        .unwrap()
    }

    /// An endpoint an earlier probe found speaking the WebSocket mode.
    fn learned_websocket_model(endpoint: &MockEndpoint) -> Model {
        record_learned_capability(&endpoint_key(&endpoint.base_url).unwrap(), true);
        model(&endpoint.base_url)
    }

    fn options(session_id: Option<&str>, transport: Option<Transport>) -> OpenAIResponsesOptions {
        OpenAIResponsesOptions::from_base(StreamOptions {
            api_key: Some("test-key".to_string()),
            transport,
            session_id: session_id.map(str::to_string),
            ..Default::default()
        })
    }

    fn user(text: &str, timestamp: u64) -> Message {
        Message::User(UserMessage {
            content: UserMessageContent::Text(text.to_string()),
            timestamp,
            rest: Map::default(),
        })
    }

    fn first_turn() -> Context {
        Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![user("Say hello", 1)],
            tools: None,
        }
    }

    fn follow_up(first: &AssistantMessage) -> Context {
        Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![
                user("Say hello", 1),
                Message::Assistant(first.clone()),
                user("Now finish", 2),
            ],
            tools: None,
        }
    }

    fn text(message: &AssistantMessage) -> Option<String> {
        message.content.iter().find_map(|content| match content {
            crate::types::AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
    }

    fn session(name: &str) -> String {
        format!("{name}-{}", std::process::id())
    }

    async fn run(
        model: &Model,
        context: &Context,
        options: &OpenAIResponsesOptions,
    ) -> AssistantMessage {
        stream_openai_responses(model, context, Some(options))
            .result()
            .await
    }

    /// `auto` continues a session's turn over the WebSocket with only the new input and the
    /// previous response id, while a one-shot request (nothing can continue it) on the same
    /// endpoint rides SSE.
    #[tokio::test]
    async fn auto_continues_a_session_with_only_the_delta_and_keeps_one_shots_on_sse() {
        let endpoint = spawn_endpoint(
            vec![WsConnection::Serve(vec![
                WsReply::Events(response_events("resp_1", "Hello")),
                WsReply::Events(response_events("resp_2", "Done")),
            ])],
            vec![response_events("resp_3", "One shot")],
        )
        .await;
        let model = learned_websocket_model(&endpoint);
        let session = session("ws-delta");
        let first = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let second = run(&model, &follow_up(&first), &options(Some(&session), None)).await;
        let one_shot = run(&model, &first_turn(), &options(None, None)).await;

        let new_input = convert_responses_messages(
            &model,
            &Context {
                system_prompt: None,
                messages: vec![user("Now finish", 2)],
                tools: None,
            },
            &OPENAI_TOOL_CALL_PROVIDERS,
            ConvertResponsesMessagesOptions {
                include_system_prompt: false,
            },
        );
        let bodies = endpoint.ws_bodies();
        assert_eq!(
            (
                text(&second),
                bodies.len(),
                bodies[1].get("previous_response_id"),
                bodies[1].get("input"),
                bodies[0].get("stream"),
                text(&one_shot),
                endpoint.http_bodies().len(),
            ),
            (
                Some("Done".to_string()),
                2,
                Some(&json!("resp_1")),
                Some(&Value::Array(new_input)),
                None,
                Some("One shot".to_string()),
                1,
            )
        );
    }

    /// A `previous_response_not_found` error resends the request once with the full context on
    /// the same connection.
    #[tokio::test]
    async fn a_dropped_continuation_is_resent_once_in_full_on_the_same_connection() {
        let endpoint = spawn_endpoint(
            vec![WsConnection::Serve(vec![
                WsReply::Events(response_events("resp_1", "Hello")),
                WsReply::Events(vec![json!({
                    "type": "error",
                    "status": 400,
                    "error": {
                        "type": "invalid_request_error",
                        "code": "previous_response_not_found",
                        "message": "Previous response with id 'resp_1' not found.",
                    },
                })]),
                WsReply::Events(response_events("resp_2", "Done")),
            ])],
            vec![],
        )
        .await;
        let model = learned_websocket_model(&endpoint);
        let session = session("ws-stale");
        let first = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let second = run(&model, &follow_up(&first), &options(Some(&session), None)).await;
        let bodies = endpoint.ws_bodies();
        let previous_ids: Vec<_> = bodies
            .iter()
            .map(|body| body.get("previous_response_id").cloned())
            .collect();
        assert_eq!(
            (
                second.stop_reason,
                text(&second),
                previous_ids,
                endpoint.upgrades(),
                session_counts(&session).map(|counts| counts.stale_continuation_retries),
            ),
            (
                StopReason::Stop,
                Some("Done".to_string()),
                vec![None, Some(json!("resp_1")), None],
                1,
                Some(1),
            )
        );
    }

    /// A known WebSocket endpoint whose handshake fails serves the request over SSE, and the
    /// session stays on SSE.
    #[tokio::test]
    async fn a_failed_handshake_falls_back_to_sse_for_the_session() {
        let endpoint = spawn_endpoint(
            vec![WsConnection::Reject],
            vec![
                response_events("resp_1", "Hello"),
                response_events("resp_2", "Done"),
            ],
        )
        .await;
        let model = learned_websocket_model(&endpoint);
        let session = session("ws-handshake-fallback");
        let first = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let second = run(&model, &follow_up(&first), &options(Some(&session), None)).await;
        assert_eq!(
            (
                text(&first),
                text(&second),
                endpoint.upgrades(),
                endpoint.http_bodies().len()
            ),
            (Some("Hello".to_string()), Some("Done".to_string()), 1, 2)
        );
    }

    /// `auto` probes an endpoint of unknown capability once: the failed probe falls back to SSE
    /// without a visible error, and the learned verdict spares the next request the probe.
    #[tokio::test]
    async fn an_unknown_endpoint_is_probed_once_and_the_verdict_is_kept() {
        let endpoint = spawn_endpoint(
            vec![],
            vec![
                response_events("resp_1", "Hello"),
                response_events("resp_2", "Again"),
            ],
        )
        .await;
        let model = model(&endpoint.base_url);
        let session = session("ws-probe");
        let first = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let second = run(&model, &first_turn(), &options(Some(&session), None)).await;
        assert_eq!(
            (
                (text(&first), first.diagnostics.is_none()),
                text(&second),
                endpoint.upgrades(),
                endpoint.http_bodies().len(),
                session_counts(&session),
            ),
            (
                (Some("Hello".to_string()), true),
                Some("Again".to_string()),
                1,
                2,
                Some(ResponsesTransportCounts {
                    auto_websocket: 1,
                    auto_sse_unsupported: 1,
                    probes: 1,
                    probe_failures: 1,
                    ..Default::default()
                }),
            )
        );
    }

    /// Two mid-stream WebSocket failures in a row demote the session to SSE.
    #[tokio::test]
    async fn repeated_mid_stream_failures_demote_the_session_to_sse() {
        let partial = || response_events("resp_x", "Partial")[..4].to_vec();
        let endpoint = spawn_endpoint(
            vec![
                WsConnection::Serve(vec![WsReply::EventsThenDrop(partial())]),
                WsConnection::Serve(vec![WsReply::EventsThenDrop(partial())]),
            ],
            vec![response_events("resp_3", "Done")],
        )
        .await;
        let model = learned_websocket_model(&endpoint);
        let session = session("ws-demote");
        let first = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let second = run(&model, &first_turn(), &options(Some(&session), None)).await;
        let third = run(&model, &first_turn(), &options(Some(&session), None)).await;
        assert_eq!(
            (
                first.stop_reason,
                second.stop_reason,
                text(&third),
                endpoint.upgrades(),
                endpoint.http_bodies().len(),
            ),
            (
                StopReason::Error,
                StopReason::Error,
                Some("Done".to_string()),
                2,
                1
            )
        );
    }

    /// An explicit `sse` transport never probes an endpoint of unknown capability.
    #[tokio::test]
    async fn an_explicit_sse_setting_never_probes() {
        let endpoint = spawn_endpoint(vec![], vec![response_events("resp_1", "Hello")]).await;
        let model = model(&endpoint.base_url);
        let session = session("ws-explicit-sse");
        let reply = run(
            &model,
            &first_turn(),
            &options(Some(&session), Some(Transport::Sse)),
        )
        .await;
        assert_eq!(
            (
                text(&reply),
                endpoint.upgrades(),
                endpoint.http_bodies().len()
            ),
            (Some("Hello".to_string()), 0, 1)
        );
    }

    /// A Responses route that answers 404 to a real request after discovery (or the docs) said it
    /// exists is demoted: the endpoint's models load on Chat Completions again.
    #[tokio::test]
    async fn a_missing_responses_route_demotes_the_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = read_head(&mut socket).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        let base_url = format!("http://127.0.0.1:{port}/v1");
        crate::providers::responses_websocket::record_learned_for_test(&base_url, Some(true));
        let model = model(&base_url);
        let reply = run(&model, &first_turn(), &options(None, Some(Transport::Sse))).await;
        let mut completions = model.clone();
        completions.api = "openai-completions".to_string();
        assert_eq!(
            (
                reply.stop_reason,
                crate::providers::responses_websocket::learned_endpoint(&base_url)
                    .and_then(|learned| learned.responses_http),
                crate::providers::responses_websocket::apply_responses_wire_upgrade(
                    &mut completions
                ),
            ),
            (StopReason::Error, Some(false), false)
        );
    }

    /// Background discovery never holds a request: with the probe stuck on an endpoint that
    /// accepts and never answers, a request on another endpoint still completes.
    #[tokio::test]
    async fn a_hanging_probe_never_delays_a_request() {
        let hanging = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hanging_port = hanging.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = hanging.accept().await.unwrap();
            let _ = accepted_tx.send(());
            // Hold the socket open without answering.
            std::future::pending::<()>().await;
            drop(socket);
        });
        crate::providers::responses_websocket::discover_endpoints(
            vec![crate::providers::responses_websocket::DiscoveryTarget {
                base_url: format!("http://127.0.0.1:{hanging_port}/v1"),
                model_id: "m".to_string(),
                wire: crate::providers::responses_websocket::DiscoveryWire::ChatCompletions,
                headers: vec![],
            }],
            crate::utils::prewarm::NetworkMode::Online,
        );
        accepted_rx
            .await
            .expect("the probe reached the hanging endpoint");
        let endpoint = spawn_endpoint(vec![], vec![response_events("resp_1", "Hello")]).await;
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &model(&endpoint.base_url),
                &first_turn(),
                &options(None, Some(Transport::Sse)),
            ),
        )
        .await
        .expect("the request completes while the probe hangs");
        assert_eq!(text(&reply), Some("Hello".to_string()));
    }
}
