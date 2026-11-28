#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "`runtime_daemon_cli_commands_end_to_end` walks every daemon command in one scenario"
    )
)]

//! End-to-end daemon-command tests: spawn the runtime daemon (`prime-agent
//! --mode daemon`, the default, on `daemon2.sock` beside a temp socket),
//! drive `prime-agent` list/stop/rename/schedule/send against it (the
//! commands speaking wire2 to it), and diff the output against goldens
//! captured from the installed TS `prime-agent` binary. The runtime daemon
//! addresses a session by its durable id: the CLI shows the id's last 12 hex
//! digits, and that short form reaches open sessions only.
//!
//! Goldens (captured from the live TS binary, protocol 7):
//!
//! ```text
//! $ prime-agent list            # one live session named "research"
//! name      id            status  age  model                         messages  clients
//! research  b72ad7009b11  idle    3s   prime-inference/z-ai/glm-5.3  0         0
//!
//! $ prime-agent list           # no live sessions
//! No active agents.
//!
//! $ prime-agent list --all     # no saved sessions
//! No agents.
//!
//! $ prime-agent stop <id>
//! ok
//!
//! $ prime-agent rename <id> gamma
//! Renamed 0f04b31cb2b6 to gamma
//!
//! $ prime-agent schedule add gamma "every 5 minutes" -- "do the thing"
//! Scheduled 0451e5a9-951c-457b-ae76-ea3142ea5d25 next=2026-09-16T18:36:59.287Z
//!
//! $ prime-agent schedule list
//! 0451e5a9-951c-457b-ae76-ea3142ea5d25 active next=9/16/2026, 6:36:59 PM last=- runs=0 schedule="every 5 minutes" prompt="do the thing"
//! ```
#![cfg(unix)]

mod support;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// Env keys this box's own worker sets; they must not leak into spawned
/// daemons. `PI_PACKAGE_DIR` too: it redirects the package dir both binaries resolve.
const SCRUB_ENV: [&str; 10] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
    "PI_PACKAGE_DIR",
];

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
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The runtime daemon on the agent dir, beside `daemon_socket` (it listens
/// on `daemon2.sock` there), its faux provider scripted by `script`.
fn spawn_runtime_daemon(
    dir: &Path,
    agent_dir: &Path,
    daemon_socket: &Path,
    script: &Value,
) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(daemon_socket)
        .current_dir(dir)
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .env("PRIME_AGENT_AGENT_DIR", agent_dir)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .env("PRIME_AGENT_RUNTIME_DAEMON", "1")
        .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    let socket = daemon_socket.with_file_name("daemon2.sock");
    let mut child = command.spawn().expect("spawn the runtime daemon");
    support::await_listening(&socket, &mut child);
    Daemon { child, socket }
}

/// A blocking wire2 client of the runtime daemon.
struct Wire2 {
    stream: UnixStream,
    decoder: pa_types::daemon::framing::PrivateFrameDecoder,
    frames: VecDeque<Value>,
    next_id: u64,
}

impl Wire2 {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to the runtime daemon");
        stream
            .set_read_timeout(Some(Duration::from_mins(1)))
            .expect("set timeout");
        let mut wire = Wire2 {
            stream,
            decoder: pa_types::daemon::framing::PrivateFrameDecoder::new(
                pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
            ),
            frames: VecDeque::new(),
            next_id: 0,
        };
        wire.send(&json!({ "type": "hello", "version": pa_types::wire2::WIRE2_VERSION, "client": "daemon-commands-e2e" }));
        let welcome = wire.frame();
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        wire
    }

    fn send(&mut self, frame: &Value) {
        let bytes = pa_types::daemon::framing::encode_private_frame(
            frame,
            &[],
            pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .expect("encode a frame");
        self.stream.write_all(&bytes).expect("send");
    }

    fn frame(&mut self) -> Value {
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return frame;
            }
            let read = self
                .stream
                .read(&mut chunk)
                .expect("a frame within the wait");
            assert!(read > 0, "the runtime daemon closed the connection");
            for frame in self.decoder.push(&chunk[..read]).expect("decode frames") {
                self.frames.push_back(frame.header);
            }
        }
    }

    /// Makes `method` with `args`; its result.
    fn call(&mut self, method: &str, args: &Value) -> Value {
        self.next_id += 1;
        let id = format!("c{}", self.next_id);
        self.send(&json!({ "type": "call", "id": id, "method": method, "args": args }));
        loop {
            let frame = self.frame();
            if frame["type"] == "reply" && frame["id"] == id.as_str() {
                assert_eq!(frame["status"], "ok", "{method}: {frame}");
                return frame["result"].clone();
            }
        }
    }
}

/// Run a CLI in an isolated env (agent dir + TMPDIR point into the temp dir),
/// on the default daemon (the runtime daemon).
fn run_cli(binary: &Path, dir: &Path, agent_dir: &Path, args: &[&str]) -> Output {
    run_cli_with(&[], binary, dir, agent_dir, args)
}

/// [`run_cli`] with `env` set.
fn run_cli_with(
    env: &[(&str, &str)],
    binary: &Path,
    dir: &Path,
    agent_dir: &Path,
    args: &[&str],
) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .envs(env.iter().copied())
        .current_dir(dir)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("TMPDIR", dir);
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    // The runtime daemon is the default.
    command.env_remove("PRIME_AGENT_RUNTIME_DAEMON");
    command.output().expect("spawn CLI binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Replace volatile tokens (ages, hex ids, uuids, timestamps) so outputs compare equal.
fn normalize(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.split_whitespace()
                .map(normalize_token)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_token(token: &str) -> String {
    if let Some((key, value)) = token.split_once('=') {
        return format!("{key}={}", normalize_token(value));
    }
    if let Some(stripped) = token.strip_suffix(',') {
        return format!("{},", normalize_token(stripped));
    }
    // The strict, fixed-length id shapes are matched first: a 12-char hex
    // display id can read as an age (eleven digits plus a `d` suffix), and
    // the age matcher would otherwise fold it into `<time>` and break the
    // golden on a legal id draw.
    if is_hex_id(token) || is_uuid(token) || is_iso_timestamp(token) || is_local_date(token) {
        return "<timestamp>".to_string();
    }
    if is_age(token) || is_clock(token) || token == "AM" || token == "PM" {
        return "<time>".to_string();
    }
    token.to_string()
}

fn is_age(token: &str) -> bool {
    let digits = token
        .strip_suffix(['s', 'm', 'h', 'd', 'w', 'y'])
        .unwrap_or("");
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn is_hex_id(token: &str) -> bool {
    token.len() == 12 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_uuid(token: &str) -> bool {
    token.len() == 36
        && token.split('-').count() == 5
        && token
            .split('-')
            .map(str::len)
            .zip([8, 4, 4, 4, 12])
            .all(|(actual, expected)| actual == expected)
}

fn is_iso_timestamp(token: &str) -> bool {
    token.len() >= 20 && token.starts_with(|c: char| c.is_ascii_digit()) && token.contains('T')
}

fn is_local_date(token: &str) -> bool {
    let Some((month_day_year, _)) = token.split_once('/') else {
        return false;
    };
    month_day_year.chars().all(|c| c.is_ascii_digit())
}

fn is_clock(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[test]
fn hex_display_id_normalizes_as_a_timestamp_even_when_it_reads_as_an_age() {
    // The display id is the last 12 hex chars of a random uuid, so one
    // draw in ~2800 is eleven digits plus `d` — the exact shape of the
    // age matcher. It is an id, and must fold into `<timestamp>` (the
    // CI flake this regression test pins; the real ages keep `<time>`).
    assert_eq!(normalize_token("12345678901d"), "<timestamp>");
    assert_eq!(normalize_token("b72ad7009b11"), "<timestamp>");
    assert_eq!(normalize_token("3s"), "<time>");
    assert_eq!(normalize_token("2h"), "<time>");
    assert_eq!(normalize_token("10:34:59"), "<time>");
    assert_eq!(normalize_token("PM"), "<time>");
}

/// CLI args with the daemon socket flag prepended.
fn list_args<'a>(socket: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&'a str> = vec!["--daemon-socket", socket];
    args.extend(rest.iter().copied());
    args
}

// Rust end-to-end: the runtime daemon + prime-agent

#[test]
fn runtime_daemon_cli_commands_end_to_end() {
    let dir = fixture_dir();
    let agent_dir = dir.path().join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let socket = dir.path().join("daemon.sock");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let daemon = spawn_runtime_daemon(
        dir.path(),
        &agent_dir,
        &socket,
        &json!({ "responses": ["hello"], "repeatLastResponse": true }),
    );
    let socket_str = socket.to_string_lossy().to_string();

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(stdout(&list), "No active agents.\n");

    let list_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--all"]),
    );
    assert_eq!(stdout(&list_all), "No agents.\n");

    let bogus = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--bogus"]),
    );
    assert_eq!(bogus.status.code(), Some(1));
    assert_eq!(stderr(&bogus), "Error: Unknown list option: --bogus\n");

    let sessions_empty = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions"]),
    );
    assert_eq!(sessions_empty.status.code(), Some(0));
    assert_eq!(stdout(&sessions_empty), "No active agents.\n");

    let sessions_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--all"]),
    );
    assert_eq!(stdout(&sessions_all), "No agents.\n");

    let sessions_bogus = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--bogus"]),
    );
    assert_eq!(sessions_bogus.status.code(), Some(1));
    assert_eq!(
        stderr(&sessions_bogus),
        "Error: Unknown sessions option: --bogus\n"
    );

    // `session` addresses the session (its durable id's last 12 hex digits);
    // `printed` is the id a rename prints.
    let (session, printed) = {
        let mut wire = Wire2::connect(&daemon.socket);
        let created = wire.call("session.create", &json!({ "cwd": dir.path() }));
        let id = created["session"]
            .as_str()
            .expect("a session id")
            .to_string();
        wire.call(
            "session.rename",
            &json!({ "session": id, "name": "parity" }),
        );
        let hex = id.replace('-', "");
        (hex[hex.len() - 12..].to_string(), id)
    };

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(list.status.code(), Some(0), "{}", stderr(&list));
    // A runtime daemon session runs the faux provider's model.
    assert_eq!(
        normalize(&stdout(&list)),
        "name id status age model messages clients\n\
         parity <timestamp> idle <time> faux/faux-1 0 0\n",
        "table shape must match the TS golden"
    );

    // The --json output carries the daemon's summary rows unmodified.
    let list_json = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--json"]),
    );
    let parsed: Value = serde_json::from_str(&stdout(&list_json)).expect("valid json list");
    let row = &parsed["sessions"][0];
    for key in [
        "id",
        "sessionId",
        "cwd",
        "lifecycle",
        "activity",
        "isSessionActive",
        "isStreaming",
        "isCompacting",
        "attachedClients",
        "messageCount",
        "sessionActions",
    ] {
        assert!(row.get(key).is_some(), "summary row must carry {key}");
    }

    // Golden: the sessions operator table over the live session (TS binary
    // shape): the last-heard, activity, error, and usage cells stay empty for
    // a healthy idle session.
    let sessions_table = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions"]),
    );
    assert_eq!(
        sessions_table.status.code(),
        Some(0),
        "{}",
        stderr(&sessions_table)
    );
    assert_eq!(
        normalize(&stdout(&sessions_table)),
        "name status activity last heard error usage\nparity idle\n",
        "sessions table shape must match the TS golden"
    );

    // The sessions --json dump is the same list RPC data (the sessions
    // command reuses it).
    let sessions_json = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--json"]),
    );
    let parsed: Value = serde_json::from_str(&stdout(&sessions_json)).expect("valid json sessions");
    assert_eq!(parsed["sessions"][0]["sessionName"], "parity");

    let rename = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["rename", session.as_str(), "renamed"]),
    );
    assert_eq!(rename.status.code(), Some(0), "{}", stderr(&rename));
    assert_eq!(stdout(&rename), format!("Renamed {printed} to renamed\n"));

    // A prompt scheduled on the live session, listed, then cancelled (the
    // schedule commands take no socket flag: the socket comes from the
    // environment).
    let schedule = |args: &[&str]| {
        let mut full = vec!["schedule"];
        full.extend(args);
        let out = run_cli_with(
            &[("PRIME_AGENT_DAEMON_SOCKET", socket_str.as_str())],
            &cli,
            dir.path(),
            &agent_dir,
            &full,
        );
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        stdout(&out)
    };
    let added = schedule(&[
        "add",
        session.as_str(),
        "every 5 minutes",
        "--",
        "do the thing",
    ]);
    assert_eq!(
        normalize(&added),
        "Scheduled <timestamp> next=<timestamp>\n",
        "{added}"
    );
    let job = added
        .split_whitespace()
        .nth(1)
        .expect("the job id")
        .to_string();
    assert_eq!(
        normalize(&schedule(&["list"])),
        "<timestamp> active next=<timestamp>, <time> <time> last=- runs=0 schedule=\"every 5 minutes\" prompt=\"do the thing\"\n"
    );
    assert_eq!(
        schedule(&["cancel", job.as_str()]),
        format!("Cancelled {job}\n")
    );
    // A cancelled job is listed no more.
    assert_eq!(schedule(&["list"]), "No scheduled prompts.\n");

    let stop = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["stop", session.as_str()]),
    );
    assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), "ok\n");

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(stdout(&list), "No active agents.\n");

    let stop_missing = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["stop", "missing-selector"]),
    );
    assert_eq!(stop_missing.status.code(), Some(1));
    assert_eq!(
        stderr(&stop_missing),
        "Error: Unknown active session: missing-selector\n"
    );

    // Saved sessions still appear in list --all after the stop.
    let list_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--all"]),
    );
    let normalized = normalize(&stdout(&list_all));
    assert_eq!(normalized.lines().count(), 2, "saved row after kill");
    assert!(normalized.contains("<timestamp>"), "{normalized}");

    // send_message by the stopped session's short id answers the unknown-session
    // error: the short form reaches open sessions only; the catalog keys saved
    // sessions by id and name.
    let send = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["send", session.as_str(), "hello"]),
    );
    assert_eq!(send.status.code(), Some(1));
    assert_eq!(
        stderr(&send),
        format!("Error: Unknown active session: {session}\n")
    );

    // The saved-session wake (messaging-7): sending by the NAME wakes it — the
    // catalog resolves the selector, the daemon opens the session, and delivers.
    let send_wake = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["send", "renamed", "hello again"]),
    );
    assert_eq!(send_wake.status.code(), Some(0), "{}", stderr(&send_wake));
    assert_eq!(stdout(&send_wake), "Sent to renamed\n");
}

#[test]
fn cli_connect_error_matches_ts_golden() {
    let dir = fixture_dir();
    let agent_dir = dir.path().join("agent");
    let given = dir.path().join("missing.sock");
    // The runtime daemon's socket beside the one given: the one the CLI connects to.
    let missing = pa_runtime::daemon::socket_path(&given);
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let output = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &["--daemon-socket", given.to_string_lossy().as_ref(), "list"],
    );
    assert_eq!(output.status.code(), Some(1));

    let text = stderr(&output).trim_end().to_string();
    let expected_prefix = format!(
        "Error: Failed to connect to the Prime Agent daemon: connect ENOENT {missing}. Socket: {missing}. Daemon log: ",
        missing = missing.display()
    );
    assert!(
        text.starts_with(&expected_prefix),
        "connect error text: {text}"
    );
    let log_detail = text
        .strip_prefix(&expected_prefix)
        .expect("daemon log detail");
    let log_name = log_detail.rsplit('/').next().expect("daemon log file name");
    let hash_end = log_name
        .strip_prefix(&format!(
            "{}.",
            missing.file_name().unwrap().to_string_lossy()
        ))
        .and_then(|rest| rest.strip_suffix(".log."))
        .expect("daemon log basename and tail");
    assert!(
        log_detail.starts_with(agent_dir.to_string_lossy().as_ref()),
        "log under agent dir: {text}"
    );
    assert_eq!(text, format!("{expected_prefix}{log_detail}"));
    assert_eq!(hash_end.len(), 8, "8-hex log suffix: {text}");
    assert!(
        hash_end.chars().all(|c| c.is_ascii_hexdigit()),
        "hex log suffix: {text}"
    );
}

// Schedule CLI usage surface: no daemon, validation fires first

/// The schedule commands' usage errors fire in the router's validation, before
/// any daemon request (`--daemon-socket` before the command rotates into the
/// operands; the TS CLI rejects the same shape).
#[test]
fn schedule_usage_errors_and_valid_invocations() {
    let dir = fixture_dir();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let rotated_socket = dir.path().join("bogus.sock");
    let rotated_socket = rotated_socket.to_str().expect("socket path");

    let usage_shapes = [
        (
            vec!["schedule", "cancel"],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
        (
            vec!["schedule", "cancel", "a", "b"],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
        (
            vec!["schedule", "list", "--bogus-flag"],
            "Error: Usage: prime-agent schedule list [--all] [agent] [--json]\n",
        ),
        (
            vec!["--daemon-socket", rotated_socket, "schedule", "list"],
            "Error: Usage: prime-agent schedule list [--all] [agent] [--json]\n",
        ),
        (
            vec![
                "--daemon-socket",
                rotated_socket,
                "schedule",
                "cancel",
                "id",
            ],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
    ];
    for (args, golden) in usage_shapes {
        let out = run_cli(&cli, dir.path(), &agent_dir, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&out), "", "{args:?}");
        assert_eq!(stderr(&out), golden, "{args:?}");
    }

    // Valid shapes pass validation and fail only at the connection attempt.
    let valid_shapes = [
        vec!["schedule", "list"],
        vec!["schedule", "list", "--all"],
        vec!["schedule", "list", "-a"],
        vec!["schedule", "list", "--json"],
        vec!["schedule", "list", "some-agent"],
        vec!["schedule", "cancel", "fake-id"],
        vec!["schedule", "cancel", "--json", "fake-id"],
    ];
    for args in valid_shapes {
        let out = run_cli(&cli, dir.path(), &agent_dir, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = stderr(&out);
        assert!(
            err.starts_with("Error: Failed to connect to the Prime Agent daemon:"),
            "{args:?}: {err}"
        );
    }
}
