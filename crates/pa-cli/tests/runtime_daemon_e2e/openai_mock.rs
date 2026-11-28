//! A loopback `openai-completions` provider whose answer to each request a
//! test chooses: a streamed reply with the usage it bills, or a server
//! error. It keeps every request's JSON body.

use std::fmt::Write as _;
use std::io::{BufRead as _, Read as _, Write as _};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// What the provider answers one request with.
pub(super) enum Answer {
    /// A `200` stream of one assistant `content` delta, billing `usage`.
    Reply { content: String, usage: Value },
    /// A `500` with an OpenAI-style error body.
    Unavailable,
}

/// Chooses the answer to the request at `index` (counted from 0) with
/// `body`.
type Answerer = dyn Fn(usize, &Value) -> Answer + Send + Sync;

pub(super) struct OpenAiMock {
    pub(super) port: u16,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl OpenAiMock {
    pub(super) fn start(answer: impl Fn(usize, &Value) -> Answer + Send + Sync + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let kept = Arc::clone(&bodies);
        let answer: Arc<Answerer> = Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (kept, answer) = (Arc::clone(&kept), Arc::clone(&answer));
                std::thread::spawn(move || serve(stream, &kept, answer.as_ref()));
            }
        });
        OpenAiMock { port, bodies }
    }

    /// The provider's `baseUrl`.
    pub(super) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The bodies of the requests it received, in order.
    pub(super) fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

/// A model entry of `models.json` on this provider's API.
pub(super) fn model(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name, "api": "openai-completions",
            "contextWindow": 128_000, "maxTokens": 4096 })
}

/// The environment a runtime daemon needs to reach a loopback provider.
pub(super) const LOOPBACK: &[(&str, &str)] =
    &[("NO_PROXY", "127.0.0.1"), ("no_proxy", "127.0.0.1")];

fn serve(stream: std::net::TcpStream, bodies: &Mutex<Vec<Value>>, answer: &Answerer) {
    let mut reader = std::io::BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let index = {
        let mut bodies = bodies.lock().unwrap();
        bodies.push(body.clone());
        bodies.len() - 1
    };
    let response = match answer(index, &body) {
        Answer::Reply { content, usage } => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
            sse(&content, &usage)
        ),
        Answer::Unavailable => {
            let error = json!({ "error": { "message": "provider unavailable" } }).to_string();
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{error}",
                error.len()
            )
        }
    };
    let _ = reader.get_mut().write_all(response.as_bytes());
}

/// One chunk of a completion stream.
fn chunk(choices: &Value, usage: &Value) -> String {
    json!({
        "id": "chatcmpl-test", "object": "chat.completion.chunk",
        "created": 1_750_000_000, "model": "mock-1",
        "choices": choices, "usage": usage,
    })
    .to_string()
}

/// The stream of one assistant reply: its content delta, the finish and the
/// terminal usage frame.
fn sse(content: &str, usage: &Value) -> String {
    let mut stream = String::new();
    for data in [
        chunk(
            &json!([{ "index": 0, "delta": { "role": "assistant", "content": content },
                       "finish_reason": null }]),
            &small_usage(),
        ),
        chunk(
            &json!([{ "index": 0, "delta": {}, "finish_reason": "stop" }]),
            usage,
        ),
        chunk(&json!([]), usage),
    ] {
        write!(stream, "data: {data}\n\n").unwrap();
    }
    stream.push_str("data: [DONE]\n\n");
    stream
}

/// The usage a small reply bills.
pub(super) fn small_usage() -> Value {
    json!({ "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "prompt_tokens_details": { "cached_tokens": 0 } })
}
