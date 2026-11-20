#![cfg(unix)]

//! tmux user-level verifier for the session credential pin: the real `prime-agent` binary runs
//! in a private tmux server against its own daemon and an isolated HOME holding two faux keys
//! for one provider: `key-default` in the default slot (`auth.json`) and `key-work` stored as the
//! named credential `work` (`auth-accounts.json`). Started with `--credential work`, the session
//! answers through a loopback chat-completions endpoint that records every request's bearer: each
//! one carries the pinned key, never the default slot's. The model picker names the account.
//!
//! Every wait polls the captured pane (or the recorded requests) for observable state under a
//! deadline; nothing sleeps for a fixed time.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

const WAIT: Duration = Duration::from_secs(60);

/// A private tmux server (its own socket in the test's temp dir) holding one pane that runs
/// the product; dropped, it shuts the product's daemon down and kills the server.
struct Pane {
    socket: PathBuf,
    launcher: PathBuf,
}

impl Pane {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("tmux runs (install tmux to run this verifier)")
    }

    /// Type `text` literally, then press Enter.
    fn submit(&self, text: &str) {
        assert!(self
            .tmux(&["send-keys", "-t", "main", "-l", text])
            .status
            .success());
        assert!(self
            .tmux(&["send-keys", "-t", "main", "Enter"])
            .status
            .success());
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.tmux(&["capture-pane", "-p", "-t", "main"]).stdout)
            .into_owned()
    }

    /// Poll the pane until `ready` holds; the screen it held then.
    fn wait(&self, what: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if ready(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // The product's daemon outlives its client: stop it through the product's own command.
        let _ = Command::new("sh")
            .arg(&self.launcher)
            .args(["shutdown", "--force"])
            .output();
        let _ = self.tmux(&["kill-server"]);
    }
}

/// A loopback chat-completions endpoint: every POST is answered with one streamed
/// `pinned answer`, and its `Authorization` header is recorded. Runs until the process ends.
fn completions_fixture() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let bearers = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bearers);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                // The head, then the body its Content-Length announces.
                let head_end = loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                let length: usize = header("content-length")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                while request.len() < head_end + length {
                    let Ok(read) = stream.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                if !head.starts_with("POST ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(header("authorization").unwrap_or_default());
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    json!({ "id": "e2e", "object": "chat.completion.chunk", "created": 0,
                            "model": "e2e-model",
                            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
                };
                let mut done = chunk(json!({}), json!("stop"));
                done["usage"] =
                    json!({ "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 });
                let body = format!(
                    "data: {}\n\ndata: {done}\n\ndata: [DONE]\n\n",
                    chunk(
                        json!({ "role": "assistant", "content": "pinned answer" }),
                        serde_json::Value::Null
                    )
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (origin, bearers)
}

/// Start the product with `args` against an agent dir holding both faux keys.
fn start(dir: &Path, origin: &str, args: &str) -> Pane {
    let home = dir.join("home");
    let agent = home.join(".prime/agent");
    let project = dir.join("pin-chat");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        agent.join("settings.json"),
        json!({ "onboardingCompleted": true, "onboardingShown": true,
                "telemetry": { "noticeShown": true },
                "defaultProvider": "openai", "defaultModel": "e2e-model" })
        .to_string(),
    )
    .expect("settings");
    std::fs::write(
        agent.join("models.json"),
        json!({ "providers": { "openai": {
            "baseUrl": format!("{origin}/v1"),
            "models": [{ "id": "e2e-model", "api": "openai-completions" }],
        } } })
        .to_string(),
    )
    .expect("models");
    std::fs::write(
        agent.join("auth.json"),
        json!({ "openai": { "type": "api_key", "key": "key-default" } }).to_string(),
    )
    .expect("auth");
    std::fs::write(
        agent.join("auth-accounts.json"),
        json!({ "version": 1, "providers": { "openai": [{
            "id": "c_00000000e2e0work", "label": "work", "addedAt": 0,
            "credential": { "type": "api_key", "key": "key-work" },
        }] } })
        .to_string(),
    )
    .expect("auth accounts");
    // The pane's whole environment, so nothing ambient (provider keys, the user's HOME, a
    // daemon socket) reaches the product.
    let launcher = dir.join("launch.sh");
    let quote = |path: &Path| format!("'{}'", path.display());
    std::fs::write(
        &launcher,
        format!(
            "cd {project}\nexec env -i PATH=/usr/bin:/bin HOME={home} TERM=xterm-256color \
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1 PRIME_AGENT_DAEMON_SOCKET={socket} \
             {binary} \"$@\"\n",
            project = quote(&project),
            home = quote(&home),
            socket = quote(&dir.join("daemon.sock")),
            binary = quote(Path::new(env!("CARGO_BIN_EXE_prime-agent"))),
        ),
    )
    .expect("launcher");
    let pane = Pane {
        socket: dir.join("tmux.sock"),
        launcher,
    };
    let command = format!("sh {} {args}", quote(&pane.launcher));
    let started = pane.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "120",
        "-y",
        "30",
        &command,
    ]);
    assert!(
        started.status.success(),
        "tmux new-session: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    pane
}

#[test]
fn a_session_pinned_to_a_named_credential_sends_only_that_key() {
    let dir = tempfile::TempDir::new_in(std::env::temp_dir().canonicalize().expect("temp root"))
        .expect("temp dir");
    let (origin, bearers) = completions_fixture();
    let pane = start(dir.path(), &origin, "--credential work");
    pane.wait("the composer", |screen| screen.contains("e2e-model"));
    pane.submit("hello");
    pane.wait("the answer", |screen| screen.contains("pinned answer"));
    let sent = bearers.lock().unwrap().clone();
    assert!(!sent.is_empty(), "no request reached the fixture");
    assert!(
        sent.iter().all(|bearer| bearer == "Bearer key-work"),
        "every request carries the pinned key: {sent:?}"
    );
    pane.submit("/model");
    pane.wait("the picker's account", |screen| {
        screen
            .lines()
            .any(|line| line.contains("e2e-model") && line.contains("account: work"))
    });
}
