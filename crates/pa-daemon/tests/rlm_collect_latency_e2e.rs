//! How long `rlm.collect` takes against the real `pa-daemon` supervisor: children are
//! sessions of the daemon on the faux engine, the parent's registry is the one a worker
//! builds. A collect reads the registry's records, so it must end at its timeout however
//! many children are busy, and must not wait for a child that is waiting on its parent.
//! The idle wait the settle watchers park on must end where it is served.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_core::session_engine::rlm_host::{RlmSpawnRequest, RlmSubagentHost};
use pa_daemon::rlm_children::{ParentIdentity, SupervisorChildSessions};
use pa_daemon::supervisor_link::SupervisorLink;
use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

/// A JSONL supervisor client; a line that does not come within 30 seconds fails the test.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let mut hello = String::new();
        client.reader.read_line(&mut hello).expect("hello");
        client
    }

    fn send(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    /// The next response line (events are skipped).
    fn response(&mut self) -> Value {
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .expect("a supervisor line within the read timeout");
            assert!(read > 0, "supervisor closed the connection");
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if value["type"] == "response" {
                return value;
            }
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send(id, command);
        loop {
            let response = self.response();
            if response["id"] == id {
                return response;
            }
        }
    }
}

/// A real parent session and the children registry a worker would build for it.
struct Family {
    dir: tempfile::TempDir,
    client: Client,
    children: SupervisorChildSessions,
    identity: ParentIdentity,
    // Declared last: the daemon is reaped after everything that talks to it.
    _daemon: Daemon,
}

impl Family {
    fn start() -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let agent_dir = dir.path().join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
            .arg("supervisor")
            .arg("--socket")
            .arg(&socket)
            // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`: keep them in the test's dir.
            .env("TMPDIR", dir.path())
            .arg("--agent-dir")
            .arg(&agent_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env(
                pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
                "15000",
            )
            .spawn()
            .expect("spawn pa-daemon supervisor");
        common::await_listening(&socket, &mut child);
        let daemon = Daemon { child };
        let mut client = Client::connect(&socket);
        let parent_script = dir.path().join("parent.json");
        std::fs::write(
            &parent_script,
            json!({ "engine": "faux", "responses": [{ "text": "parent turn done" }] }).to_string(),
        )
        .expect("parent script");
        let created = client.request(
            "create-parent",
            &json!({
                "type": "create",
                "name": "parent",
                "config": {
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                    "script": parent_script.to_string_lossy(),
                },
            }),
        );
        assert_eq!(created["success"], true, "create parent failed: {created}");
        let parent = &created["data"];
        let parent_active = parent["activeSessionId"]
            .as_str()
            .or_else(|| parent["id"].as_str())
            .expect("parent active id")
            .to_string();
        // The parent's worker token: the roster a collect reads is worker-token gated.
        let worker_token = std::fs::read_dir(agent_dir.join("daemon-workers"))
            .expect("daemon-workers")
            .flatten()
            .find_map(|instance| {
                let descriptor =
                    std::fs::read_to_string(instance.path().join(format!("{parent_active}.json")))
                        .ok()?;
                serde_json::from_str::<Value>(&descriptor).ok()?["authenticationToken"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("the parent's worker token");
        let children = SupervisorChildSessions::new(
            Arc::new(SupervisorLink::new(socket.clone())),
            agent_dir.clone(),
            parent_active,
            Arc::new(pa_daemon::model_allowlist::ModelRefusalTelemetry::new(
                agent_dir, /*telemetry_disabled*/ true,
            )),
            worker_token,
        );
        let identity = ParentIdentity {
            rlm_depth: 0,
            rlm_max_depth: 2,
            model: Some("faux/faux-1".to_string()),
            cwd: Some(dir.path().to_string_lossy().to_string()),
            session_id: parent["sessionId"].as_str().map(str::to_string),
            session_file: parent["sessionFile"].as_str().map(str::to_string),
            thinking: None,
            child_script: None,
        };
        Family {
            dir,
            client,
            children,
            identity,
            _daemon: daemon,
        }
    }

    /// Spawn one child whose faux model plays `responses`, and release its task prompt (this
    /// registry sits outside a parent turn, so the test signals the turn boundary).
    async fn spawn(&self, name: &str, responses: &Value) {
        let script = self.dir.path().join(format!("{name}.json"));
        std::fs::write(
            &script,
            json!({ "engine": "faux", "responses": responses }).to_string(),
        )
        .expect("child script");
        self.children.set_identity(ParentIdentity {
            child_script: Some(script.to_string_lossy().to_string()),
            ..self.identity.clone()
        });
        self.children
            .spawn(RlmSpawnRequest {
                prompt: "run the lane task".to_string(),
                name: Some(name.to_string()),
                model: None,
                thinking: None,
                cwd: None,
                spawned_by_request_id: None,
                factory: None,
            })
            .await
            .expect("spawn");
        self.children.notify_turn_done();
    }

    /// The supervisor roster's subagent rows, once `count` of them satisfy `ready`.
    fn await_children(&mut self, count: usize, ready: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_mins(2);
        loop {
            let list = self.client.request("list", &json!({ "type": "list" }));
            let rows: Vec<Value> = list["data"]["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["runtimeKind"] == "subagent" && ready(row))
                .cloned()
                .collect();
            if rows.len() == count {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "only {} of {count} children became ready: {list}",
                rows.len()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Twenty-four children mid-turn: a snapshot comes back at once and a timed collect ends at
/// its timeout. Before the collect read the registry it asked each child in turn and then
/// waited five more seconds for a reply that could not come: 7.2 s for a 2 s timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collect_of_many_running_children_ends_at_its_timeout() {
    const CHILDREN: usize = 24;
    let mut family = Family::start();
    for index in 0..CHILDREN {
        family
            .spawn(
                &format!("lane-{index}"),
                &json!([{ "text": "slow answer", "delayMs": 300_000 }]),
            )
            .await;
    }
    family.await_children(CHILDREN, |row| row["isStreaming"] == true);

    let started = Instant::now();
    let snapshot = family
        .children
        .collect(Vec::new(), 0)
        .await
        .expect("snapshot");
    let snapshot_took = started.elapsed();
    let started = Instant::now();
    let waited = family
        .children
        .collect(Vec::new(), 2_000)
        .await
        .expect("a timeout is never an error");
    let wait_took = started.elapsed();

    let running = |rows: &[pa_core::session_engine::rlm_host::RlmChildResult]| {
        rows.iter()
            .filter(|row| row.status == "running" && !row.settled)
            .count()
    };
    assert_eq!(
        (running(&snapshot), running(&waited)),
        (CHILDREN, CHILDREN),
        "every child is still running in both collects"
    );
    assert!(
        snapshot_took < Duration::from_secs(1),
        "a snapshot of {CHILDREN} busy children took {snapshot_took:?}"
    );
    assert!(
        (Duration::from_secs(2)..Duration::from_millis(3_500)).contains(&wait_took),
        "a 2 s collect of {CHILDREN} busy children took {wait_took:?}"
    );
}

/// A child parked in `event-wait` for its parent's message cannot settle until the
/// parent's cell returns, so a collect does not wait for it: the row says the child is
/// waiting on the caller. Before, both sides ran out their timeouts (25 s for a 20 s collect).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collect_does_not_wait_for_a_child_that_waits_on_its_parent() {
    let mut family = Family::start();
    let cell = r#"(event-wait #:timeout-ms 120000 #:sender-role "parent")"#;
    family
        .spawn(
            "asker",
            &json!([
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": { "code": cell } },
                ] },
                { "text": "done" },
            ]),
        )
        .await;
    // Readiness: the child's wait names the parent on the supervisor's roster.
    family.await_children(1, |row| row["waitingOn"].is_string());

    let started = Instant::now();
    let rows = family
        .children
        .collect(Vec::new(), 20_000)
        .await
        .expect("collect");
    let took = started.elapsed();
    assert_eq!(
        rows.iter()
            .map(|row| (row.status, row.settled, row.waiting_on_you))
            .collect::<Vec<_>>(),
        [("running", false, true)]
    );
    assert!(
        took < Duration::from_secs(5),
        "the collect waited {took:?} for a child that was waiting on it"
    );
}

/// An idle wait with a `timeoutMs` is answered by the session at that bound, so the settle
/// watchers' wait slices leave nothing behind on a busy child's route. Before, each slice
/// stayed in flight until the child went idle, and after 128 of them (a child busy for about
/// two hours) the route refused every command as overloaded: the child read as unreachable.
#[test]
fn a_bounded_idle_wait_ends_at_its_bound_and_leaves_the_childs_route_free() {
    const ROUNDS: usize = 3;
    const PER_ROUND: usize = 50;
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut family = Family::start();
    runtime.block_on(family.spawn(
        "long",
        &json!([{ "text": "slow answer", "delayMs": 300_000 }]),
    ));
    let child = family.await_children(1, |row| row["isStreaming"] == true)[0]["id"]
        .as_str()
        .expect("the child's active session id")
        .to_string();

    // More waits than the route holds in flight (128), in rounds a connection can
    // dispatch at once (64): each round is answered before the next is sent.
    let mut answers = Vec::new();
    for round in 0..ROUNDS {
        for index in 0..PER_ROUND {
            family.client.send(
                &format!("wait-{round}-{index}"),
                &json!({
                    "type": "wait_for_idle",
                    "activeSessionId": child,
                    "waitForRlmQuiescence": true,
                    "timeoutMs": 200,
                }),
            );
        }
        for _ in 0..PER_ROUND {
            answers.push(family.client.response()["data"].clone());
        }
    }
    assert_eq!(
        answers,
        vec![json!({ "timedOut": true }); ROUNDS * PER_ROUND]
    );

    let state = family.client.request(
        "state",
        &json!({ "type": "get_state", "activeSessionId": child }),
    );
    assert_eq!(
        (
            state["success"].clone(),
            state["data"]["isStreaming"].clone()
        ),
        (json!(true), json!(true)),
        "the busy child still answers after its timed-out waits: {state}"
    );
}
