//! Daemon-boot reap e2e. Sessions are hosted inside the daemon now, but the product binary
//! still has its `worker` role and a daemon from before the change ran one worker process per
//! session: a daemon that boots on a socket must still clear a worker PROCESS left on that
//! socket (it holds its session's lease, so the session would refuse to open forever), and
//! must never touch another socket's daemon, sessions or leftovers. Linux-only (/proc).
#![cfg(target_os = "linux")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
    #[expect(
        dead_code,
        reason = "the spawn records the socket path; the tests reach the daemon only through the client"
    )]
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let mut command = Command::new(binary);
    let command = command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Session sockets land in `<TMPDIR>/prime-agent-<uid>/`: keep them in
        // the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn pa-daemon supervisor");
    Daemon {
        child,
        socket: socket.to_path_buf(),
    }
}

fn wait_socket_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Liveness that ignores zombies (a re-parented child nobody reaps keeps its
/// /proc entry until the status is collected).
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

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        self.send(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
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

    /// The response for `id` (broadcast lines are skipped).
    fn read_response(&mut self, id: &str) -> Value {
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// A session worker PROCESS as a daemon from before sessions were hosted left it: the
/// product binary in its `worker` role, stamped with the daemon socket it belonged to,
/// serving its own socket while no daemon answers on that one.
struct LeftoverWorker {
    child: Child,
    socket: PathBuf,
}

impl Drop for LeftoverWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a leftover worker of `daemon_socket`'s lineage and wait until it serves. Its
/// supervisor-lost window is long: only a boot reap ends it inside the test.
fn spawn_leftover_worker(dir: &Path, daemon_socket: &Path, name: &str) -> LeftoverWorker {
    // The endpoint a daemon on `daemon_socket` (started with TMPDIR = `dir`) gives a worker.
    let socket_dir = dir.join(format!(
        "{}-{}",
        pa_types::platform::StateLayout::BUILD.endpoint_prefix(),
        pa_core::platform::process::current_user_id()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let socket = socket_dir.join(format!(
        "worker-{}-{name}.sock",
        pa_daemon::paths::hash_key(&daemon_socket.to_string_lossy(), 12)
    ));
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("worker")
        .env("TMPDIR", dir)
        .env(pa_daemon::worker::WORKER_ROLE_ENV, "1")
        .env(pa_daemon::worker::WORKER_SOCKET_ENV, &socket)
        .env(pa_daemon::worker::WORKER_TOKEN_ENV, format!("token-{name}"))
        .env(pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV, name)
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
            daemon_socket,
        )
        .env(
            pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
            dir.join(format!("{name}.recovery.jsonl")),
        )
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "600000",
        )
        .env("DO_NOT_TRACK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the leftover worker");
    let leftover = LeftoverWorker { child, socket };
    let deadline = Instant::now() + Duration::from_secs(30);
    while UnixStream::connect(&leftover.socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "the leftover worker never served its socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    leftover
}

/// Wait until the boot reap has ended `leftover`.
fn await_reaped(leftover: &LeftoverWorker) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while process_alive(leftover.child.id()) {
        assert!(
            Instant::now() < deadline,
            "the leftover worker survived the new daemon's boot reap"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The operator's report, end to end: a worker process is left on the socket holding its
/// session's lease, a daemon boots on the SAME socket, and the session opens.
#[test]
fn boot_reap_clears_a_leftover_worker_process_and_its_session_reopens() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    // The session the leftover holds: its file on disk, and its lease naming the leftover
    // process (pid and start id) as the owner.
    let mut session =
        pa_daemon::session_store::SessionFile::create(&dir.path().to_string_lossy(), None, 0);
    session.append_message(&json!({ "role": "user", "content": "lane work", "timestamp": 1u64 }));
    let session_file = sessions_dir.join(format!("{}.jsonl", session.session_id()));
    session.set_path(session_file.clone());
    session.rewrite().expect("session file");
    let leftover = spawn_leftover_worker(dir.path(), &socket, "leftover");
    std::mem::forget(
        pa_daemon::lease::acquire_runtime_session_lease(&session_file, &agent_dir)
            .expect("the lease to hand to the leftover"),
    );
    let owner_path = std::fs::read_dir(agent_dir.join("session-leases"))
        .expect("leases dir")
        .flatten()
        .map(|entry| entry.path().join("owner.json"))
        .find(|path| path.exists())
        .expect("the lease's owner record");
    let mut owner: Value =
        serde_json::from_str(&std::fs::read_to_string(&owner_path).expect("owner record"))
            .expect("owner json");
    owner["pid"] = json!(leftover.child.id());
    owner["processStartId"] = json!(pa_daemon::lease::get_process_start_id(leftover.child.id()));
    std::fs::write(&owner_path, owner.to_string()).expect("hand the lease over");
    assert!(
        pa_daemon::lease::acquire_runtime_session_lease(&session_file, &agent_dir).is_err(),
        "the live leftover holds the session (the refusal the operator saw)"
    );

    // THE ENV-PROPAGATION GUARD: a process that merely INHERITED the worker environment
    // must survive the new daemon's boot (an env-only reap would kill a session's whole
    // process tree).
    let mut inherited_env_child = Command::new("sleep")
        .arg("300")
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
            socket.to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_SOCKET_ENV,
            "/tmp/inherited-env-not-a-worker.sock",
        )
        .spawn()
        .expect("spawn the inherited-env child");

    let daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    await_reaped(&leftover);

    // The operator's open: the same session file, through the new daemon. Without the reap
    // this create refuses; with it, the fresh lease reclaims the dead holder's.
    let script = dir.path().join("script-reopen.json");
    std::fs::write(
        &script,
        json!({ "responses": [{ "text": "turn-reopen", "delayMs": 10 }] }).to_string(),
    )
    .expect("write the reopen script");
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    client.send_command(
        "reopen",
        &json!({
            "type": "create",
            "sessionPath": session_file.to_string_lossy(),
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let reopened = client.read_response("reopen");
    let inherited_env_child_alive = process_alive(inherited_env_child.id());
    let _ = inherited_env_child.kill();
    let _ = inherited_env_child.wait();
    let log = std::fs::read_to_string(pa_daemon::paths::daemon_log_path(&socket, &agent_dir))
        .unwrap_or_default();
    assert_eq!(
        (
            reopened["success"].clone(),
            reopened["data"]["sessionId"].clone(),
            inherited_env_child_alive,
            log.contains("boot reap: leftover worker"),
            leftover.socket.exists(),
        ),
        (json!(true), json!(session.session_id()), true, true, false,),
        "the session reopens, the inherited-env process survives, the reap names what it \
         cleared and the leftover's socket file left with it: {reopened}\nlog: {log}"
    );

    client.send_command("bye", &json!({ "type": "shutdown" }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(daemon.child.id()) {
        assert!(Instant::now() < deadline, "the new supervisor never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn boot_reap_never_touches_another_sockets_daemon_sessions_or_leftovers() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket_a = dir.path().join("daemon-a.sock");
    let socket_b = dir.path().join("daemon-b.sock");
    let socket_c = dir.path().join("daemon-c.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    // A healthy daemon on a DIFFERENT socket with one live session, and a leftover worker
    // of a third socket's lineage.
    let daemon_b = spawn_supervisor(&socket_b, &agent_dir);
    wait_socket_ready(&socket_b);
    let (mut client_b, hello_b) = Client::connect(&socket_b);
    assert_eq!(hello_b["type"], "daemon_hello");
    let script = dir.path().join("script-b.json");
    std::fs::write(
        &script,
        json!({ "responses": [{ "text": "turn-b", "delayMs": 10 }] }).to_string(),
    )
    .expect("write script");
    client_b.send_command(
        "create-b",
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client_b.read_response("create-b");
    assert_eq!(created["success"], true, "create on B failed: {created}");
    let session_b = created["data"]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let leftover_c = spawn_leftover_worker(dir.path(), &socket_c, "leftover-c");

    // Socket A's own leftover, then a daemon booting on socket A: it reaps A's only.
    let leftover_a = spawn_leftover_worker(dir.path(), &socket_a, "leftover-a");
    let daemon_a = spawn_supervisor(&socket_a, &agent_dir);
    wait_socket_ready(&socket_a);
    await_reaped(&leftover_a);

    client_b.send_command(
        "p-b",
        &json!({ "type": "prompt_and_wait", "activeSessionId": session_b, "message": "go" }),
    );
    let prompted = client_b.read_response("p-b");
    assert_eq!(
        (
            process_alive(daemon_b.child.id()),
            process_alive(leftover_c.child.id()),
            prompted["success"].clone(),
        ),
        (true, true, json!(true)),
        "the other socket's daemon, its session and the third socket's leftover were never \
         reap targets: {prompted}"
    );

    // Teardown: both daemons take the protocol stop and exit inside the window.
    client_b.send_command("bye-b", &json!({ "type": "shutdown" }));
    let (mut client_a, _) = Client::connect(&socket_a);
    client_a.send_command("bye-a", &json!({ "type": "shutdown" }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(daemon_b.child.id()) || process_alive(daemon_a.child.id()) {
        assert!(
            Instant::now() < deadline,
            "the shut-down supervisors never exited"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
