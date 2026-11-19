//! The opt-in quota fallback through the engine's own wiring: a
//! quota-exhausted turn on a provider whose models.json `auth` block
//! declares `fallback` retries once on the fallback credential and
//! transport, the session keeping its provider and model.
use std::io::{Read as _, Write as _};

use super::*;

/// A loopback inference server answering from a queue, recording each
/// request's path and `Authorization` header. Nothing leaves loopback.
struct InferenceServer {
    port: u16,
    routes: std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
}

impl InferenceServer {
    fn start(responses: Vec<Vec<u8>>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let queue = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            responses,
        )));
        let log = std::sync::Arc::clone(&routes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let log = std::sync::Arc::clone(&log);
                let queue = std::sync::Arc::clone(&queue);
                std::thread::spawn(move || {
                    // Read the head and the whole body before answering.
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 8192];
                    let (head, body_start) = loop {
                        let Ok(read) = stream.read(&mut buffer) else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..read]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break (String::from_utf8_lossy(&bytes[..end]).to_string(), end + 4);
                        }
                    };
                    let header = |name: &str| {
                        head.lines().skip(1).find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.trim()
                                .eq_ignore_ascii_case(name)
                                .then(|| value.trim().to_string())
                        })
                    };
                    let length: usize = header("content-length")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    while bytes.len() < body_start + length {
                        let Ok(read) = stream.read(&mut buffer) else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..read]);
                    }
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    log.lock().unwrap().push((path, header("authorization")));
                    let response = queue.lock().unwrap().pop_front().unwrap_or_else(|| {
                        b"HTTP/1.1 500 Drained\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    });
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                });
            }
        });
        Self { port, routes }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// The free tier's quota answer: a 429 `FreeUsageLimitError` whose reset is
/// 17,209 s away (beyond the retry cap).
fn free_usage_exhausted() -> Vec<u8> {
    let body = json!({"type": "error", "error": {"type": "FreeUsageLimitError",
        "message": "429 Free usage limit reached. Free models work in OpenCode; subscribe to OpenCode Go"}})
    .to_string();
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 17209\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A complete openai-completions SSE reply.
fn sse_reply(text: &str) -> Vec<u8> {
    let delta = json!({"id":"r","object":"chat.completion.chunk","created":0,"model":"hy4-flash-free",
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    let done = json!({"id":"r","object":"chat.completion.chunk","created":0,"model":"hy4-flash-free",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {delta}\n\ndata: {done}\n\ndata: [DONE]\n\n"
    )
    .into_bytes()
}

/// The engine's turn on the built-in `opencode` provider whose free quota
/// is exhausted: its models.json override declares the `opencode-go`
/// fallback, whose stored key serves exactly one immediate retry on the
/// `opencode-go` transport. The turn succeeds under the fallback notice,
/// with no failure, on the session's own provider and model.
#[test]
fn a_quota_exhausted_turn_retries_once_on_the_declared_fallback() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let server = InferenceServer::start(vec![free_usage_exhausted(), sse_reply("ok")]);
    let model = json!([{ "id": "hy4-flash-free", "api": "openai-completions" }]);
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": {
            "opencode": {
                "baseUrl": server.url("/zen/v1"),
                "auth": { "strategy": "api-key", "fallback": ["opencode-go"] },
                "models": model,
            },
            "opencode-go": { "baseUrl": server.url("/zen/go/v1"), "models": model },
        } })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("auth.json"),
        json!({
            "opencode": { "type": "api_key", "key": "free-key" },
            "opencode-go": { "type": "api_key", "key": "go-key" },
        })
        .to_string(),
    )
    .unwrap();
    let engine = std::sync::Arc::new(
        AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: Some("opencode".to_string()),
            model: Some("hy4-flash-free".to_string()),
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: Some(true),
            cron_store: None,
            queued_steering_probe: None,
        })
        .expect("engine"),
    );
    engine.register_arc();
    let mut events = Vec::new();
    admit(&engine, "hello".to_string(), &mut events);
    // The retry rows and the settle, with the reset clock masked (it is
    // relative to now).
    let outcome: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => Some(format!(
                "start {attempt}/{max_attempts} in {delay_ms}ms: {}",
                error_message
                    .chars()
                    .map(|c| if c.is_ascii_digit() { '#' } else { c })
                    .collect::<String>()
            )),
            EngineEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => Some(format!("end {success} at {attempt}: {final_error:?}")),
            EngineEvent::AssistantMessage(message) => Some(format!(
                "assistant {}/{} {}",
                message["provider"].as_str().unwrap_or_default(),
                message["model"].as_str().unwrap_or_default(),
                message["stopReason"].as_str().unwrap_or_default()
            )),
            EngineEvent::Done(result) => Some(format!("done {result:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        (outcome, server.routes.lock().unwrap().clone()),
        (
            vec![
                "assistant opencode/hy4-flash-free error".to_string(),
                "start 1/3 in 0ms: using opencode-go credential (opencode usage limit reached until ##:## UTC)".to_string(),
                "assistant opencode/hy4-flash-free stop".to_string(),
                "end true at 1: None".to_string(),
                "done Ok(())".to_string(),
            ],
            vec![
                (
                    "/zen/v1/chat/completions".to_string(),
                    Some("Bearer free-key".to_string())
                ),
                (
                    "/zen/go/v1/chat/completions".to_string(),
                    Some("Bearer go-key".to_string())
                ),
            ],
        )
    );
}
