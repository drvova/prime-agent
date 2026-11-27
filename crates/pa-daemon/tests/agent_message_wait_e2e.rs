//! `agent_message.wait` end to end on a live supervisor: two sibling roots, scripted kernel
//! cells. Alpha parks on beta; beta's wait on alpha answers `cycle` at once; beta's direct
//! message ends alpha's wait with a preview while the body reaches alpha's context exactly once;
//! a wait nobody answers times out.
#![cfg(unix)]
#![expect(
    clippy::too_many_lines,
    reason = "the wait, cycle and timeout scenario reads top to bottom"
)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.0);
    }
}

fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    let log = std::fs::File::create(socket.with_extension("daemon.log")).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        // Worker sockets stay in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        // Every roster push lands late (the debug-only gap seam): beta's last wait walks the
        // supervisor's roster right after its send returns, so it reads alpha's withdrawn
        // `waitingOn` only if the delivery published it before acknowledging.
        .env("PA_WORKER_ROSTER_PUSH_GAP_MS", "400")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(15);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon(child)
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("read timeout");
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
            next_id: 0,
        };
        assert_eq!(client.read_line()["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .expect("read supervisor line");
            assert!(read > 0, "supervisor closed the connection");
            if !line.trim().is_empty() {
                return serde_json::from_str(line.trim()).expect("parse line");
            }
        }
    }

    fn request(&mut self, command: &Value) -> Value {
        self.next_id += 1;
        let id = format!("c{}", self.next_id);
        let mut line = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        })
        .to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        loop {
            let frame = self.read_line();
            if frame["id"] == id {
                assert_eq!(frame["success"], true, "{command} failed: {frame}");
                return frame["data"].clone();
            }
        }
    }
}

/// One scripted turn: the cell, then the text that closes it.
fn cell_turn(code: &str) -> [Value; 2] {
    [
        json!({ "content": [
            { "type": "toolCall", "name": "eval", "arguments": { "code": code } },
        ] }),
        json!({ "text": "cell turn done" }),
    ]
}

/// A form that evaluates `call` and writes its result (or error) to `<name>.json`
/// (`.error`), with the call's wall time in ms beside the result.
fn record(call: &str, name: &str, dir: &Path) -> String {
    let path = dir.join(format!("{name}.json")).display().to_string();
    let error = dir.join(format!("{name}.error")).display().to_string();
    format!(
        r"(with-handler
  (lambda (e)
    (fs-write {error:?} (to-string e))
    (error (to-string e)))
  (let* ([started (current-milliseconds)]
         [result (hash-insert {call} 'elapsedMs (- (current-milliseconds) started))])
    (fs-write {path:?} (value->jsexpr-string result))))
"
    )
}

/// The recorded result once it lands (an `.error` record fails at once).
fn recorded(dir: &Path, name: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(error) = std::fs::read_to_string(dir.join(format!("{name}.error"))) {
            panic!("{name} raised: {error}");
        }
        if let Ok(content) = std::fs::read_to_string(dir.join(format!("{name}.json"))) {
            return serde_json::from_str(&content).expect("record json");
        }
        assert!(Instant::now() < deadline, "{name} never recorded");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn create(client: &mut Client, dir: &Path, name: &str, responses: &[Value]) -> String {
    let script = dir.join(format!("{name}.json"));
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .expect("write script");
    let created = client.request(&json!({
        "type": "create",
        "name": name,
        "config": {
            "cwd": dir.to_string_lossy(),
            "sessionDir": dir.join("agent/sessions").to_string_lossy(),
            "script": script.to_string_lossy(),
        },
    }));
    created["activeSessionId"]
        .as_str()
        .or_else(|| created["id"].as_str())
        .expect("active session id")
        .to_string()
}

#[test]
fn waits_end_on_a_message_answer_a_cycle_at_once_and_time_out() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path();
    let records = root.join("records");
    std::fs::create_dir_all(&records).expect("records dir");
    std::fs::create_dir_all(root.join("agent/sessions")).expect("sessions dir");
    let body = format!("the contract is ready: {}", "x".repeat(300));

    let alpha_wait = record(
        r#"(agent-message-wait #:sender "beta" #:timeout-ms 60000)"#,
        "alpha-wait",
        &records,
    );
    let beta_code = [
        record(
            r#"(agent-message-wait #:sender "alpha" #:timeout-ms 60000)"#,
            "beta-cycle",
            &records,
        ),
        record(
            &format!(
                r#"(agent-message-send {body:?} #:receiver-role "sibling" #:receiver-name "alpha")"#
            ),
            "beta-send",
            &records,
        ),
        record(
            r#"(agent-message-wait #:sender "alpha" #:timeout-ms 300)"#,
            "beta-timeout",
            &records,
        ),
    ]
    .join("\n");
    let filler = json!({ "text": "absorbed" });
    let [alpha_call, alpha_done] = cell_turn(&alpha_wait);
    let [beta_call, beta_done] = cell_turn(&beta_code);

    let socket = root.join("daemon.sock");
    let _daemon = spawn_supervisor(&socket, &root.join("agent"));
    let mut client = Client::connect(&socket);
    let alpha = create(
        &mut client,
        root,
        "alpha",
        &[alpha_call, alpha_done, filler.clone(), filler.clone()],
    );
    let beta = create(&mut client, root, "beta", &[beta_call, beta_done, filler]);

    // Alpha parks on beta; the supervisor's roster reports its `waitingOn` (the readiness beta's
    // walk needs: the walk reads that roster, which `list`'s live pull can run ahead of).
    client.request(
        &json!({ "type": "prompt", "activeSessionId": alpha, "message": "wait for beta" }),
    );
    let mut roster_probe = Client::connect(&socket);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot = roster_probe.request(&json!({ "type": "roster_subscribe" }));
        let waiting_on = snapshot["roster"]
            .as_array()
            .expect("roster")
            .iter()
            .find(|entry| entry["summary"]["activeSessionId"] == alpha.as_str())
            .and_then(|entry| entry["summary"].get("waitingOn").cloned());
        if waiting_on == Some(json!(beta)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "alpha never reported waitingOn beta: {snapshot}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(roster_probe);

    client
        .request(&json!({ "type": "prompt", "activeSessionId": beta, "message": "talk to alpha" }));
    let cycle = recorded(&records, "beta-cycle");
    assert!(
        cycle["elapsedMs"].as_u64().expect("elapsed") < 10_000,
        "{cycle}"
    );
    assert_eq!(
        cycle,
        json!({
            "outcome": "cycle",
            "messageId": null,
            "senderName": null,
            "senderRole": null,
            "preview": null,
            "blockedVia": ["alpha"],
            "elapsedMs": cycle["elapsedMs"],
        })
    );
    let sent = recorded(&records, "beta-send");
    let waited = recorded(&records, "alpha-wait");
    let preview: String = body.chars().take(239).chain(['…']).collect();
    assert_eq!(
        waited,
        json!({
            "outcome": "message",
            "messageId": sent["id"],
            "senderName": "beta",
            "senderRole": "sibling",
            "preview": preview,
            "blockedVia": null,
            "elapsedMs": waited["elapsedMs"],
        })
    );
    let timeout = recorded(&records, "beta-timeout");
    assert_eq!(timeout["outcome"], "timeout", "{timeout}");
    assert!(
        timeout["elapsedMs"].as_u64().expect("elapsed") >= 300,
        "{timeout}"
    );

    // The body reaches alpha's context once: as the ordinary agent-message turn.
    client.request(&json!({ "type": "wait_for_idle", "activeSessionId": alpha }));
    let messages = client
        .request(&json!({ "type": "get_messages", "activeSessionId": alpha }))
        .to_string();
    let rendered = serde_json::to_string(&format!("[agent-message from beta]\n\n{body}"))
        .expect("rendered prompt");
    let rendered = rendered.trim_matches('"');
    assert_eq!(messages.matches(rendered).count(), 1, "{messages}");
}
