//! End-to-end fd-lifecycle audit against the real `pa-daemon`. Sessions are hosted inside
//! the daemon process, so every descriptor a session opens is the daemon's and one leaked
//! per session, prompt, connection or failed rebuild accumulates in the process that hosts
//! every other session. Each test samples the daemon's `/proc/<pid>/fd` around the work and
//! holds it to its baseline.
// Stack-resident futures by design; boxing for a lint tick is a perf regression.
#![allow(clippy::large_futures)]
// 64-bit-only targets; narrowing casts sit at bounded OS/protocol boundaries.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
// API-shape opinions, not defects; the surfaces are deliberate.
#![allow(
    clippy::unnecessary_wraps,
    clippy::zero_sized_map_values,
    clippy::struct_excessive_bools,
    clippy::struct_field_names
)]
// Every probe reads /proc (fd tables, process state, limits): Linux only.
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Sample the open-fd set of a process (empty for a dead pid).
fn fd_snapshot(pid: u32) -> Vec<String> {
    let dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let mut targets = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return targets;
    };
    for entry in entries.flatten() {
        let target = std::fs::read_link(entry.path())
            .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
        targets.push(target);
    }
    targets
}

/// Histogram of fd readlink targets (socket inodes collapse to a class).
fn fd_classes(targets: &[String]) -> Vec<(String, usize)> {
    let mut classes: HashMap<String, usize> = HashMap::new();
    for target in targets {
        let class = if target.starts_with("socket:") {
            "socket".to_string()
        } else if target.starts_with("pipe:") {
            "pipe".to_string()
        } else if target.starts_with("/memfd:") {
            "memfd".to_string()
        } else if target.starts_with("/dev/") {
            "device".to_string()
        } else {
            "file".to_string()
        };
        *classes.entry(class).or_default() += 1;
    }
    let mut rows: Vec<(String, usize)> = classes.into_iter().collect();
    rows.sort();
    rows
}

const DAEMON: &str = env!("CARGO_BIN_EXE_pa-daemon");

struct Daemon {
    child: Child,
    #[expect(dead_code)]
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
fn spawn_daemon(mut launcher: Command, socket: &Path, agent_dir: &Path) -> Daemon {
    let child = launcher
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Session sockets land in `<TMPDIR>/prime-agent-<uid>/`: keep them in
        // the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Daemon {
                child,
                socket: socket.to_path_buf(),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, serde_json::Value) {
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
        (client, hello)
    }

    fn send_command(&mut self, id: &str, command: &serde_json::Value) {
        let envelope = serde_json::json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse response line"),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn read_response(&mut self, id: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(|v| v.as_str()) == Some(id) {
                return line;
            }
        }
    }
}

/// Whether the daemon still holds a record of the session (its descriptor file): a
/// stopped session is retired once this is gone.
fn session_recorded(agent_dir: &Path, socket_path: &Path, worker_id: &str) -> bool {
    pa_daemon::descriptor::descriptor_dir(agent_dir, socket_path)
        .join(format!("{worker_id}.json"))
        .exists()
}

#[track_caller]
fn wait_until(deadline: Duration, mut probe: impl FnMut() -> bool) {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if probe() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not reached within {deadline:?}");
}

const CYCLES: usize = 20;
/// fds that may legitimately sit open on top of the steady state.
const FD_SLACK: usize = 6;

#[test]
fn daemon_fd_count_returns_to_baseline_across_session_cycles() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let daemon = spawn_daemon(Command::new(DAEMON), &socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [ { "text": "cycled" } ] }).to_string(),
    )
    .expect("write script");

    // Warm-up cycle: the first launch may populate lazy caches; the steady-state
    // baseline is what the cycle set must hold.
    let mut counts: Vec<usize> = Vec::new();
    for cycle in 0..=CYCLES {
        let create_id = format!("c{cycle}");
        client.send_command(
            &create_id,
            &serde_json::json!({
                "type": "create",
                "config": {
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                    "script": script_path.to_string_lossy(),
                },
            }),
        );
        let created = client.read_response(&create_id);
        assert_eq!(created["success"], true, "create failed: {created}");
        let session_id = created["data"]["id"]
            .as_str()
            .or_else(|| created["data"]["sessionId"].as_str())
            .expect("session id")
            .to_string();
        client.send_command(
            &format!("a{cycle}"),
            &serde_json::json!({ "type": "attach", "activeSessionId": session_id }),
        );
        assert_eq!(
            client.read_response(&format!("a{cycle}"))["success"],
            true,
            "attach failed"
        );
        client.send_command(
            &format!("p{cycle}"),
            &serde_json::json!({
                "type": "prompt_and_wait",
                "activeSessionId": session_id,
                "message": "cycle",
            }),
        );
        let prompt = client.read_response(&format!("p{cycle}"));
        assert_eq!(prompt["success"], true, "prompt failed: {prompt}");

        client.send_command(
            &format!("k{cycle}"),
            &serde_json::json!({ "type": "kill", "activeSessionId": session_id }),
        );
        let killed = client.read_response(&format!("k{cycle}"));
        assert_eq!(killed["success"], true, "kill failed: {killed}");

        // The kill settles asynchronously: the session's record is retired and the
        // roster drops the row; sample only once both are observed, so a slow stop
        // cannot fake stability.
        wait_until(Duration::from_secs(10), || {
            !session_recorded(&agent_dir, &socket, &session_id)
        });
        wait_until(Duration::from_secs(10), || {
            client.send_command("l", &serde_json::json!({ "type": "list" }));
            let list = client.read_response("l");
            list["data"]["sessions"]
                .as_array()
                .is_some_and(std::vec::Vec::is_empty)
        });

        let targets = fd_snapshot(supervisor_pid);
        println!(
            "cycle {cycle}: daemon open fds {} ({:?})",
            targets.len(),
            fd_classes(&targets)
        );
        if !targets.is_empty() {
            counts.push(targets.len());
        }
    }

    // Baseline: the steady state after the warm-up cycle.
    let baseline = counts[0];
    for (index, count) in counts.iter().enumerate() {
        assert!(
            *count <= baseline + FD_SLACK,
            "the daemon's fd count grew across session cycles: baseline {baseline}, \
             cycle {index} count {count} ({:?} at cycle {index})",
            fd_classes(&fd_snapshot(supervisor_pid))
        );
    }
    assert!(
        counts.last().copied().unwrap_or_default() >= 3,
        "fd sampling looked broken (no fds observed)"
    );
}

#[test]
fn daemon_fd_count_stable_across_a_sessions_prompts() {
    const PROMPTS: usize = 30;
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let daemon = spawn_daemon(Command::new(DAEMON), &socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
            { "text": "turn answer" },
        ] })
        .to_string(),
    )
    .expect("write script");

    client.send_command(
        "c0",
        &serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c0");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();
    client.send_command(
        "a0",
        &serde_json::json!({ "type": "attach", "activeSessionId": session_id }),
    );
    assert_eq!(client.read_response("a0")["success"], true, "attach failed");

    let mut baseline: Option<usize> = None;
    for turn in 0..PROMPTS {
        let id = format!("p{turn}");
        client.send_command(
            &id,
            &serde_json::json!({
                "type": "prompt_and_wait",
                "activeSessionId": session_id,
                "message": format!("turn {turn}"),
            }),
        );
        let prompt = client.read_response(&id);
        assert_eq!(prompt["success"], true, "prompt failed: {prompt}");
        if turn % 5 == 0 {
            let daemon_fds = fd_snapshot(supervisor_pid);
            println!(
                "turn {turn}: daemon fds {} ({:?})",
                daemon_fds.len(),
                fd_classes(&daemon_fds),
            );
            // The turn's roster pushes each dial their own socket and may
            // still be in flight after the reply; a leaked fd never drains,
            // a push's socket does.
            let baseline = *baseline.get_or_insert(daemon_fds.len());
            wait_until(Duration::from_secs(10), || {
                fd_snapshot(supervisor_pid).len() <= baseline + FD_SLACK
            });
        }
    }
    assert!(
        baseline.is_some_and(|baseline| baseline >= 3),
        "fd sampling looked broken (no fds observed)"
    );
}

// The daemon's fd table across connection churn on a session's socket (the EMFILE class)

/// Pids whose parent is `ppid`.
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

fn read_exact_timeout(stream: &mut UnixStream, buffer: &mut [u8], deadline: Instant) {
    use std::io::Read as _;
    let mut read = 0usize;
    while read < buffer.len() {
        assert!(Instant::now() < deadline, "worker frame read timed out");
        match stream.read(&mut buffer[read..]) {
            Ok(0) => panic!("worker closed the connection mid-frame"),
            Ok(n) => read += n,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("worker read: {error}"),
        }
    }
}

/// A raw private-frame probe against a session's own socket: connect, consume the
/// hello frame, drop — the exact shape of liveness probes and peer deliveries.
fn probe_worker(socket: &Path) {
    let mut stream = UnixStream::connect(socket).expect("connect worker socket");
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set timeout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut prefix = [0u8; 8];
    read_exact_timeout(&mut stream, &mut prefix, deadline);
    let header_len = u32::from_be_bytes(prefix[0..4].try_into().expect("len")) as usize;
    let payload_len = u32::from_be_bytes(prefix[4..8].try_into().expect("len")) as usize;
    let mut header = vec![0u8; header_len];
    read_exact_timeout(&mut stream, &mut header, deadline);
    let mut payload = vec![0u8; payload_len];
    read_exact_timeout(&mut stream, &mut payload, deadline);
    let header: serde_json::Value = serde_json::from_slice(&header).expect("frame header");
    assert_eq!(header["outboundType"], "daemon_hello");
}

/// The socket the hosted session serves on, from its descriptor.
fn worker_socket_path(agent_dir: &Path, socket: &Path, worker_id: &str) -> PathBuf {
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(agent_dir, socket);
    let content = std::fs::read_to_string(descriptor_dir.join(format!("{worker_id}.json")))
        .expect("descriptor file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("descriptor json");
    PathBuf::from(value["socketPath"].as_str().expect("socket path"))
}

#[test]
fn daemon_fd_table_stable_across_connection_churn_on_a_session_socket() {
    const PROBES: usize = 15;
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let daemon = spawn_daemon(Command::new(DAEMON), &socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [ { "text": "churn" } ] }).to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        &serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();
    let worker_socket = worker_socket_path(&agent_dir, &socket, &session_id);

    // Warm-up probes, then the baseline: the count the table settles back to
    // once the session has seen each probe's EOF.
    for _ in 0..2 {
        probe_worker(&worker_socket);
    }
    let mut baseline = fd_snapshot(supervisor_pid).len();
    wait_until(Duration::from_secs(10), || {
        let settled = fd_snapshot(supervisor_pid).len() == baseline;
        baseline = fd_snapshot(supervisor_pid).len();
        settled
    });
    println!("daemon baseline fds {baseline}");

    for _ in 0..PROBES {
        probe_worker(&worker_socket);
    }
    // The session's read loops observe EOF and the fan-out tasks exit: a
    // leaked connection never drains, a closing one does.
    wait_until(Duration::from_secs(10), || {
        fd_snapshot(supervisor_pid).len() <= baseline + 1
    });

    // The session still serves after the churn: a routed command round-trips.
    client.send_command(
        "g1",
        &serde_json::json!({ "type": "get_last_assistant_text", "activeSessionId": session_id }),
    );
    let last = client.read_response("g1");
    assert_eq!(last["success"], true, "routed command failed: {last}");

    client.send_command(
        "k1",
        &serde_json::json!({ "type": "kill", "activeSessionId": session_id }),
    );
    let killed = client.read_response("k1");
    assert_eq!(killed["success"], true, "kill failed: {killed}");
    wait_until(Duration::from_secs(10), || {
        !session_recorded(&agent_dir, &socket, &session_id)
    });
}

// Rebuild loop: nothing left behind, stable fds, terminal cleanup

/// The daemon's rebuild loop must not leak when the create replay keeps failing (a corrupt
/// session store): each failed rebuild releases what it opened, and the give-up leaves no
/// process, no lease and no extra descriptor behind (the "failure 4/5 restart loop" shape
/// from the EMFILE report).
#[test]
fn a_failing_rebuild_loop_leaves_nothing_behind() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let daemon = spawn_daemon(Command::new(DAEMON), &socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({
            "responses": [ { "text": "once" } ],
            "panicOn": { "message": "crash now", "panic": "injected fault" },
        })
        .to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        &serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();
    let session_file = created["data"]["sessionFile"]
        .as_str()
        .expect("session file in create response")
        .to_string();

    let baseline = fd_snapshot(supervisor_pid);

    // Corrupt the durable session file, then crash the session: every create replay fails.
    std::fs::write(&session_file, "not valid jsonl\n").expect("corrupt session file");
    client.send_command(
        "crash",
        &serde_json::json!({
            "type": "prompt",
            "activeSessionId": session_id,
            "message": "crash now",
        }),
    );

    // The rebuild loop runs to its failure budget and removes the session.
    wait_until(Duration::from_mins(1), || {
        client.send_command("l", &serde_json::json!({ "type": "list" }));
        let list = client.read_response("l");
        list["data"]["sessions"]
            .as_array()
            .is_some_and(std::vec::Vec::is_empty)
    });

    // Nothing of the session survives the loop: no process under the daemon, no lease.
    let held_leases: Vec<PathBuf> = std::fs::read_dir(agent_dir.join("session-leases"))
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
        .unwrap_or_default();
    assert_eq!(
        (child_pids_of(supervisor_pid), held_leases),
        (Vec::<u32>::new(), Vec::<PathBuf>::new()),
        "the failed rebuilds left something behind"
    );

    // The daemon's own fd table returns to baseline: the session that held
    // its share is gone, and no failed rebuild kept a descriptor.
    wait_until(Duration::from_secs(10), || {
        fd_snapshot(supervisor_pid).len() <= baseline.len() + 1
    });
}

/// A supervisor started under a low soft fd limit raises it to the hard
/// limit before it binds its socket (macOS starts every process at 256).
#[cfg(target_os = "linux")]
#[test]
fn supervisor_raises_a_low_soft_fd_limit_to_the_hard_limit() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut launcher = Command::new("sh");
    launcher.args(["-c", r#"ulimit -Sn 64 && exec "$0" "$@""#, DAEMON]);
    let daemon = spawn_daemon(launcher, &socket, &agent_dir);
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", daemon.child.id()))
        .expect("read supervisor limits");
    let open_files: Vec<&str> = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .expect("open files row")
        .split_whitespace()
        .skip(3)
        .take(2)
        .collect();
    assert_eq!(open_files[0], open_files[1], "{limits}");
}
