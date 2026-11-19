//! Provider pinning e2e: a session whose model id two configured providers
//! serve stays on its own provider. A spent retry budget fails the turn
//! there; no request ever reaches the other provider.
#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Supervisor {
    child: Child,
    #[allow(dead_code)]
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

/// A mock OpenAI-completions provider: the first `failures` requests get a
/// 500 with an OpenAI-style error body; the rest get one fixed SSE answer.
struct FailingMock {
    requests: Arc<Mutex<usize>>,
    port: u16,
}

impl FailingMock {
    fn start(failures: usize, answer: &'static str) -> FailingMock {
        let requests = Arc::new(Mutex::new(0usize));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("mock addr").port();
        let requests_for_thread = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let requests = Arc::clone(&requests_for_thread);
                std::thread::spawn(move || {
                    let _ = serve(stream, failures, answer, &requests);
                });
            }
        });
        FailingMock { requests, port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn count(&self) -> usize {
        *self.requests.lock().expect("mock lock")
    }
}

fn chunk(delta: &Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1_750_000_000,
        "model": "mock-1",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
    .to_string()
}

fn serve(
    mut stream: TcpStream,
    failures: usize,
    answer: &str,
    requests: &Arc<Mutex<usize>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let mut content_length = 0usize;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or_default();
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_bytes)?;
    }
    let index = {
        let mut requests = requests.lock().expect("mock lock");
        *requests += 1;
        *requests
    };
    if index <= failures {
        let body = json!({
            "error": { "message": "mock provider overloaded", "type": "server_error", "code": 500 }
        })
        .to_string();
        return stream.write_all(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        );
    }
    let delta = json!({"role":"assistant","content":answer});
    let mut payload = String::new();
    for data in [chunk(&delta, None), chunk(&json!({}), Some("stop"))] {
        write!(payload, "data: {data}\n\n").expect("write to String");
    }
    payload.push_str("data: [DONE]\n\n");
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{payload}"
        )
        .as_bytes(),
    )
}

#[allow(clippy::zombie_processes)]
fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Supervisor {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`, which a killed
        // worker never cleans: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if std::env::var_os("PA_PINNING_E2E_DEBUG").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .env_remove("PRIME_API_KEY")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Supervisor {
                child,
                socket: socket.to_path_buf(),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

/// One client connection: request/response plus the session events that stream while a response is
/// outstanding.
struct Client {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    events: Vec<Value>,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        let writer = stream.try_clone().expect("clone");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
            events: Vec::new(),
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_mins(1);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .unwrap_or_else(|error| panic!("write command {id}: {error}"));
    }

    fn request(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_mins(3);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
            self.collect_event(&line);
        }
    }

    fn collect_event(&mut self, line: &Value) {
        if line.get("type").and_then(Value::as_str) == Some("session_event") {
            self.events.push(line["event"].clone());
        }
    }

    fn drain_events(&mut self, quiet_ms: Duration) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last_line = Instant::now();
        let mut line = String::new();
        loop {
            assert!(Instant::now() < deadline, "event drain timed out");
            self.reader
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("timeout");
            match self.reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let value: Value = serde_json::from_str(line.trim()).expect("parse line");
                    self.collect_event(&value);
                    last_line = Instant::now();
                    line.clear();
                }
                Err(_) => {
                    if last_line.elapsed() >= quiet_ms {
                        return;
                    }
                }
            }
        }
    }
}

/// Shared harness: supervisor + a two-provider models.json (both serving
/// `mock-1`) + fast retry settings carrying a legacy `retry.failover`
/// block + a created, attached session on the first provider.
fn setup(
    name: &str,
    primary: &FailingMock,
    backup: &FailingMock,
) -> (tempfile::TempDir, Supervisor, Client, String) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "prime-inference": {
                    "api": "openai-completions",
                    "baseUrl": primary.url(),
                    "apiKey": "sk-primary",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "api": "openai-completions",
                            "contextWindow": 128_000,
                            "maxTokens": 4096
                        }
                    ]
                },
                "prime-backup": {
                    "api": "openai-completions",
                    "baseUrl": backup.url(),
                    "apiKey": "sk-backup",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1 (backup)",
                            "api": "openai-completions",
                            "contextWindow": 128_000,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
    // One fast quick retry. The retired `retry.failover` block still loads
    // (existing settings files carry it) and changes nothing.
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({
            "retry": {
                "enabled": true,
                "maxRetries": 1,
                "baseDelayMs": 50,
                "failover": { "enabled": true, "maxRetries": 1, "baseDelayMs": 50 }
            }
        })
        .to_string(),
    )
    .expect("write settings.json");
    let socket = dir.path().join(format!("{name}.sock"));
    let supervisor = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    client.send_command(
        "c1",
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": session_dir.to_string_lossy(),
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    let created = client.request("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();
    client.send_command(
        "a1",
        &json!({ "type": "attach", "activeSessionId": session_id }),
    );
    let attached = client.request("a1");
    assert_eq!(attached["success"], true, "attach failed: {attached}");
    (dir, supervisor, client, session_id)
}

fn retry_starts(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("auto_retry_start"))
        .collect()
}

fn retry_end(events: &[Value]) -> &Value {
    events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("auto_retry_end"))
        .expect("auto_retry_end")
}

/// A spent retry budget on the session's provider fails the turn there:
/// the other provider serving the same model id never sees a request.
#[test]
fn a_spent_retry_budget_fails_the_turn_on_the_session_provider() {
    let primary = FailingMock::start(usize::MAX, "never reached");
    let other = FailingMock::start(0, "answered by the other provider");
    let (_dir, _supervisor, mut client, session_id) = setup("pinned", &primary, &other);
    client.send_command(
        "p1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": session_id, "message": "hi" }),
    );
    let done = client.request("p1");
    client.drain_events(Duration::from_secs(1));

    let starts = retry_starts(&client.events);
    let end = retry_end(&client.events);
    assert_eq!(
        (
            done["success"].clone(),
            primary.count(),
            other.count(),
            starts.len(),
            end["success"].clone(),
            end["attempt"].clone(),
        ),
        (json!(false), 2, 0, 1, json!(false), json!(1)),
        "the turn fails on the session provider after its one retry: {:?}",
        client.events
    );
    assert!(
        starts
            .iter()
            .all(|start| start.get("reason").is_none() && start.get("backupModel").is_none()),
        "no retry names another provider: {starts:?}"
    );
    let turn_end = client
        .events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("turn_end"))
        .expect("turn_end");
    assert_eq!(
        (
            turn_end["message"]["provider"].clone(),
            turn_end["message"]["stopReason"].clone(),
        ),
        (json!("prime-inference"), json!("error")),
        "turn_end: {turn_end}"
    );
}
