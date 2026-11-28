#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "`force_shutdown_from_an_env_pinned_root_stops_only_that_daemon_tree` is one scenario, top to bottom"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::cast_possible_truncation,
        reason = "the daemon pids read back from its lock are kernel-bounded pids"
    )
)]

//! End-to-end containment tests for the daemon-discovery commands. Every
//! daemon lives in a test-created fixture directory. A discovery scan or
//! `shutdown --force` run from a *different* fixture root must never see or
//! stop it — nor any other live daemon on the machine. Each daemon is the
//! runtime daemon (the default), listening on the socket beside the one it
//! is given (`daemon2.sock`).
#![cfg(unix)]

mod support;

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Environment keys this box's own prime-agent session sets; they must not
/// leak into spawned daemons.
const SCRUB_ENV: [&str; 9] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
];

/// Child-mode env var for the launcher test's re-exec'd client (below):
/// the daemon socket the client must launch the runtime daemon beside.
const LAUNCHER_CLIENT_SOCKET_ENV: &str = "PRIME_AGENT_TEST_LAUNCHER_CLIENT_SOCKET";

fn cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"))
}

/// A canonical fixture root short enough for every socket the product makes
/// under it. macOS's `$TMPDIR` is too long: the product socket dir then falls
/// back to the shared `/tmp/prime-agent-<uid>`, where the user's real daemon
/// listens. Canonical so the paths the product reports compare equal.
fn fixture_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(
        pa_types::platform::canonicalize(Path::new("/tmp")).expect("canonical /tmp"),
    )
    .expect("fixture dir")
}

/// A spawned daemon, killed on drop so a failed test never leaks a daemon
/// into the machine.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the real runtime daemon on a fixture socket the way production does
/// (`prime-agent --mode daemon --daemon-socket`), with agent dir and TMPDIR
/// pinned inside the fixture root so every socket it ever makes stays there.
/// Its sessions answer the faux script `<agent dir>/faux-script.json`; it
/// listens on the socket [`runtime_socket`] names.
fn spawn_daemon(socket: &Path, agent_dir: &Path, tmp_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    std::fs::create_dir_all(tmp_dir).expect("tmp dir");
    let script = agent_dir.join("faux-script.json");
    std::fs::write(
        &script,
        json!({ "responses": [{ "text": "ok", "delayMs": 0 }] }).to_string(),
    )
    .expect("write script");
    let mut command = Command::new(cli_binary());
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("TMPDIR", tmp_dir)
        .env("PRIME_AGENT_FAUX_SCRIPT", &script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    let mut child = command.spawn().expect("spawn the daemon");
    support::await_listening(&runtime_socket(socket), &mut child);
    Daemon { child }
}

/// The socket the runtime daemon listens on when it is started with
/// `--daemon-socket socket`.
fn runtime_socket(socket: &Path) -> PathBuf {
    pa_runtime::daemon::socket_path(socket)
}

/// The pids the runtime daemon on `agent_dir` records in its lock: its
/// restart owner (when kept) and the daemon.
fn lock_holder(agent_dir: &Path) -> (Option<i32>, i32) {
    let holder: Value = serde_json::from_str(
        &std::fs::read_to_string(agent_dir.join("runtime-daemon.lock"))
            .expect("the runtime-daemon lock"),
    )
    .expect("the lock's record");
    (
        holder["keeper"].as_i64().map(|pid| pid as i32),
        holder["pid"].as_i64().expect("the daemon's pid") as i32,
    )
}

/// Run the CLI with its state root pinned inside `root`: agent dir and TMPDIR
/// resolve there, so the invocation's `DaemonStateRoot` can never be the
/// ambient environment's real paths.
fn run_cli(root: &Path, args: &[&str]) -> Output {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut command = Command::new(cli_binary());
    command
        .args(args)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("TMPDIR", root)
        .stdin(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    // The state root follows this var (the env-pinned root contract), so a
    // root-less invocation must not inherit an ambient pin.
    command.env_remove("PRIME_AGENT_DAEMON_SOCKET");
    command.output().expect("spawn CLI binary")
}

/// The same launcher used before the TUI attaches must create the daemon
/// outside the TUI's terminal session. Otherwise closing that terminal can
/// hang up a daemon that still serves other clients.
///
/// The launching client is a separate process: this test binary re-exec'd
/// in the child mode below, with the fixture's state environment pinned on
/// its spawn the same way `spawn_daemon` pins its children. The daemon
/// the client launches inherits that pinned environment — never this
/// process's ambient one, which on a dev box points at the real agent dir —
/// and afterwards the whole client process is gone, not just its runtime:
/// the terminal-close scenario. Only the daemon it spawned (under its restart
/// owner) may remain.
#[test]
fn interactive_launcher_detaches_the_daemon_from_client_session() {
    let fixture = fixture_dir();
    let agent_dir = fixture.path().join("agent");
    let tmp_dir = fixture.path().join("tmp");
    let socket = fixture.path().join("launched.sock");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&tmp_dir).expect("tmp dir");
    let _daemon = DetachedDaemon {
        agent_dir: agent_dir.clone(),
    };

    let mut client = Command::new(std::env::current_exe().expect("test binary"));
    client
        .arg("--exact")
        .arg("launcher_client_child_mode")
        .current_dir(fixture.path())
        .env(LAUNCHER_CLIENT_SOCKET_ENV, &socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("TMPDIR", &tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in SCRUB_ENV {
        client.env_remove(key);
    }
    let client = client.output().expect("spawn launcher client");
    assert!(
        client.status.success(),
        "the launcher client failed: {}{}",
        String::from_utf8_lossy(&client.stdout),
        String::from_utf8_lossy(&client.stderr),
    );
    // The spawning client's process, runtime, and probe connection are gone.

    let socket = runtime_socket(&socket);
    assert!(UnixStream::connect(&socket).is_ok(), "daemon socket");
    // The daemon leads its own session, and so does the restart owner it
    // runs under: neither shares the launching client's terminal session.
    let (keeper, pid) = lock_holder(&agent_dir);
    assert_eq!(unsafe { libc::getsid(pid) }, pid, "daemon owns its session");
    for process in keeper.into_iter().chain([pid]) {
        assert_ne!(
            unsafe { libc::getsid(0) },
            unsafe { libc::getsid(process) },
            "the daemon's process {process} must not share the launching client's terminal session"
        );
    }
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "daemon remains available"
    );
}

/// Child mode for the test above (a plain `cargo test` run without the env
/// var passes trivially): this binary re-exec'd as the launching client —
/// run the real launcher (`ensure_daemon_running_with`, the interactive
/// mode's pre-TUI path) against the fixture socket and exit, leaving only
/// the detached daemon behind.
#[test]
fn launcher_client_child_mode() {
    let Some(socket) = std::env::var_os(LAUNCHER_CLIENT_SOCKET_ENV) else {
        return;
    };
    let socket = PathBuf::from(socket);
    let spawn_cwd = std::env::current_dir().expect("cwd");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(pa_cli::ensure_runtime_daemon_running_with(
            &cli_binary(),
            &socket,
            &spawn_cwd,
        ))
        .expect("launch the daemon");
}

/// Cleanup for a daemon spawned by the interactive launcher rather than by
/// this test process's `Command`, including a failed assertion's unwind: its
/// restart owner is sent the deliberate stop, and the cleanup waits (bounded)
/// for both to be gone, so a daemon never leaks into later test binaries nor
/// outlives the fixture tempdir's removal below.
struct DetachedDaemon {
    agent_dir: PathBuf,
}

impl Drop for DetachedDaemon {
    fn drop(&mut self) {
        support::stop_runtime_daemon(&self.agent_dir);
    }
}

/// Stop the runtime daemon on `socket` by protocol (`daemon.shutdown`).
///
/// # Panics
///
/// The daemon does not answer.
fn shutdown_on_socket(socket: &Path) {
    support::runtime_daemon::Wire2::connect(socket)
        .call("daemon.shutdown", &json!({}))
        .expect("daemon.shutdown");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every socketPath in a JSON report, from both the stopped and failed rows.
fn reported_socket_paths(report: &Value) -> Vec<String> {
    ["stopped", "failed", "reaped", "skipped"]
        .iter()
        .flat_map(|key| {
            report
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|row| {
            row.get("socketPath")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// `status` lists the runtime daemon and `doctor --fix` reaps it.
#[test]
fn doctor_lists_and_then_reaps_an_idle_root_owned_daemon() {
    let fixture = fixture_dir();
    let agent_dir = fixture.path().join("agent");
    // Non-default: not <socket-dir>/daemon.sock (or the runtime daemon's daemon2.sock beside it).
    let daemon_socket_arg = agent_dir.join("daemon.sock");
    let mut daemon = spawn_daemon(&daemon_socket_arg, &agent_dir, &fixture.path().join("tmp"));
    let socket = runtime_socket(&daemon_socket_arg);

    // Inspect-only doctor: the daemon shows up in the table and survives.
    let inspect = run_cli(fixture.path(), &["doctor"]);
    assert_eq!(inspect.status.code(), Some(0), "{}", stderr(&inspect));
    let table = stdout(&inspect);
    let header = table.split('\n').next().unwrap_or_default();
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        ["socket", "pid", "version", "status", "sessions", "uptime"],
        "table header mismatch: {table}"
    );
    assert!(
        table.contains(&*socket.to_string_lossy()),
        "table must list the fixture daemon: {table}"
    );
    assert!(
        !table.contains("* default background service"),
        "a non-default socket must not carry the default footnote: {table}"
    );
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "doctor (inspect) must leave the daemon listening"
    );
    assert!(daemon.child.try_wait().expect("wait").is_none());

    // `doctor --fix` reaps the idle non-default daemon.
    let fix = run_cli(fixture.path(), &["doctor", "--fix", "--json"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    let report: Value = serde_json::from_str(&stdout(&fix)).expect("reap json report");
    let reaped = report
        .get("reaped")
        .and_then(Value::as_array)
        .expect("reaped rows");
    assert_eq!(reaped.len(), 1, "reap report: {}", stdout(&fix));
    assert_eq!(
        reaped[0].get("socketPath").and_then(Value::as_str),
        Some(socket.to_string_lossy().as_ref())
    );
    let action = reaped[0]
        .get("action")
        .and_then(Value::as_str)
        .expect("action");
    assert!(
        action.starts_with("stopped idle background service"),
        "reap action: {action}"
    );
    assert!(
        report
            .get("skipped")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "skipped rows: {}",
        stdout(&fix)
    );
    wait_until_stopped(&socket, &mut daemon);
}

#[test]
fn shutdown_without_force_requires_confirmation_and_harms_nothing() {
    let fixture = fixture_dir();
    let agent_dir = fixture.path().join("agent");
    let mut daemon = spawn_daemon(
        &agent_dir.join("daemon.sock"),
        &agent_dir,
        &fixture.path().join("tmp"),
    );
    let socket = runtime_socket(&agent_dir.join("daemon.sock"));

    // Non-interactive stdin: the router refuses before any discovery.
    let tty_error = run_cli(fixture.path(), &["shutdown"]);
    assert_eq!(tty_error.status.code(), Some(1));
    assert!(
        stderr(&tty_error).contains(
            "Shutdown requires confirmation in an interactive terminal. Use \"prime-agent shutdown --force\"."
        ),
        "router error mismatch: {}{}",
        stdout(&tty_error),
        stderr(&tty_error)
    );
    assert!(UnixStream::connect(&socket).is_ok());

    // JSON without force: the daemon is counted, the report demands --force.
    let json_error = run_cli(fixture.path(), &["shutdown", "--json"]);
    assert_eq!(json_error.status.code(), Some(1));
    let report: Value = serde_json::from_str(&stdout(&json_error)).expect("json report");
    let failed = report
        .get("failed")
        .and_then(Value::as_array)
        .expect("failed rows");
    assert_eq!(failed.len(), 1, "report: {}", stdout(&json_error));
    assert_eq!(
        failed[0].get("socketPath").and_then(Value::as_str),
        Some(socket.to_string_lossy().as_ref())
    );
    assert_eq!(
        failed[0].get("reason").and_then(Value::as_str),
        Some("confirmation required; use \"prime-agent shutdown --force --json\"")
    );
    assert!(
        report
            .get("stopped")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "stopped rows: {}",
        stdout(&json_error)
    );

    // The daemon is untouched by both refusals.
    assert!(UnixStream::connect(&socket).is_ok());
    assert!(daemon.child.try_wait().expect("wait").is_none());
}

#[test]
fn discovery_and_shutdown_from_another_root_leave_a_foreign_daemon_alive() {
    // Fixture A holds a live daemon; fixture B is a completely different
    // state root. Everything run from B must leave A untouched.
    let fixture_a = fixture_dir();
    let fixture_b = fixture_dir();
    let agent_a = fixture_a.path().join("agent");
    // The socket must sit inside A's state root (the agent dir) so root-A
    // discovery is expected to own it; anything at the fixture root itself
    // is outside the root by the TS state-root rules.
    let mut daemon = spawn_daemon(
        &agent_a.join("daemon.sock"),
        &agent_a,
        &fixture_a.path().join("tmp"),
    );
    let socket_a = runtime_socket(&agent_a.join("daemon.sock"));

    // `status` from root B: A is not in B's root, so nothing is reported.
    let status = run_cli(fixture_b.path(), &["status", "--json"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "status stderr: {}",
        stderr(&status)
    );
    assert_eq!(stdout(&status).trim(), "[]");

    // `shutdown --force --json` from root B: succeeds, reports nothing, and
    // reports no socket outside fixture B (a containment regression would
    // surface this sandbox's live ambient daemons here — or kill them).
    let shutdown = run_cli(fixture_b.path(), &["shutdown", "--force", "--json"]);
    assert_eq!(
        shutdown.status.code(),
        Some(0),
        "shutdown stdout: {} stderr: {}",
        stdout(&shutdown),
        stderr(&shutdown)
    );
    let report: Value = serde_json::from_str(&stdout(&shutdown)).expect("shutdown json report");
    assert!(
        reported_socket_paths(&report).is_empty(),
        "shutdown from root B must not report any daemon: {}",
        stdout(&shutdown)
    );

    // The daemon in A survived untouched: still listening, still running.
    assert!(
        UnixStream::connect(&socket_a).is_ok(),
        "foreign daemon socket must still accept connections"
    );
    assert!(
        daemon.child.try_wait().expect("wait on daemon").is_none(),
        "foreign daemon process must still be running"
    );

    // Positive control: the same shutdown rooted at A does stop A's daemon —
    // containment only excludes what is outside the invocation's root.
    let shutdown = run_cli(fixture_a.path(), &["shutdown", "--force", "--json"]);
    assert_eq!(
        shutdown.status.code(),
        Some(0),
        "shutdown stdout: {} stderr: {}",
        stdout(&shutdown),
        stderr(&shutdown)
    );
    let report: Value = serde_json::from_str(&stdout(&shutdown)).expect("shutdown json report");
    let paths = reported_socket_paths(&report);
    assert_eq!(paths, vec![socket_a.to_string_lossy().to_string()]);
    assert!(
        report
            .get("failed")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "stopping the root-owned daemon must not fail: {}",
        stdout(&shutdown)
    );

    // The daemon exits and stops listening.
    wait_until_stopped(&socket_a, &mut daemon);
}

/// Wait for the daemon's socket to stop accepting connections and its
/// process to exit, bounded at 10s (a daemon shutdown handshake is fast;
/// the bound only guards a stuck test).
fn wait_until_stopped(socket: &Path, daemon: &mut Daemon) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(socket).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        UnixStream::connect(socket).is_err(),
        "daemon must stop listening: {}",
        socket.display()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.child.try_wait().expect("wait on daemon").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon.child.try_wait().expect("wait on daemon").is_some(),
        "daemon process must exit: {}",
        socket.display()
    );
}

/// The env-pinned invocation shape: the CLI runs with daemon B's state
/// environment — B's agent dir, the shared TMPDIR, and B's socket through
/// `PRIME_AGENT_DAEMON_SOCKET` (a root-less invocation scrubs an ambient
/// pin so it resolves the default root).
fn run_cli_state_env(
    tmp: &Path,
    agent_dir: &Path,
    daemon_socket: Option<&Path>,
    args: &[&str],
) -> Output {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let mut command = Command::new(cli_binary());
    command
        .args(args)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("TMPDIR", tmp)
        .stdin(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    match daemon_socket {
        Some(socket) => command.env("PRIME_AGENT_DAEMON_SOCKET", socket),
        None => command.env_remove("PRIME_AGENT_DAEMON_SOCKET"),
    };
    command.output().expect("spawn CLI binary")
}

/// One live scripted (faux) session on the runtime daemon on `socket`: a
/// real session whose engine answers from the daemon's script, so a session
/// exists without any model traffic.
fn create_scripted_session(socket: &Path, agent_dir: &Path) -> HostedSession {
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let created = support::runtime_daemon::Wire2::connect(socket)
        .call(
            "session.create",
            &json!({
                "cwd": agent_dir.to_string_lossy(),
                "sessionDir": sessions.to_string_lossy(),
            }),
        )
        .unwrap_or_else(|error| panic!("create failed: {error}"));
    HostedSession {
        socket: socket.to_path_buf(),
        id: created["session"]
            .as_str()
            .expect("the created session's id")
            .to_string(),
    }
}

/// A live session a runtime daemon hosts: sessions run inside their daemon,
/// so the session is alive while its daemon answers and lists it open.
struct HostedSession {
    socket: PathBuf,
    id: String,
}

impl HostedSession {
    /// Whether the daemon still answers and holds the session open.
    fn alive(&self) -> bool {
        support::runtime_daemon::Wire2::try_connect(&self.socket)
            .ok()
            .and_then(|mut client| client.query("sessions.list", &json!({ "open": true })).ok())
            .is_some_and(|list| {
                list["sessions"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["id"] == self.id.as_str()))
            })
    }

    /// Wait (bounded) until nothing serves the session's daemon socket.
    fn wait_gone(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while UnixStream::connect(&self.socket).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        UnixStream::connect(&self.socket).is_err()
    }
}

/// The 2026-10-14 incident, end to end: an isolated test daemon's
/// `shutdown --force` stopped the shared default daemon and every live
/// session on the machine — the state root ignored
/// `PRIME_AGENT_DAEMON_SOCKET`. One shared TMPDIR, like a real machine: A owns
/// the product-default socket, B and C are pinned to sockets outside the
/// product socket dir, C's directory holding B's, each with a live session of
/// its own. A shutdown must stop only the daemon its root owns — the pinned
/// socket's daemon, never the pinned directory's others — and never another
/// daemon's sessions.
#[test]
fn force_shutdown_from_an_env_pinned_root_stops_only_that_daemon_tree() {
    let shared_tmp = fixture_dir();
    let fixture_a = fixture_dir();
    let fixture_b = fixture_dir();
    let fixture_c = fixture_dir();
    // SAFETY: getuid is a pure identity query.
    let socket_dir = shared_tmp
        .path()
        .join(format!("prime-agent-{}", unsafe { libc::getuid() }));

    let pinned_a = socket_dir.join("daemon.sock"); // the product default
    let agent_a = fixture_a.path().join("agent");
    let mut daemon_a = spawn_daemon(&pinned_a, &agent_a, shared_tmp.path());
    let socket_a = runtime_socket(&pinned_a);
    let hosted_a = create_scripted_session(&socket_a, &agent_a);

    // B and C are pinned OUTSIDE the product socket dir, B's socket inside
    // the directory of C's: a pinned root must own its pinned socket's daemon,
    // never a directory (C's shutdown must not see B in its subdirectory, nor
    // B's see C beside it). The runtime daemon listens beside its socket, so
    // the two cannot share one directory.
    let pins_dir = shared_tmp.path().join("pins");
    std::fs::create_dir_all(pins_dir.join("b")).expect("pins dir");

    let agent_b = fixture_b.path().join("agent");
    let pinned_b = pins_dir.join("b").join("b.sock"); // env-pinned, isolated
    let mut daemon_b = spawn_daemon(&pinned_b, &agent_b, shared_tmp.path());
    let socket_b = runtime_socket(&pinned_b);
    let hosted_b = create_scripted_session(&socket_b, &agent_b);

    // Like the launcher-pinned daemon of the incident: C's socket sits
    // outside the product socket dir.
    let agent_c = fixture_c.path().join("agent");
    let pinned_c = pins_dir.join("c.sock");
    let mut daemon_c = spawn_daemon(&pinned_c, &agent_c, shared_tmp.path());
    let socket_c = runtime_socket(&pinned_c);
    let hosted_c = create_scripted_session(&socket_c, &agent_c);

    assert!(
        hosted_a.alive() && hosted_b.alive() && hosted_c.alive(),
        "all three sessions must be live before any shutdown"
    );

    // (a) B's env-pinned shutdown stops only B: A and C are outside B's root.
    let shutdown = run_cli_state_env(
        shared_tmp.path(),
        &agent_b,
        Some(&pinned_b),
        &["shutdown", "--force", "--json"],
    );
    assert_eq!(
        shutdown.status.code(),
        Some(0),
        "shutdown stdout: {} stderr: {}",
        stdout(&shutdown),
        stderr(&shutdown)
    );
    let report: Value = serde_json::from_str(&stdout(&shutdown)).expect("shutdown report");
    assert_eq!(
        reported_socket_paths(&report),
        vec![socket_b.to_string_lossy().to_string()],
        "the B-rooted report must name only B: {}",
        stdout(&shutdown)
    );
    wait_until_stopped(&socket_b, &mut daemon_b);
    assert!(
        hosted_b.wait_gone(Duration::from_secs(10)),
        "B's session must stop with B"
    );
    assert!(
        daemon_a
            .child
            .try_wait()
            .expect("wait on daemon A")
            .is_none(),
        "A's daemon must survive B's env-pinned shutdown"
    );
    assert!(
        hosted_a.alive(),
        "A's session must survive B's env-pinned shutdown"
    );
    assert!(
        daemon_c
            .child
            .try_wait()
            .expect("wait on daemon C")
            .is_none(),
        "C's daemon must survive B's env-pinned shutdown"
    );
    assert!(
        hosted_c.alive(),
        "C's session must survive B's env-pinned shutdown"
    );

    // (b) The default-rooted shutdown from A stops A and its session, and
    // must leave C alone.
    let shutdown = run_cli_state_env(
        shared_tmp.path(),
        &agent_a,
        None,
        &["shutdown", "--force", "--json"],
    );
    assert_eq!(
        shutdown.status.code(),
        Some(0),
        "shutdown stdout: {} stderr: {}",
        stdout(&shutdown),
        stderr(&shutdown)
    );
    assert!(
        daemon_c
            .child
            .try_wait()
            .expect("wait on daemon C")
            .is_none(),
        "C's daemon must survive A's default-rooted shutdown"
    );
    assert!(
        hosted_c.alive(),
        "C's session must survive A's default-rooted shutdown"
    );
    let report: Value = serde_json::from_str(&stdout(&shutdown)).expect("shutdown report");
    assert_eq!(
        reported_socket_paths(&report),
        vec![socket_a.to_string_lossy().to_string()],
        "the A-rooted report must name only A: {}",
        stdout(&shutdown)
    );
    wait_until_stopped(&socket_a, &mut daemon_a);
    assert!(
        hosted_a.wait_gone(Duration::from_secs(10)),
        "A's session must stop with A"
    );

    // (c) Cleanup: C's own env-pinned shutdown stops C and its session.
    let shutdown = run_cli_state_env(
        shared_tmp.path(),
        &agent_c,
        Some(&pinned_c),
        &["shutdown", "--force", "--json"],
    );
    assert_eq!(
        shutdown.status.code(),
        Some(0),
        "shutdown stdout: {} stderr: {}",
        stdout(&shutdown),
        stderr(&shutdown)
    );
    let report: Value = serde_json::from_str(&stdout(&shutdown)).expect("shutdown report");
    assert_eq!(
        reported_socket_paths(&report),
        vec![socket_c.to_string_lossy().to_string()],
        "the C-rooted report must name only C: {}",
        stdout(&shutdown)
    );
    wait_until_stopped(&socket_c, &mut daemon_c);
    assert!(
        hosted_c.wait_gone(Duration::from_secs(10)),
        "C's session must stop with C"
    );
}

/// An agent-dir override can contain the shared TMPDIR while its own daemon
/// is pinned elsewhere. Such a root must still exclude other daemons from
/// both discovery and the force shutdown.
#[test]
fn a_pinned_root_with_an_overlapping_agent_dir_never_stops_foreign_workers() {
    let shared_tmp = fixture_dir();
    let pins = fixture_dir();
    let foreign_fixture = fixture_dir();
    std::fs::create_dir_all(pins.path().join("foreign")).expect("foreign pins dir");
    let own_pinned = pins.path().join("own.sock");
    let foreign_pinned = pins.path().join("foreign").join("foreign.sock");
    // The shared TMPDIR is also this root's agent dir. Neither daemon socket
    // is below it.
    let own_agent = shared_tmp.path();
    let foreign_agent = foreign_fixture.path().join("agent");
    let mut own_daemon = spawn_daemon(&own_pinned, own_agent, shared_tmp.path());
    let own_socket = runtime_socket(&own_pinned);
    let own_hosted = create_scripted_session(&own_socket, own_agent);
    let mut foreign_daemon = spawn_daemon(&foreign_pinned, &foreign_agent, shared_tmp.path());
    let foreign_socket = runtime_socket(&foreign_pinned);
    let foreign_hosted = create_scripted_session(&foreign_socket, &foreign_agent);

    let status = run_cli_state_env(
        shared_tmp.path(),
        own_agent,
        Some(&own_pinned),
        &["status", "--json"],
    );
    assert!(status.status.success(), "{}", stderr(&status));
    let services: Vec<Value> = serde_json::from_str(&stdout(&status)).expect("status report");
    let sockets: Vec<&str> = services
        .iter()
        .map(|service| service["socketPath"].as_str().expect("socket path"))
        .collect();
    assert_eq!(
        sockets,
        vec![own_socket.to_str().expect("fixture path")],
        "a pinned root must discover its own daemon only: {}",
        stdout(&status)
    );

    let shutdown = run_cli_state_env(
        shared_tmp.path(),
        own_agent,
        Some(&own_pinned),
        &["shutdown", "--force", "--json"],
    );
    assert!(shutdown.status.success(), "{}", stderr(&shutdown));
    wait_until_stopped(&own_socket, &mut own_daemon);
    assert!(own_hosted.wait_gone(Duration::from_secs(10)));
    assert!(
        foreign_daemon
            .child
            .try_wait()
            .expect("foreign daemon wait")
            .is_none()
            && foreign_hosted.alive(),
        "the foreign daemon and its original session must survive"
    );
    assert!(UnixStream::connect(&foreign_socket).is_ok());

    // Teardown names the exact fixture socket; no machine-wide discovery.
    shutdown_on_socket(&foreign_socket);
    wait_until_stopped(&foreign_socket, &mut foreign_daemon);
    assert!(foreign_hosted.wait_gone(Duration::from_secs(10)));
}
