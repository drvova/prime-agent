//! The stale-lease-release e2e: whatever ends a session without its own
//! release - the daemon that hosts it dying, or the daemon giving up on it
//! after the failure cap - must leave its session file openable. A session
//! revived at boot must be supervised through the lifetime the boot started,
//! never through the dead pre-restart identity its descriptor carried.
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

#[expect(
    clippy::zombie_processes,
    reason = "the timeout panic path cannot wait on the child; the test process exits right after and reaps it"
)]
fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
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
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return Daemon {
                child,
                socket: socket.to_path_buf(),
            };
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

/// SIGKILL by pid (the target is a worker process the test does not own
/// as a `std` child).
fn daemon_log(socket: &Path, agent_dir: &Path) -> PathBuf {
    pa_daemon::paths::daemon_log_path(socket, agent_dir)
}

fn log_contains(socket: &Path, agent_dir: &Path, needle: &str) -> bool {
    std::fs::read_to_string(daemon_log(socket, agent_dir)).is_ok_and(|log| log.contains(needle))
}

#[track_caller]
fn await_log_line(socket: &Path, agent_dir: &Path, needle: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !log_contains(socket, agent_dir, needle) {
        assert!(
            Instant::now() < deadline,
            "the daemon log never said the awaited line"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
        let deadline = Instant::now() + Duration::from_secs(30);
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

    fn read_response(&mut self, id: &str) -> Value {
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// The prompt the fixture's `panicOn` seam panics on.
const CRASHING_PROMPT: &str = "crash now";

/// A scripted session answering `text`, whose turn panics on [`CRASHING_PROMPT`].
fn write_script(dir: &Path, name: &str, text: &str) -> PathBuf {
    let script_path = dir.join(name);
    std::fs::write(
        &script_path,
        json!({
            "responses": [ { "text": text, "delayMs": 10 } ],
            "panicOn": { "message": CRASHING_PROMPT, "panic": "injected fault" },
        })
        .to_string(),
    )
    .expect("write");
    script_path
}

/// Create the session (or, with `session_file`, open that saved one) and
/// attach to its events: the create's reply.
fn open_session(
    client: &mut Client,
    dir: &Path,
    sessions_dir: &Path,
    script: &Path,
    session_file: Option<&Path>,
) -> Value {
    let mut create = json!({
        "type": "create",
        "config": {
            "cwd": dir.to_string_lossy(),
            "sessionDir": sessions_dir.to_string_lossy(),
            "script": script.to_string_lossy(),
        },
    });
    if let Some(session_file) = session_file {
        create["sessionPath"] = json!(session_file.to_string_lossy());
    }
    client.send_command("open", &create);
    let opened = client.read_response("open");
    assert_eq!(opened["success"], true, "the session must open: {opened}");
    client.send_command(
        "attach",
        &json!({ "type": "attach", "activeSessionId": opened["data"]["id"] }),
    );
    let attached = client.read_response("attach");
    assert_eq!(attached["success"], true, "attach failed: {attached}");
    opened["data"].clone()
}

/// Make the attached session's turn panic and return what the daemon did
/// about it: the `session_recovered` or `session_failed` event.
fn crash_session(client: &mut Client, session_id: &str) -> Value {
    client.send_command(
        "crash",
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": CRASHING_PROMPT }),
    );
    loop {
        let line = client.read_line();
        let event = &line["event"];
        if line["type"] == "session_event"
            && line["activeSessionId"] == session_id
            && (event["type"] == "session_recovered" || event["type"] == "session_failed")
        {
            return event.clone();
        }
    }
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

/// A session that was mid-turn when its daemon died is revived by the next
/// boot. The revived lifetime is the one the daemon watches: a crash in it is
/// its first failure and heals, and the session's file opens onto it.
#[test]
fn the_revived_session_stays_supervised_and_its_file_reopens() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    // The turn outlasts the daemon: the kill lands mid-turn.
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        json!({
            "responses": [ { "text": "turn-1", "delayMs": 1500 } ],
            "panicOn": { "message": CRASHING_PROMPT, "panic": "injected fault" },
        })
        .to_string(),
    )
    .expect("write");

    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let opened = open_session(&mut client, dir.path(), &sessions_dir, &script, None);
    let session_id = opened["id"].as_str().expect("session id").to_string();
    let session_file = PathBuf::from(opened["sessionFile"].as_str().expect("session file"));
    client.send_command(
        "prompt",
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": "go" }),
    );
    // The turn's start and the prompt's reply arrive in no fixed order.
    loop {
        let line = client.read_line();
        if line["type"] == "session_event" && line["event"]["type"] == "agent_start" {
            break;
        }
    }
    daemon.child.kill().expect("kill -9 supervisor");
    let _ = daemon.child.wait();

    // The new daemon on the SAME socket: its adoption revives the session.
    let _daemon2 = spawn_supervisor(&socket, &agent_dir);
    await_log_line(
        &socket,
        &agent_dir,
        &format!("adopted session worker {session_id} (was alive: false)"),
        Duration::from_secs(20),
    );
    let mut client2 = Client::connect(&socket);
    let reopened = open_session(
        &mut client2,
        dir.path(),
        &sessions_dir,
        &script,
        Some(&session_file),
    );
    client2.send_command(
        "idle",
        &json!({ "type": "wait_for_idle", "activeSessionId": session_id }),
    );
    assert_eq!(client2.read_response("idle")["success"], true);
    let healed = crash_session(&mut client2, &session_id);
    assert_eq!(
        (
            reopened["id"].clone(),
            healed["type"].clone(),
            healed["failures"].clone(),
            log_contains(&socket, &agent_dir, "consecutive failures"),
        ),
        (
            json!(session_id),
            json!("session_recovered"),
            json!(1),
            false
        ),
        "the file opens onto the revived session, whose first crash is failure 1 of a \
         lifetime the daemon watches: {healed}"
    );
}

/// A session idle in a daemon that is killed outright is not revived, and its lease
/// names a session of a dead daemon: the next boot releases it and the file
/// opens.
#[test]
fn a_lease_held_by_a_session_of_a_dead_daemon_is_released_at_boot() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let script = write_script(dir.path(), "script.json", "turn-1");

    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let opened = open_session(&mut client, dir.path(), &sessions_dir, &script, None);
    let session_id = opened["id"].as_str().expect("session id").to_string();
    let session_file = PathBuf::from(opened["sessionFile"].as_str().expect("session file"));
    client.send_command(
        "turn",
        &json!({ "type": "prompt_and_wait", "activeSessionId": session_id, "message": "go" }),
    );
    assert_eq!(client.read_response("turn")["success"], true);
    daemon.child.kill().expect("kill -9 supervisor");
    let _ = daemon.child.wait();
    let left_behind = held_leases(&agent_dir).len();

    let _daemon2 = spawn_supervisor(&socket, &agent_dir);
    await_log_line(
        &socket,
        &agent_dir,
        &format!("session worker {session_id} was idle at exit; not revived"),
        Duration::from_secs(20),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held_leases(&agent_dir).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the boot never released the dead daemon's lease: {:?}",
            held_leases(&agent_dir)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut client2 = Client::connect(&socket);
    let reopened = open_session(
        &mut client2,
        dir.path(),
        &sessions_dir,
        &script,
        Some(&session_file),
    );
    assert_eq!(
        (left_behind, reopened["sessionId"].clone()),
        (1, opened["sessionId"].clone()),
        "the killed daemon left one lease and the saved session opens after the boot"
    );
}

#[test]
fn a_session_that_fails_to_death_releases_the_session_hold() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let script = write_script(dir.path(), "script.json", "turn-1");

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let opened = open_session(&mut client, dir.path(), &sessions_dir, &script, None);
    let session_id = opened["id"].as_str().expect("session id").to_string();
    let session_file = PathBuf::from(opened["sessionFile"].as_str().expect("session file"));

    // Crash the session each time it comes back until the cap gives up.
    while crash_session(&mut client, &session_id)["type"] != "session_failed" {}

    // No resident serves the file (the id was abandoned) and the give-up
    // released its lease: the file opens as a new resident.
    let mut client2 = Client::connect(&socket);
    let reopened = open_session(
        &mut client2,
        dir.path(),
        &sessions_dir,
        &script,
        Some(&session_file),
    );
    assert_eq!(
        reopened["sessionId"], opened["sessionId"],
        "a given-up session must leave its file re-openable: {reopened}"
    );
}

/// A worker PROCESS stamped with a session's id (a leftover of a daemon from
/// before sessions were hosted) must not outlive the daemon's give-up on
/// that id: it would hold the session invisibly.
#[test]
fn the_give_up_sweep_reaps_a_live_leftover_of_the_abandoned_id() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let script = write_script(dir.path(), "script.json", "turn-1");

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let opened = open_session(&mut client, dir.path(), &sessions_dir, &script, None);
    let session_id = opened["id"].as_str().expect("session id").to_string();

    // A real product worker process whose active-session env names the
    // session's id; it fails registration (an unknown token) and parks in
    // the registration backoff - a process the daemon does not track.
    let mut fake = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("worker")
        .env(pa_daemon::worker::WORKER_ROLE_ENV, "1")
        .env(pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV, &session_id)
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
            socket.to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_SOCKET_ENV,
            dir.path()
                .join("fake-worker.sock")
                .to_string_lossy()
                .to_string(),
        )
        .env(pa_daemon::worker::WORKER_TOKEN_ENV, "not-the-real-token")
        .env(pa_daemon::worker::WORKER_INSTANCE_ID_ENV, "fake-instance")
        .env(
            pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
            dir.path()
                .join("fake-recovery.jsonl")
                .to_string_lossy()
                .to_string(),
        )
        .env(
            pa_daemon::worker::WORKER_CWD_ENV,
            dir.path().to_string_lossy().to_string(),
        )
        .env(
            pa_daemon::paths::AGENT_DIR_ENV,
            agent_dir.to_string_lossy().to_string(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the leftover worker role");
    let fake_pid = fake.id();
    // The leftover is up once it serves its own socket.
    let deadline = Instant::now() + Duration::from_secs(30);
    while UnixStream::connect(dir.path().join("fake-worker.sock")).is_err() {
        assert!(
            Instant::now() < deadline,
            "the leftover worker role never came up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    while crash_session(&mut client, &session_id)["type"] != "session_failed" {}
    await_log_line(
        &socket,
        &agent_dir,
        &format!("give-up sweep: leftover worker pid {fake_pid}"),
        Duration::from_secs(10),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(fake_pid) {
        assert!(
            Instant::now() < deadline,
            "the leftover of the abandoned id survived the give-up sweep"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = fake.wait();
}
