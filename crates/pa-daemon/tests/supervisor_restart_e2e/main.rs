//! Daemon restart e2e. Sessions are hosted inside the daemon process, so
//! they die with it; what survives is on disk. These tests kill or stop the
//! daemon with sessions in every state and assert what the next boot does:
//! mid-turn sessions are revived from their journals under the same ids,
//! idle ones stay down until a client opens them, and a stop that was under
//! way is finished.
#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "each restart scenario reads top to bottom"
    )
)]
#![cfg(unix)]

#[path = "../common/mod.rs"]
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
    spawn_supervisor_env(socket, agent_dir, &[])
}

fn spawn_supervisor_env(socket: &Path, agent_dir: &Path, extra_env: &[(&str, String)]) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let mut command = Command::new(binary);
    let command = command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Session sockets land in `<TMPDIR>/prime-agent-<uid>/`, which a killed
        // daemon never cleans: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn pa-daemon supervisor");
    Daemon {
        child,
        socket: socket.to_path_buf(),
    }
}

/// Wait until the supervisor socket accepts connections (a restarted
/// supervisor parks on the stale socket file for up to a second before
/// replacing it, so file existence is not readiness).
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

fn child_pids_of(ppid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let Ok(entry_pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{entry_pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next();
        let Ok(parent) = fields.next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        if parent == ppid {
            pids.push(entry_pid);
        }
    }
    pids
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

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// Wait until the recovery journal holds the admission `busy: true` record
/// (the durable busy-at-crash evidence): the kill that follows provably
/// lands mid-turn, whatever the runner's pacing does to the stream.
fn wait_for_busy_journal_evidence(agent_dir: &Path, socket: &Path, session_id: &str) {
    let journal_path = pa_daemon::descriptor::descriptor_dir(agent_dir, socket)
        .join(format!("{session_id}.recovery.jsonl"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let evidence = std::fs::read_to_string(&journal_path).is_ok_and(|content| {
            content.lines().any(|line| {
                let Ok(record) = serde_json::from_str::<Value>(line) else {
                    return false;
                };
                record["activeSessionId"].as_str() == Some(session_id)
                    && record["busy"].as_bool() == Some(true)
            })
        });
        if evidence {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "busy journal evidence never landed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The socket a hosted session serves on, from its descriptor.
fn session_socket(agent_dir: &Path, socket: &Path, worker_id: &str) -> PathBuf {
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(agent_dir, socket);
    let content = std::fs::read_to_string(descriptor_dir.join(format!("{worker_id}.json")))
        .expect("descriptor file");
    let value: Value = serde_json::from_str(&content).expect("descriptor json");
    PathBuf::from(value["socketPath"].as_str().expect("socket path"))
}

/// What "no session outlives its daemon" means once the daemon has exited:
/// nothing serves the sessions' sockets and no session lease is held.
fn assert_sessions_gone(agent_dir: &Path, session_sockets: &[PathBuf]) {
    let serving: Vec<&PathBuf> = session_sockets
        .iter()
        .filter(|session_socket| UnixStream::connect(session_socket).is_ok())
        .collect();
    let held_leases: Vec<PathBuf> = std::fs::read_dir(agent_dir.join("session-leases"))
        .map_or_default(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "lock")
                })
                .collect()
        });
    assert_eq!(
        (serving, held_leases),
        (Vec::<&PathBuf>::new(), Vec::<PathBuf>::new()),
        "a session outlived the daemon's shutdown"
    );
}

fn workers_registered_since(log_path: &Path, since: &str) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((timestamp, message)) = rest.split_once(']') else {
            continue;
        };
        if timestamp < since {
            continue;
        }
        let message = message.trim();
        if let Some(id) = message.strip_prefix("session worker ") {
            if let Some((worker_id, tail)) = id.split_once(" re-registered (epoch") {
                if tail.contains(')') {
                    ids.push(worker_id.to_string());
                }
            } else if let Some((worker_id, tail)) = id.split_once(" registered (epoch") {
                if tail.contains(')') {
                    ids.push(worker_id.to_string());
                }
            }
        }
    }
    ids
}

fn distinct(values: Vec<String>) -> Vec<String> {
    let mut values = values;
    values.sort();
    values.dedup();
    values
}

// The restart regression families live in the child modules (same tree
// position); every child's use-super glob resolves through this root's
// harness, and the one test binary stays one CI shard unit.
mod boot_cleanup;
mod plain_boot;
mod restart;
mod revival;
mod shutdown_continue;
mod update_boot;
