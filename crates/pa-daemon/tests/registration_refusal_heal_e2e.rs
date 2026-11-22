//! The stranded-session heal e2e: whatever is left holding a session after the daemon that
//! knew it is gone must not hold it forever. A worker PROCESS no descriptor backs (the
//! live-box incident's leftover: alive, invisible, holding its runtime session lease) has its
//! registration refused and retires. A session hosted by a daemon that died, whose descriptor
//! is gone, leaves only its lease, and the session file resumes. A session wedged in a call
//! that never returns is ended by its owner's stop, and its file resumes.
// The suite's liveness and child-discovery helpers read Linux procfs;
// on other unixes they cannot observe processes, and the waits would
// pass vacuously — skip the suite there instead of reporting a false
// green.
// Fn length is a style gate, not correctness: each e2e reads top to bottom as one scenario.
#![allow(clippy::too_many_lines)]
#![cfg(all(unix, target_os = "linux"))]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
    // The socket path rides the struct for harness symmetry; no case reads it here.
    #[allow(dead_code)]
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

// The timeout panic path cannot wait on the child; the test process exits
// immediately afterwards, reaping it.
#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`, which a killed
        // worker never cleans: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        );
    let child = command.spawn().expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return Daemon {
                child,
                socket: socket.to_path_buf(),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never came up");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect supervisor: {error}"),
            }
        };
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        let mut line = serde_json::to_string(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }))
        .expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(70);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if !line.trim().is_empty() => {
                    return serde_json::from_str(line.trim()).expect("parse response line");
                }
                Ok(_) => line.clear(),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(70);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(|value| value.as_str()) == Some(id) {
                return line;
            }
        }
    }
}

fn write_script(dir: &Path, responses: &[&str]) -> PathBuf {
    let script_path = dir.join(format!("script-{}.json", std::process::id()));
    let scripted: Vec<Value> = responses
        .iter()
        .map(|text| json!({ "text": text }))
        .collect();
    std::fs::write(&script_path, json!({ "responses": scripted }).to_string())
        .expect("write script");
    script_path
}

/// Liveness that ignores zombies (a re-parented child nobody reaps keeps
/// its /proc entry until the status is collected).
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next().unwrap_or_default();
    !state.starts_with('Z') && !state.starts_with('X')
}

/// Wait until the pid is gone or the deadline passes.
fn wait_gone(pid: u32, deadline: Instant) -> bool {
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// End a possibly-stopped worker by force (SIGKILL reaches a stopped
/// process; SIGTERM does not deliver while it is stopped).
fn force_kill(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

/// The session leases held under `agent_dir` (one `.lock` directory each).
fn held_leases(agent_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(agent_dir.join("session-leases"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "lock")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The active id a create response answered (the summary's `id`, what a pane attaches by).
fn create_session(client: &mut Client, request_id: &str, config: &Value) -> (String, Value) {
    client.send_command(request_id, &json!({ "type": "create", "config": config }));
    let created = client.read_response(request_id);
    assert_eq!(created["success"], true, "create failed: {created}");
    let id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();
    (id, created)
}

/// The session's durable file (TS `get_session_stats` -> sessionFile).
fn session_file_of(client: &mut Client, id: &str, request_id: &str) -> String {
    client.send_command(
        request_id,
        &json!({ "type": "get_session_stats", "activeSessionId": id }),
    );
    let stats = client.read_response(request_id);
    assert_eq!(stats["success"], true, "stats failed: {stats}");
    stats["data"]["sessionFile"]
        .as_str()
        .expect("session file in stats")
        .to_string()
}

/// Run one scripted turn to completion so the worker sits idle holding its session lease
/// (the completed turn makes the file a real transcript).
fn run_one_turn(client: &mut Client, id: &str, request_id: &str) {
    client.send_command(
        request_id,
        &json!({
            "type": "prompt_and_wait",
            "activeSessionId": id,
            "message": "go",
        }),
    );
    let done = client.read_response(request_id);
    assert_eq!(done["success"], true, "scripted turn failed: {done}");
}

#[test]
fn a_refused_registration_retires_the_worker() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    // Binding the socket precedes the boot predecessor census. Wait for the
    // accept loop's hello before spawning our intentionally descriptorless
    // worker, so startup cannot mistake it for a predecessor to reap.
    drop(Client::connect(&socket));

    // A bare worker with an identity no descriptor backs: the supervisor can only refuse it.
    // The scripted engine keeps the runtime self-contained, so the retire path under test
    // is the registration close alone.
    let script_path = write_script(dir.path(), &["unused"]);
    let mut worker = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("worker")
        .env(pa_daemon::worker::WORKER_ROLE_ENV, "1")
        .env(pa_daemon::worker::WORKER_TOKEN_ENV, "token-no-descriptor")
        .env(pa_daemon::worker::WORKER_INSTANCE_ID_ENV, "instance-orphan")
        .env(
            pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
            "orphan000001",
        )
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
            socket.to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_SOCKET_ENV,
            dir.path().join("orphan.sock").to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
            dir.path()
                .join("orphan.recovery.jsonl")
                .to_string_lossy()
                .to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_SCRIPT_ENV,
            script_path.to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::paths::AGENT_DIR_ENV,
            agent_dir.to_string_lossy().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");
    let worker_pid = worker.id();

    // The refusal lands in the daemon's rotating log (the operator-facing trace the incident
    // lacked) — and the worker exits instead of retrying forever.
    let log_path = pa_daemon::paths::daemon_log_path(&socket, &agent_dir);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("orphan000001 registration refused") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the refused registration never reached the log: {log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    if !wait_gone(worker_pid, Instant::now() + Duration::from_secs(15)) {
        force_kill(worker_pid);
        let _ = worker.wait();
        panic!("the refused worker never retired (it must run its graceful close and exit)");
    }
    let _ = worker.wait();

    // The refusal leaves the worker's own socket behind: a graceful exit owns its endpoint.
    assert!(
        !dir.path().join("orphan.sock").exists(),
        "the retired worker cleaned up its own socket"
    );
    let _ = std::fs::remove_file(dir.path().join("orphan.sock"));
}

/// The box incident under hosting: the daemon dies hard with a session in it and the
/// session's descriptor is destroyed. Nothing on disk names the session any more except the
/// lease its dead host left; the next daemon must not treat that as a live holder.
#[test]
fn a_descriptorless_session_of_a_dead_daemon_resumes() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let script_path = write_script(dir.path(), &["first scripted", "resumed turn"]);
    let create_config = json!({
        "cwd": dir.path().to_string_lossy(),
        "sessionDir": sessions_dir.to_string_lossy(),
        "script": script_path.to_string_lossy(),
    });
    let (worker_id, created) = create_session(&mut client, "c1", &create_config);
    run_one_turn(&mut client, &worker_id, "p1");
    let session_file = session_file_of(&mut client, &worker_id, "s1");

    // The daemon dies hard, the session with it, its lease still on disk. Its descriptor is
    // then destroyed — the stranded state the incident left.
    drop(client);
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    drop(daemon);
    let stranded_leases = held_leases(&agent_dir).len();
    std::fs::remove_file(
        pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
            .join(format!("{worker_id}.json")),
    )
    .expect("destroy the descriptor");

    // The next daemon cycle: the session resumes from its file (no hold refusal).
    let _daemon2 = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    client.send_command(
        "r1",
        &json!({
            "type": "create",
            "sessionPath": session_file,
            "config": create_config,
        }),
    );
    let resumed = client.read_response("r1");
    assert_eq!(
        (
            stranded_leases,
            resumed["success"].clone(),
            resumed["data"]["sessionId"].clone(),
        ),
        (1, json!(true), created["data"]["sessionId"].clone()),
        "the dead daemon left the session's lease and the resumed create succeeds: {resumed}"
    );
    let resumed_id = resumed["data"]["id"]
        .as_str()
        .expect("resumed session id")
        .to_string();
    run_one_turn(&mut client, &resumed_id, "p2");
}

/// A session whose cell sits in a native call no interrupt reaches cannot be unwound. Its
/// owner's stop must still be durable before the session is told, must end the session, and
/// must leave the session file openable.
#[test]
fn an_owned_stop_ends_a_wedged_session_and_its_file_resumes() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The second turn's cell enters a native call no interrupt reaches, which writes
    // its marker once it is inside and never returns.
    let on_engine = dir.path().join("cell-on-engine");
    let cell = format!(
        "(%test-block {})",
        serde_json::to_string(&on_engine.to_string_lossy()).expect("a Scheme string literal")
    );
    let wedging_script = dir.path().join("wedging.json");
    std::fs::write(
        &wedging_script,
        json!({
            "engine": "faux",
            "responses": [
                { "text": "first scripted" },
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": { "code": cell } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write script");
    // A client-owned session: its owner's stop must get past the wedged cell.
    client.send_command(
        "c1",
        &json!({
            "type": "create",
            "lifecycle": "client_owned",
            // The wedge is the kernel's test seam: no kernel function of its own
            // ignores a stop.
            "launchEnv": { "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1" },
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": wedging_script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let worker_id = created["data"]["id"]
        .as_str()
        .expect("session id in create response")
        .to_string();
    run_one_turn(&mut client, &worker_id, "p1");
    let session_file = session_file_of(&mut client, &worker_id, "s1");
    client.send_command(
        "p2",
        &json!({ "type": "prompt", "activeSessionId": worker_id, "message": "wedge" }),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !on_engine.exists() {
        assert!(
            Instant::now() < deadline,
            "the cell never reached the engine"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    client.send_command(
        "k1",
        &json!({ "type": "complete_owned_session", "activeSessionId": worker_id }),
    );
    // The stop's durable intent is observable on disk BEFORE the session is told: the
    // descriptor carries its stop tombstone while the stop is still ending the session — a
    // daemon that died mid-stop would finish the stop at its next boot, never re-adopt a
    // stopped session as healthy.
    let descriptor_path = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
        .join(format!("{worker_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let descriptor: Value =
            serde_json::from_str(&std::fs::read_to_string(&descriptor_path).unwrap_or_default())
                .unwrap_or(Value::Null);
        let marker = descriptor
            .get("stopRequestedAt")
            .and_then(Value::as_str)
            .is_some_and(|requested| !requested.is_empty())
            && descriptor.get("archiveOnStop") == Some(&Value::Bool(false));
        if marker {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the owned stop never persisted its tombstone (the stop must be durable before the shutdown routes): {descriptor}"
        );
        std::thread::yield_now();
    }
    let stopped = client.read_response("k1");
    // The stop answered: the session is retired and holds nothing.
    assert_eq!(
        (
            stopped["success"].clone(),
            descriptor_path.exists(),
            held_leases(&agent_dir),
        ),
        (json!(true), false, Vec::<PathBuf>::new()),
        "the owned stop must end the wedged session before it answers: {stopped}"
    );

    // The session file opens again instead of answering the hold refusal.
    let resume_script = write_script(dir.path(), &["first scripted", "resumed turn"]);
    client.send_command(
        "r1",
        &json!({
            "type": "create",
            "sessionPath": session_file,
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": resume_script.to_string_lossy(),
            },
        }),
    );
    let resumed = client.read_response("r1");
    assert_eq!(
        (
            resumed["success"].clone(),
            resumed["data"]["sessionId"].clone()
        ),
        (json!(true), created["data"]["sessionId"].clone()),
        "the stopped session must resume: {resumed}"
    );
    let resumed_id = resumed["data"]["id"]
        .as_str()
        .expect("resumed session id")
        .to_string();
    run_one_turn(&mut client, &resumed_id, "p3");
}
