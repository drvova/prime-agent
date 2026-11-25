//! Worker session-end kernel disposal e2e (TS `closeSession` parity): every session-end path
//! a live daemon runs (`kill`, daemon `shutdown`) must call `AgentSessionEngine::dispose_kernel`
//! explicitly, and a daemon that was killed outright disposes nothing, so the next boot reaps what
//! its sessions journalled. The kernel is an engine inside the daemon, but the shell commands
//! its cells start are processes of their own: a command that survives its session is
//! orphaned forever. Each test starts one in the background from a scripted `eval` cell and
//! watches that process.
// Process liveness is read from /proc: Linux only.
#![cfg(target_os = "linux")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Whether process `pid` still runs (a zombie waiting for its reaper has ended).
fn is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // The state follows the parenthesized command name.
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// Poll until the session's background command is gone: the session-end dispose under
/// test. A survivor past the budget is the leak this test exists to catch.
fn await_command_gone(pid: u32, budget: Duration) {
    let deadline = Instant::now() + budget;
    while is_running(pid) {
        assert!(
            Instant::now() < deadline,
            "the session's background command (pid {pid}) outlived the session-end path (missing dispose)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

fn spawn_supervisor(socket: &Path, agent_dir: &Path, lost_window_ms: &str) -> Daemon {
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
        // Hermetic agent dir: point every fallback at the test sandbox.
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env_remove("PRIME_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // The supervisor-lost exit window its workers inherit (TS `workerSupervisorLostExitMs`;
        // the env flows to the workers the supervisor spawns).
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            lost_window_ms,
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon { child }
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

/// JSONL supervisor client (command envelopes, id-matched responses).
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        self.write_line(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn write_line(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("serialize line");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_mins(2);
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
        let deadline = Instant::now() + Duration::from_mins(4);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// A faux script whose single turn runs one cell that starts a long command in the
/// background. The command starts the program `/bin/sleep` and writes its pid as a receipt:
/// proof that the session built, its kernel ran the cell, and the process to watch. A
/// program, by its path: under the built-in shell `$$` is a job number and a bare `sleep`
/// the shell's own, so neither names a process.
fn write_faux_script(dir: &Path) -> PathBuf {
    let receipts_dir = dir.join("receipts");
    std::fs::create_dir_all(&receipts_dir).expect("receipts dir");
    let receipt = receipts_dir.join("command.pid");
    let command = format!(
        "/bin/sleep 3600 & echo $! > '{}'; wait",
        receipt.to_string_lossy()
    );
    let cell = format!(
        "(bash {} (hash 'background #t))",
        serde_json::to_string(&command).expect("a Scheme string literal")
    );
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": cell,
                    } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write faux script");
    script
}

/// Create a session on the supervisor and run its one scripted turn (prompt + idle wait).
fn create_session_with_kernel(client: &mut Client, dir: &Path, script: &Path, id: &str) -> String {
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    client.send_command(
        id,
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response(id);
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["activeSessionId"]
        .as_str()
        .or_else(|| created["data"]["id"].as_str())
        .expect("active session id")
        .to_string();
    let prompt_id = format!("{id}-prompt");
    client.send_command(
        &prompt_id,
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": "run the cell" }),
    );
    let prompted = client.read_response(&prompt_id);
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    let idle_id = format!("{id}-idle");
    client.send_command(
        &idle_id,
        &json!({ "type": "wait_for_idle", "activeSessionId": session_id }),
    );
    let idle = client.read_response(&idle_id);
    assert_eq!(idle["success"], true, "wait_for_idle failed: {idle}");
    session_id
}

/// The pid of the background command the session's cell started, once it is running.
fn await_command(dir: &Path) -> u32 {
    let receipt = dir.join("receipts").join("command.pid");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pid = std::fs::read_to_string(&receipt)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok());
        if let Some(pid) = pid.filter(|pid| is_running(*pid)) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the cell's background command never wrote its pid to {}",
            receipt.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn kill_disposes_the_session_kernel() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path());

    let _daemon = spawn_supervisor(&socket, &agent_dir, "15000");
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let session_id = create_session_with_kernel(&mut client, dir.path(), &script, "c1");

    // The session's kernel ran the cell and its command is alive.
    let command = await_command(dir.path());

    // User delete: the routed `kill` closes the session (the worker stops right after).
    client.send_command(
        "k1",
        &json!({ "type": "kill", "activeSessionId": session_id }),
    );
    let killed = client.read_response("k1");
    assert_eq!(killed["success"], true, "kill failed: {killed}");

    // The command died with the session: process exit never runs the
    // engine-drop teardown, so only the explicit dispose clears this.
    await_command_gone(command, Duration::from_secs(90));
}

/// Daemon `shutdown` routes a `shutdown` to every worker: each disposes its kernel
/// before its own process exit (no destructors run on `std::process::exit`).
#[test]
fn shutdown_disposes_session_kernels_before_the_worker_exits() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path());

    let _daemon = spawn_supervisor(&socket, &agent_dir, "15000");
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let _session_id = create_session_with_kernel(&mut client, dir.path(), &script, "c1");
    let command = await_command(dir.path());

    // Daemon stop: the supervisor routes `shutdown` to the workers and exits.
    client.send_command("sd", &json!({ "type": "shutdown" }));
    let shutdown = client.read_response("sd");
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");

    await_command_gone(command, Duration::from_secs(90));
}

/// A daemon that is killed outright takes its sessions with it and disposes
/// nothing, so a command a cell left running is orphaned. Each command is
/// journalled before it starts; the next daemon on the socket reaps what the
/// dead one's sessions journalled.
#[test]
fn a_command_orphaned_by_the_daemons_death_is_reaped_at_the_next_boot() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path());

    let mut daemon = spawn_supervisor(&socket, &agent_dir, "15000");
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let _session_id = create_session_with_kernel(&mut client, dir.path(), &script, "c1");
    let command = await_command(dir.path());

    daemon.child.kill().expect("SIGKILL supervisor");
    let _ = daemon.child.wait();
    assert!(
        is_running(command),
        "the command outlives the daemon that started it: only the next boot can end it"
    );

    let _next = spawn_supervisor(&socket, &agent_dir, "15000");
    wait_socket_ready(&socket);
    await_command_gone(command, Duration::from_secs(30));
}
