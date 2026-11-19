//! Background endpoint discovery: zero-cost probes that learn whether a catalog endpoint serves
//! the Responses API over HTTP and its WebSocket mode, so `auto` can use them without a user
//! setting. Probes never generate (no billed request):
//!
//! - Responses over HTTP: `POST {base}/responses` with an empty `{}` body. A validation answer
//!   (400 or 422 with a JSON error body, e.g. `OpenAI`'s `missing_required_parameter`) means the
//!   route exists; 404 or 405 means it does not; anything else (401/403/429/5xx, a non-JSON 400,
//!   a network failure) is inconclusive and records nothing.
//! - The WebSocket mode: the handshake on `wss://{base}/responses`, then one `response.create`
//!   with `generate: false` (the documented warmup that prepares state without generating), then
//!   close. Any Responses event (a created/completed response or an `error` event) means the mode
//!   answers; a refused upgrade means it does not; a timeout is inconclusive.
//!
//! Discovery runs on a spawned task the caller never awaits, one endpoint at a time with a pause
//! between probes, skips endpoints checked within the learned-record TTL, and sends nothing
//! offline (`PI_OFFLINE`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use super::capability::{endpoint_key, is_verified_endpoint, learned_endpoint, record_learned};
use super::connection::{spawn_connection_worker, WireProfile, WorkerEvent};
use super::errors::ResponsesStreamError;
use crate::utils::prewarm::NetworkMode;
use crate::utils_inner::http::{send, RequestOptions, Transport as HttpTransport};
use crate::utils_inner::stream_failure::ConnectionErrorProfile;

/// The bound on each probe step (connect, answer).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between two endpoints' probes (discovery never bursts).
const PROBE_SPACING: Duration = Duration::from_millis(500);

/// The warmup probe ends at the first Responses event of any kind.
const PROBE_WIRE: WireProfile = WireProfile {
    label: "Responses probe",
    terminal_events: &[
        "response.created",
        "response.completed",
        "response.incomplete",
        "response.failed",
        "error",
    ],
};

/// The wire an endpoint is reached over today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryWire {
    /// Chat Completions: probe the Responses route, then its WebSocket mode when it exists.
    ChatCompletions,
    /// The Responses API: probe its WebSocket mode.
    Responses,
}

/// One endpoint to check: a base URL the user has credentials for, a model id it serves (the
/// warmup names one), and the request headers (credentials) its requests carry.
#[derive(Clone)]
pub struct DiscoveryTarget {
    pub base_url: String,
    pub model_id: String,
    pub wire: DiscoveryWire,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for DiscoveryTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryTarget")
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("wire", &self.wire)
            .field("headers", &self.headers.len())
            .finish()
    }
}

/// Process-wide discovery counts for the adoption telemetry (one worker process serves one
/// session); counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoveryCounts {
    /// Probes sent (HTTP route checks and WebSocket warmups).
    pub probes: u64,
    /// Probes that found the Responses route.
    pub found_responses: u64,
    /// Probes that found the WebSocket mode.
    pub found_websocket: u64,
    /// Built-in models moved from Chat Completions to the Responses wire.
    pub upgrades: u64,
    /// Upgraded endpoints demoted after the Responses route failed.
    pub demotions: u64,
}

static PROBES: AtomicU64 = AtomicU64::new(0);
static FOUND_RESPONSES: AtomicU64 = AtomicU64::new(0);
static FOUND_WEBSOCKET: AtomicU64 = AtomicU64::new(0);
static UPGRADES: AtomicU64 = AtomicU64::new(0);
static DEMOTIONS: AtomicU64 = AtomicU64::new(0);

/// The discovery counts so far in this process.
#[must_use]
pub fn discovery_counts() -> DiscoveryCounts {
    DiscoveryCounts {
        probes: PROBES.load(Ordering::Relaxed),
        found_responses: FOUND_RESPONSES.load(Ordering::Relaxed),
        found_websocket: FOUND_WEBSOCKET.load(Ordering::Relaxed),
        upgrades: UPGRADES.load(Ordering::Relaxed),
        demotions: DEMOTIONS.load(Ordering::Relaxed),
    }
}

/// What one probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeVerdict {
    Supported,
    Unsupported,
    Inconclusive,
}

impl ProbeVerdict {
    fn known(self) -> Option<bool> {
        match self {
            ProbeVerdict::Supported => Some(true),
            ProbeVerdict::Unsupported => Some(false),
            ProbeVerdict::Inconclusive => None,
        }
    }
}

/// Classify the answer to `POST {base}/responses` with an empty body.
pub(crate) fn classify_route_answer(status: u16, body: &str) -> ProbeVerdict {
    match status {
        404 | 405 => ProbeVerdict::Unsupported,
        400 | 422 => {
            let validation = serde_json::from_str::<Value>(body).is_ok_and(|value| {
                value.get("error").is_some_and(Value::is_object) || value.get("detail").is_some()
            });
            if validation {
                ProbeVerdict::Supported
            } else {
                ProbeVerdict::Inconclusive
            }
        }
        _ => ProbeVerdict::Inconclusive,
    }
}

fn responses_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

async fn probe_route(target: &DiscoveryTarget) -> ProbeVerdict {
    PROBES.fetch_add(1, Ordering::Relaxed);
    let mut headers = target.headers.clone();
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    let request = send(RequestOptions {
        method: reqwest::Method::POST,
        url: responses_url(&target.base_url),
        headers,
        body: Some("{}".to_string()),
        signal: None,
        timeout_ms: u64::try_from(PROBE_TIMEOUT.as_millis()).ok(),
        connection: ConnectionErrorProfile::Sdk,
        transport: HttpTransport::Http1,
    });
    let Ok(Ok(mut response)) = tokio::time::timeout(PROBE_TIMEOUT, request).await else {
        return ProbeVerdict::Inconclusive;
    };
    let body = tokio::time::timeout(PROBE_TIMEOUT, response.read_all_text())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    classify_route_answer(response.status, &body)
}

/// The `ws://`/`wss://` URL of a base URL's Responses route.
pub(crate) fn websocket_url(base_url: &str) -> String {
    let url = responses_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url
    }
}

async fn probe_websocket(target: &DiscoveryTarget) -> ProbeVerdict {
    PROBES.fetch_add(1, Ordering::Relaxed);
    let connected = spawn_connection_worker(
        &websocket_url(&target.base_url),
        &target.headers,
        None,
        PROBE_WIRE,
        Some(PROBE_TIMEOUT),
    )
    .await;
    let worker = match connected {
        Ok((worker, _)) => worker,
        Err(ResponsesStreamError::Transport(error)) if error.close_code().is_none() => {
            // A refused or failed handshake: timeouts stay inconclusive.
            return if error.message().ends_with("Connection timed out") {
                ProbeVerdict::Inconclusive
            } else {
                ProbeVerdict::Unsupported
            };
        }
        Err(_) => return ProbeVerdict::Inconclusive,
    };
    let connection = super::connection::AcquiredConnection {
        worker,
        session_id: None,
        reused: false,
        cached: false,
        connection_id: 0,
    };
    let warmup =
        json!({ "model": target.model_id, "input": [], "store": false, "generate": false });
    let verdict = match connection.send_request(&warmup, None).await {
        Ok(mut events) => match tokio::time::timeout(PROBE_TIMEOUT, events.recv()).await {
            Ok(Some(WorkerEvent::Event(_))) => ProbeVerdict::Supported,
            Ok(Some(WorkerEvent::Terminal(_)) | None) | Err(_) => ProbeVerdict::Inconclusive,
        },
        Err(_) => ProbeVerdict::Inconclusive,
    };
    connection.close().await;
    verdict
}

/// Probe one endpoint and record what it found.
pub(crate) async fn discover_endpoint(target: &DiscoveryTarget) {
    let Some(key) = endpoint_key(&target.base_url) else {
        return;
    };
    let mut responses_http = match target.wire {
        DiscoveryWire::Responses => Some(true),
        DiscoveryWire::ChatCompletions => probe_route(target).await.known(),
    };
    if responses_http == Some(true) && target.wire == DiscoveryWire::ChatCompletions {
        FOUND_RESPONSES.fetch_add(1, Ordering::Relaxed);
    }
    let websocket = if responses_http == Some(true) {
        probe_websocket(target).await.known()
    } else {
        None
    };
    if websocket == Some(true) {
        FOUND_WEBSOCKET.fetch_add(1, Ordering::Relaxed);
    }
    if target.wire == DiscoveryWire::Responses {
        // The route is the provider's own wire; only a finding about it is recorded.
        responses_http = None;
    }
    if responses_http.is_none() && websocket.is_none() {
        return;
    }
    record_learned(&key, |learned| {
        if responses_http.is_some() {
            learned.responses_http = responses_http;
        }
        if websocket.is_some() {
            learned.websocket = websocket;
        }
    });
}

/// Check the endpoints whose capabilities are unknown or stale on a spawned task, one at a time;
/// returns at once (the caller never waits on a probe). Endpoints are deduplicated by host.
pub fn discover_endpoints(targets: Vec<DiscoveryTarget>, network: NetworkMode) {
    if network == NetworkMode::Offline {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        let mut seen = std::collections::HashSet::new();
        for target in targets {
            let Some(key) = endpoint_key(&target.base_url) else {
                continue;
            };
            if !seen.insert(key)
                || is_verified_endpoint(&target.base_url)
                || learned_endpoint(&target.base_url).is_some()
            {
                continue;
            }
            discover_endpoint(&target).await;
            tokio::time::sleep(PROBE_SPACING).await;
        }
    });
}

/// Move a built-in Chat Completions model onto the Responses wire when its endpoint is known to
/// serve the Responses route (docs-verified or discovered). Returns whether the model moved.
///
/// The switch is safe only for a plain Chat Completions model: any `compat` object marks a
/// provider quirk the Responses path cannot carry (`thinkingFormat` reasoning fields,
/// `maxTokensField`, OpenRouter/Vercel routing, Anthropic-style cache control, tool-result name
/// rules), so a model with one stays put. Messages, tool calls, images and reasoning effort map
/// one to one between the two wires (`openai_completions` / `openai_responses_shared`). The
/// caller applies this to built-in catalog models only (a models.json model keeps its declared
/// API), when a session's registry loads, so a running session never switches.
pub fn apply_responses_wire_upgrade(model: &mut crate::types::Model) -> bool {
    if model.api != "openai-completions" || model.compat.is_some() {
        return false;
    }
    let route = is_verified_endpoint(&model.base_url)
        || learned_endpoint(&model.base_url).and_then(|learned| learned.responses_http)
            == Some(true);
    if !route {
        return false;
    }
    model.api = "openai-responses".to_string();
    if let Some(key) = endpoint_key(&model.base_url) {
        if upgraded_endpoints()
            .lock()
            .is_ok_and(|mut seen| seen.insert(key))
        {
            UPGRADES.fetch_add(1, Ordering::Relaxed);
        }
    }
    true
}

fn upgraded_endpoints() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// The endpoint's Responses route answered 404/405 to a real request: record it missing, so the
/// next registry load keeps (or puts back) its models on Chat Completions.
pub(crate) fn demote_responses_route(base_url: &str) {
    let Some(key) = endpoint_key(base_url) else {
        return;
    };
    if learned_endpoint(base_url).and_then(|learned| learned.responses_http) != Some(true) {
        return;
    }
    DEMOTIONS.fetch_add(1, Ordering::Relaxed);
    record_learned(&key, |learned| {
        learned.responses_http = Some(false);
        learned.websocket = Some(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A validation answer means the route exists, 404/405 that it does not, and every other
    /// answer is inconclusive.
    #[test]
    fn route_answers_classify_by_status_and_error_shape() {
        let openai_validation = r#"{"error":{"message":"Missing required parameter: 'model'.","type":"invalid_request_error","param":"model","code":"missing_required_parameter"}}"#;
        let fastapi_validation = r#"{"detail":[{"loc":["body","model"],"msg":"Field required"}]}"#;
        assert_eq!(
            [
                classify_route_answer(400, openai_validation),
                classify_route_answer(422, fastapi_validation),
                classify_route_answer(404, r#"{"error":"Not Found"}"#),
                classify_route_answer(405, ""),
                classify_route_answer(400, "Bad Request"),
                classify_route_answer(401, openai_validation),
                classify_route_answer(429, openai_validation),
                classify_route_answer(500, ""),
            ],
            [
                ProbeVerdict::Supported,
                ProbeVerdict::Supported,
                ProbeVerdict::Unsupported,
                ProbeVerdict::Unsupported,
                ProbeVerdict::Inconclusive,
                ProbeVerdict::Inconclusive,
                ProbeVerdict::Inconclusive,
                ProbeVerdict::Inconclusive,
            ]
        );
    }

    /// The WebSocket probe sends one `response.create` warmup with `generate: false` (never a
    /// generating request), and an answering endpoint is learned as speaking the mode.
    #[tokio::test]
    async fn the_websocket_probe_sends_only_a_non_generating_warmup() {
        use base64::Engine as _;
        use sha1::Digest as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames_tx, frames_rx) = tokio::sync::oneshot::channel::<Vec<Value>>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let key = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim().to_string())
                })
                .unwrap();
            let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
                format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
            ));
            socket
                .write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut frames = Vec::new();
            loop {
                let mut header = [0u8; 2];
                if socket.read_exact(&mut header).await.is_err() {
                    break;
                }
                let length = usize::from(header[1] & 0x7F);
                let mut mask = [0u8; 4];
                socket.read_exact(&mut mask).await.unwrap();
                let mut payload = vec![0u8; length];
                socket.read_exact(&mut payload).await.unwrap();
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
                if header[0] & 0x0F == 8 {
                    break;
                }
                frames.push(serde_json::from_slice(&payload).unwrap());
                let created = br#"{"type":"response.created","response":{"id":"resp_warm"}}"#;
                let mut frame = vec![0x81u8, u8::try_from(created.len()).unwrap()];
                frame.extend_from_slice(created);
                socket.write_all(&frame).await.unwrap();
            }
            let _ = frames_tx.send(frames);
        });
        let base_url = format!("http://127.0.0.1:{port}/v1");
        discover_endpoint(&DiscoveryTarget {
            base_url: base_url.clone(),
            model_id: "gpt-warm".to_string(),
            wire: DiscoveryWire::Responses,
            headers: vec![],
        })
        .await;
        assert_eq!(
            (
                frames_rx.await.unwrap(),
                learned_endpoint(&base_url).and_then(|learned| learned.websocket),
            ),
            (
                vec![json!({
                    "type": "response.create",
                    "model": "gpt-warm",
                    "input": [],
                    "store": false,
                    "generate": false,
                })],
                Some(true),
            )
        );
    }
}
