//! Sessions hosted inside the daemon process: what replaces the process
//! boundary a session worker used to have.
//!
//! - **Containment**: a panic in one session's turn fails that turn with the
//!   panic's own reason and leaves the other session's running turn, and the
//!   control plane, untouched. No worker process exists at any point.
//! - **Visibility**: the crash reaches the attached client as an event, the
//!   session listing as `lastCrash`, and the daemon's durable crash log.
//! - **Self-heal**: the crashed session is rebuilt from its transcript and
//!   serves the next prompt; a session that keeps crashing is given up on
//!   after the restart policy's cap, and its clients are told.
//! - **Stuck work**: a kernel cell whose host call ignores its interrupt is
//!   reported and its session rebuilt instead of wedging.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let log_file = std::fs::File::create(socket.with_extension("daemon.log")).expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        // Session sockets land under TMPDIR: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .env_remove("PRIME_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    let daemon = Daemon {
        child: command.spawn().expect("spawn pa-daemon supervisor"),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

/// JSONL supervisor client. Every read is bounded: a frame that never comes
/// fails the test instead of hanging it.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Frames read past while waiting for another: a response and the
    /// events around it arrive in no fixed order.
    passed: Vec<Value>,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("connect");
        writer
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("read timeout");
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        let mut client = Self {
            reader,
            writer,
            passed: Vec::new(),
        };
        let hello = client.read_until("the daemon hello", |frame| frame["type"] == "daemon_hello");
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn send(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": {"name": "prime-agent.daemon", "version": 7},
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
    }

    /// The first frame `wanted` accepts, among those already read past or
    /// the ones still to come.
    fn read_until(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.passed.iter().position(&wanted) {
            return self.passed.remove(index);
        }
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("the daemon closed the socket while waiting for {what}"),
                Ok(_) => {}
                Err(error) => panic!(
                    "waiting for {what}: {error}; the frames read past: {:#?}",
                    self.passed
                ),
            }
            let frame: Value = serde_json::from_str(line.trim()).expect("frame json");
            if wanted(&frame) {
                return frame;
            }
            self.passed.push(frame);
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send(id, command);
        self.read_until(id, |frame| frame["id"] == id)
    }

    /// The next session event of `event_type` for `session`.
    fn event(&mut self, session: &str, event_type: &str) -> Value {
        self.read_until(event_type, |frame| {
            frame["type"] == "session_event"
                && frame["activeSessionId"] == session
                && frame["event"]["type"] == event_type
        })["event"]
            .clone()
    }

    /// Create a session from `script` (with `extra` create fields) and attach
    /// this connection to its events.
    fn create_attached(&mut self, dir: &Path, name: &str, script: &Value, extra: Value) -> String {
        let script_path = dir.join(format!("{name}-script.json"));
        std::fs::write(&script_path, script.to_string()).expect("write script");
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let mut create = json!({
            "type": "create",
            "name": name,
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        });
        if let Value::Object(fields) = extra {
            for (key, value) in fields {
                create[key] = value;
            }
        }
        let created = self.request(&format!("create-{name}"), &create);
        assert_eq!(created["success"], true, "create {name} failed: {created}");
        let session = created["data"]["activeSessionId"]
            .as_str()
            .or_else(|| created["data"]["id"].as_str())
            .expect("session id")
            .to_string();
        let attached = self.request(
            &format!("attach-{name}"),
            &json!({ "type": "attach", "activeSessionId": session }),
        );
        assert_eq!(
            attached["success"], true,
            "attach {name} failed: {attached}"
        );
        session
    }
}

fn prompt_and_wait(session: &str) -> Value {
    json!({ "type": "prompt_and_wait", "activeSessionId": session, "message": "run the turn" })
}

/// The prompt the fixtures' `panicOn` seam panics on.
fn crashing_prompt(session: &str) -> Value {
    json!({ "type": "prompt_and_wait", "activeSessionId": session, "message": "crash now" })
}

/// A plain scripted session whose turn panics on [`crashing_prompt`].
fn crashable_script(panic: &str) -> Value {
    json!({
        "responses": [ { "text": "answered" } ],
        "panicOn": { "message": "crash now", "panic": panic },
    })
}

/// The crash log's records for one session (the daemon appends one JSON
/// object per crash).
fn crash_log(agent_dir: &Path, socket: &Path) -> Vec<Value> {
    let path =
        pa_daemon::descriptor::descriptor_dir(agent_dir, socket).join("session-crashes.jsonl");
    common::read_transcript(&path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("crash record json"))
        .collect()
}

/// A cell that enters a native call no interrupt reaches (the kernel's test
/// seam, which the session's launch environment asks for): the call writes
/// its marker under `dir` once it is inside, and never returns.
fn wedged_cell(dir: &Path) -> String {
    format!(
        "(%test-block {})",
        serde_json::to_string(&dir.join("cell-blocked").to_string_lossy())
            .expect("a Scheme string literal")
    )
}

/// Wait until the [`wedged_cell`] under `dir` is inside its native call: from
/// then on a stop cannot reach it, however late it is sent.
fn await_wedged(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !dir.join("cell-blocked").exists() {
        assert!(
            Instant::now() < deadline,
            "the cell never reached its native call"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The session leases held under `agent_dir` (one `.lock` directory each).
fn held_leases(agent_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(agent_dir.join("session-leases")).map_or_default(|entries| {
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "lock")
            })
            .collect()
    })
}

/// The processes whose parent is `pid` (Linux: the kernel's own list).
#[cfg(target_os = "linux")]
fn child_processes(pid: u32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .expect("daemon tasks")
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn a_panic_in_one_session_fails_its_turn_is_reported_and_heals_while_another_runs() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // A's turn code panics on the crashing prompt. B's one turn is slow
    // enough to be running across A's crash and A's rebuild.
    let session_a = client.create_attached(
        dir.path(),
        "a",
        &crashable_script("injected fault"),
        json!({}),
    );
    let session_b = client.create_attached(
        dir.path(),
        "b",
        &json!({ "responses": [ { "text": "b done", "delayMs": 3000 } ] }),
        json!({}),
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        child_processes(daemon.child.id()),
        Vec::<String>::new(),
        "two live sessions and the daemon has no child process"
    );
    let first = client.request("a-first", &prompt_and_wait(&session_a));
    assert_eq!(first["success"], true, "A's first turn failed: {first}");

    let b_turn = std::thread::spawn({
        let socket: PathBuf = socket.clone();
        let session_b = session_b.clone();
        move || Client::connect(&socket).request("b-turn", &prompt_and_wait(&session_b))
    });
    // B's turn is running before A is made to crash.
    client.event(&session_b, "agent_start");

    client.send("a-crash", &crashing_prompt(&session_a));
    let crashed = client.event(&session_a, "session_crashed");
    let failed_turn = client.read_until("a-crash", |frame| frame["id"] == "a-crash");
    let recovered = client.event(&session_a, "session_recovered");
    let b_result = b_turn.join().expect("B's turn thread");
    // The rebuilt session serves the next prompt from its transcript: the
    // first exchange is still there.
    let healed = client.request("a-healed", &prompt_and_wait(&session_a));
    let listed = client.request("list", &json!({ "type": "list" }));
    let row_a = listed["data"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["id"] == session_a.as_str())
        .expect("A's row")
        .clone();
    let log = crash_log(&agent_dir, &socket);

    let location = crashed["location"].as_str().expect("panic location");
    assert!(
        location.contains("scripted.rs:"),
        "the crash names where the panic was raised: {location}"
    );
    assert_eq!(
        (
            crashed["kind"].clone(),
            crashed["message"].clone(),
            failed_turn["success"].clone(),
            failed_turn["error"].clone(),
            recovered["failures"].clone(),
            b_result["success"].clone(),
            healed["success"].clone(),
            row_a["lastCrash"]["message"].clone(),
            log.len(),
            log[0]["workerId"].clone(),
            log[0]["location"].clone(),
        ),
        (
            json!("panic"),
            json!("injected fault"),
            json!(false),
            json!(format!("Session crashed (panic): injected fault at {location}")),
            json!(1),
            json!(true),
            json!(true),
            json!("injected fault"),
            1,
            json!(session_a),
            json!(location),
        ),
        "crash: {crashed}\nfailed turn: {failed_turn}\nB: {b_result}\nhealed: {healed}\nrow: {row_a}"
    );
}

#[test]
fn a_session_that_keeps_crashing_is_given_up_on_and_its_client_is_told() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // Each crash comes inside the restart policy's stable-lifetime window.
    let session =
        client.create_attached(dir.path(), "doomed", &crashable_script("always"), json!({}));
    let mut recoveries = Vec::new();
    let failed = loop {
        client.send("crash", &crashing_prompt(&session));
        client.event(&session, "session_crashed");
        let next = client.read_until("the restart or the give-up", |frame| {
            frame["type"] == "session_event"
                && frame["activeSessionId"] == session.as_str()
                && (frame["event"]["type"] == "session_recovered"
                    || frame["event"]["type"] == "session_failed")
        })["event"]
            .clone();
        if next["type"] == "session_failed" {
            break next;
        }
        recoveries.push(next["failures"].clone());
    };
    // The give-up leaves nothing holding the session: its lease is gone, so
    // the session file can be opened again.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !held_leases(&agent_dir).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the given-up session still holds its lease: {:?}",
            held_leases(&agent_dir)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        (
            recoveries,
            failed["failures"].clone(),
            crash_log(&agent_dir, &socket).len(),
        ),
        (
            vec![json!(1), json!(2), json!(3), json!(4), json!(5)],
            json!(6),
            6,
        ),
        "five restarts with backoff, then the give-up"
    );
}

#[test]
fn a_cell_that_ignores_its_interrupt_is_reported_as_stuck_and_its_session_rebuilt() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The cell sits in the test seam's native call, which no interrupt
    // reaches (no kernel function of its own ignores a stop any more).
    let session = client.create_attached(
        dir.path(),
        "stuck",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": wedged_cell(dir.path()),
                    } },
                ] },
                { "text": "done" },
            ],
        }),
        // The session's own grace and the seam, from its own environment.
        json!({ "launchEnv": {
            "PRIME_AGENT_STUCK_CELL_GRACE_MS": "300",
            "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1",
        } }),
    );
    let prompted = client.request(
        "prompt",
        &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    await_wedged(dir.path());
    client.send(
        "abort",
        &json!({ "type": "abort", "activeSessionId": session }),
    );
    let crashed = client.event(&session, "session_crashed");
    let recovered = client.event(&session, "session_recovered");
    let state = client.request(
        "state",
        &json!({ "type": "get_state", "activeSessionId": session }),
    );
    assert_eq!(
        (
            crashed["kind"].clone(),
            crashed["message"].clone(),
            recovered["failures"].clone(),
            state["success"].clone(),
        ),
        (
            json!("stuck"),
            json!("a kernel cell did not stop within 300 ms of its interrupt: a host call is not observing the cell's cancellation"),
            json!(1),
            json!(true),
        ),
        "crash: {crashed}\nstate: {state}"
    );
}

#[test]
fn a_session_mid_turn_survives_the_daemon_being_killed_and_the_death_is_reported() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut first = spawn_supervisor(&socket, &agent_dir);
    let session = {
        let mut client = Client::connect(&socket);
        let session = client.create_attached(
            dir.path(),
            "survivor",
            &json!({ "responses": [ { "text": "slow answer", "delayMs": 1500 } ] }),
            json!({}),
        );
        let prompted = client.request(
            "prompt",
            &json!({ "type": "prompt", "activeSessionId": session, "message": "run the turn" }),
        );
        assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
        client.event(&session, "agent_start");
        session
    };
    // The whole daemon dies mid-turn, every session with it: nothing in it
    // gets to settle anything.
    first.child.kill().expect("kill the daemon");
    first.child.wait().expect("reap the daemon");
    let _ = std::fs::remove_file(&socket);

    // The daemon's owner starts it again on the same socket and agent dir.
    let _second = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The boot adoption revives the session; attaching waits it out.
    let deadline = Instant::now() + Duration::from_secs(30);
    let row = loop {
        let listed = client.request("list", &json!({ "type": "list" }));
        let row = listed["data"]["sessions"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["id"] == session.as_str())
                .cloned()
        });
        if let Some(row) = row.filter(|row| row["lifecycle"] == "live") {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the session never came back: {listed}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // The interrupted turn runs again in the rebuilt session, to its end.
    let finished = client.request("after", &prompt_and_wait(&session));
    let log = crash_log(&agent_dir, &socket);
    assert_eq!(
        (
            row["lastCrash"]["kind"].clone(),
            row["lastCrash"]["message"].clone(),
            log.len(),
            log[0]["workerId"].clone(),
            finished["success"].clone(),
        ),
        (
            json!("daemon_restart"),
            json!("the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn was started again"),
            1,
            json!(session),
            json!(true),
        ),
        "row: {row}\nfinished: {finished}"
    );
}

#[test]
fn a_wedged_session_is_stopped_on_request_and_its_lease_frees() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The cell sits in a native call for the whole test, and the stuck-cell
    // watch keeps its default grace: nothing but the stop ends this session.
    let session = client.create_attached(
        dir.path(),
        "wedged",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": wedged_cell(dir.path()),
                    } },
                ] },
                { "text": "done" },
            ],
        }),
        json!({ "launchEnv": { "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1" } }),
    );
    let prompted = client.request(
        "prompt",
        &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    await_wedged(dir.path());

    let killed = client.request(
        "kill",
        &json!({ "type": "kill", "activeSessionId": session }),
    );
    // The stop has retired the session when its record is gone; the daemon
    // still serves, and no session of this process holds a lease.
    let descriptor =
        pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket).join(format!("{session}.json"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while descriptor.exists() {
        assert!(
            Instant::now() < deadline,
            "the wedged session was never retired"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let listed = client.request("list", &json!({ "type": "list" }));
    let held_leases = held_leases(&agent_dir);
    assert_eq!(
        (
            killed["success"].clone(),
            listed["data"]["sessions"].clone(),
            held_leases,
        ),
        (json!(true), json!([]), Vec::<PathBuf>::new()),
        "kill: {killed}"
    );
}

/// Whether any process's command line names `marker` (Linux: /proc).
#[cfg(target_os = "linux")]
fn process_named(marker: &str) -> bool {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .any(|cmdline| String::from_utf8_lossy(&cmdline).contains(marker))
}

/// Sessions run inside the daemon, so the commands their cells start are
/// the daemon's own children: a daemon shutdown must not leave one behind.
#[cfg(target_os = "linux")]
#[test]
fn a_command_a_cell_started_does_not_survive_the_daemons_shutdown() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The command is a long sleep under a name only this test uses. Naming a
    // process is `exec -a`, an option of the program `bash`: the built-in
    // shell has no process of its own to rename, so the command starts that
    // program.
    let marker = format!("pa-shutdown-{}", uuid::Uuid::new_v4().simple());
    let session = client.create_attached(
        dir.path(),
        "runner",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": format!("(bash \"bash -c 'exec -a {marker} sleep 600'\")"),
                    } },
                ] },
                { "text": "done" },
            ],
        }),
        json!({}),
    );
    let prompted = client.request(
        "prompt",
        &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !process_named(&marker) {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    let shutdown = client.request("shutdown", &json!({ "type": "shutdown" }));
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while daemon.child.try_wait().expect("try wait").is_none() || process_named(&marker) {
        assert!(
            Instant::now() < deadline,
            "after the shutdown: daemon exited = {}, the command survives = {}",
            daemon.child.try_wait().expect("try wait").is_some(),
            process_named(&marker)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A session on the real engine whose scripted model defines `marker` in a
/// kernel cell. The script starts over in every rebuilt lifetime, so its
/// first cell is the probe: it fails exactly when `marker` is not defined.
/// Turn one probes then defines (three responses), turn two probes (two),
/// turn three answers in text, and the last cell never returns.
fn kernel_story_script(dir: &Path, extra: &Value) -> Value {
    let cell = |code: &str| {
        json!({ "content": [
            { "type": "toolCall", "name": "eval", "arguments": { "code": code } },
        ] })
    };
    let mut script = json!({
        "engine": "faux",
        "responses": [
            cell("(displayln marker)"),
            cell("(define marker 41)"),
            { "text": "defined" },
            cell("(displayln marker)"),
            { "text": "probed" },
            { "text": "idle", "delayMs": 1500 },
            cell(&wedged_cell(dir)),
            { "text": "done" },
        ],
    });
    if let Value::Object(fields) = extra {
        for (key, value) in fields {
            script[key] = value.clone();
        }
    }
    script
}

/// The entries of the one session file under `dir`, in order.
fn session_entries(dir: &Path) -> Vec<Value> {
    let file = common::transcripts(&dir.join("sessions"))
        .list(&dir.join("sessions"))
        .expect("sessions dir")
        .transcripts
        .into_iter()
        .map(|transcript| transcript.locator)
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("the session file");
    common::read_transcript(&file)
        .expect("read the session file")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("entry json"))
        .collect()
}

/// What a session's transcript under `dir` tells of its kernel, in order:
/// each user prompt, each kernel cell's outcome, each kernel-reset notice and
/// each crash or recovery row.
fn kernel_story(dir: &Path) -> Vec<String> {
    session_entries(dir)
        .into_iter()
        .filter_map(|entry| {
            let message = &entry["message"];
            match (entry["type"].as_str(), message["role"].as_str()) {
                (Some("message"), Some("user")) => Some("user".to_string()),
                (Some("message"), Some("toolResult")) if message["toolName"] == "eval" => {
                    let text = message["content"][0]["text"].as_str().unwrap_or_default();
                    Some(format!("cell: {}", text.lines().next().unwrap_or_default()))
                }
                (Some("custom_message"), _) => match entry["customType"].as_str() {
                    Some("ipython_state_restored") => Some(format!(
                        "kernel reset notice (restored: {})",
                        entry["details"]["restored"]
                    )),
                    Some("turn_interrupted") => Some("interrupted notice".to_string()),
                    Some("session_fault") => Some(match entry["details"]["type"].as_str() {
                        Some("session_crashed") => format!(
                            "crashed row ({})",
                            entry["details"]["kind"].as_str().unwrap_or_default()
                        ),
                        Some("session_recovered") => "recovered row".to_string(),
                        _ => format!("fault row: {}", entry["details"]),
                    }),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect()
}

/// The probe cell's result when `marker` is not defined in its engine.
const GONE: &str =
    "cell: Error: FreeIdentifier: Cannot reference an identifier before its definition: marker";
/// The defining cell's result (it prints nothing).
const DEFINED: &str = "cell: ";
/// The probe cell's result when `marker` is defined.
const FOUND: &str = "cell: 41";

fn turn(session: &str, message: &str) -> Value {
    json!({ "type": "prompt_and_wait", "activeSessionId": session, "message": message })
}

fn story(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| (*row).to_string()).collect()
}

#[test]
fn a_session_rebuilt_after_a_panic_is_told_once_that_its_kernel_definitions_are_gone() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "kernel",
        &kernel_story_script(
            dir.path(),
            &json!({
                "panicOn": { "message": "crash now", "panic": "injected fault" },
            }),
        ),
        json!({}),
    );
    for (id, message) in [
        ("define", "define it"),
        ("probe", "probe it"),
        ("idle", "idle"),
    ] {
        let done = client.request(id, &turn(&session, message));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    client.send("crash", &crashing_prompt(&session));
    client.event(&session, "session_recovered");
    // The rebuilt lifetime's script starts over: its probe fails, because
    // the definition went with the old engine.
    for (id, message) in [("again", "define it again"), ("probe-again", "probe it")] {
        let done = client.request(id, &turn(&session, message));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    assert_eq!(
        kernel_story(dir.path()),
        story(&[
            "user",
            GONE,
            DEFINED,
            "user",
            FOUND,
            "user",
            "crashed row (panic)",
            "recovered row",
            "user",
            "kernel reset notice (restored: false)",
            GONE,
            DEFINED,
            "user",
            FOUND,
        ])
    );
}

#[test]
fn a_session_that_never_ran_a_cell_is_told_nothing_after_its_rebuild() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "plain",
        &json!({
            "engine": "faux",
            "responses": [ { "text": "one" } ],
            "repeatLastResponse": true,
            "panicOn": { "message": "crash now", "panic": "injected fault" },
        }),
        json!({}),
    );
    let done = client.request("first", &turn(&session, "talk"));
    assert_eq!(done["success"], true, "the first turn failed: {done}");
    client.send("crash", &crashing_prompt(&session));
    client.event(&session, "session_recovered");
    for id in ["second", "third"] {
        let done = client.request(id, &turn(&session, "talk"));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    assert_eq!(
        kernel_story(dir.path()),
        story(&[
            "user",
            "crashed row (panic)",
            "recovered row",
            "user",
            "user",
        ])
    );
}

#[test]
fn a_session_rebuilt_after_a_stuck_cell_is_told_once_that_its_kernel_definitions_are_gone() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "kernel",
        &kernel_story_script(dir.path(), &json!({})),
        json!({ "launchEnv": {
            "PRIME_AGENT_STUCK_CELL_GRACE_MS": "300",
            "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1",
        } }),
    );
    for (id, message) in [
        ("define", "define it"),
        ("probe", "probe it"),
        ("idle", "idle"),
    ] {
        let done = client.request(id, &turn(&session, message));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    let prompted = client.request(
        "stuck",
        &json!({ "type": "prompt", "activeSessionId": session, "message": "run the stuck cell" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    await_wedged(dir.path());
    client.send(
        "abort",
        &json!({ "type": "abort", "activeSessionId": session }),
    );
    client.event(&session, "session_recovered");
    // The abort's suspension outlives the rebuild; a prompt that names its
    // delivery resumes it.
    for (id, message) in [("again", "define it again"), ("probe-again", "probe it")] {
        let mut prompt = turn(&session, message);
        prompt["streamingBehavior"] = json!("followUp");
        let done = client.request(id, &prompt);
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    assert_eq!(
        kernel_story(dir.path()),
        story(&[
            "user",
            GONE,
            DEFINED,
            "user",
            FOUND,
            "user",
            "user",
            "cell: Request was aborted",
            "crashed row (stuck)",
            "recovered row",
            "user",
            "kernel reset notice (restored: false)",
            GONE,
            DEFINED,
            "user",
            FOUND,
        ])
    );
}

#[test]
fn a_parked_child_whose_idle_kernel_was_released_is_told_once_on_its_next_turn() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // A parent-owned child with no attached client releases its kernel each
    // time it parks. The script never restarts here: the lifetime goes on.
    let script_path = dir.path().join("child-script.json");
    std::fs::write(
        &script_path,
        kernel_story_script(dir.path(), &json!({})).to_string(),
    )
    .expect("script");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let created = client.request(
        "create",
        &json!({
            "type": "create",
            "name": "child",
            "config": {
                "rlmDepth": 1,
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let session = created["data"]["activeSessionId"]
        .as_str()
        .or_else(|| created["data"]["id"].as_str())
        .expect("session id")
        .to_string();
    // Turn one defines `marker` and parks: the kernel is released. Turn two's
    // probe runs on a new engine and fails; turn three answers in text.
    for (id, message) in [
        ("define", "define it"),
        ("probe", "probe it"),
        ("idle", "idle"),
    ] {
        let done = client.request(id, &turn(&session, message));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    assert_eq!(
        kernel_story(dir.path()),
        story(&[
            "user",
            GONE,
            DEFINED,
            "user",
            "kernel reset notice (restored: false)",
            GONE,
            "user",
        ])
    );
}

#[test]
fn a_turn_cut_off_by_a_daemon_restart_continues_behind_one_notice_and_its_prompt_is_not_repeated() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut first = spawn_supervisor(&socket, &agent_dir);
    let session = {
        let mut client = Client::connect(&socket);
        let session = client.create_attached(
            dir.path(),
            "kernel",
            &kernel_story_script(dir.path(), &json!({})),
            json!({}),
        );
        for (id, message) in [("define", "define it"), ("probe", "probe it")] {
            let done = client.request(id, &turn(&session, message));
            assert_eq!(done["success"], true, "{id} failed: {done}");
        }
        // The third turn's answer is held back: its prompt is saved, and
        // the daemon dies before the model has said anything.
        let prompted = client.request(
            "prompt",
            &json!({ "type": "prompt", "activeSessionId": session, "message": "idle" }),
        );
        assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
        client.read_until("the third turn's start", |frame| {
            frame["activeSessionId"] == session.as_str()
                && frame["event"]["type"] == "message_end"
                && frame["event"]["message"]["content"][0]["text"] == "idle"
        });
        session
    };
    first.child.kill().expect("kill the daemon");
    first.child.wait().expect("reap the daemon");
    let _ = std::fs::remove_file(&socket);

    let _second = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The revived session continues the cut-off turn (its script starts
    // over, so that turn probes and defines); the probe turn queues behind.
    let probed = client.request("probe-again", &turn(&session, "probe it"));
    assert_eq!(probed["success"], true, "the probe failed: {probed}");
    assert_eq!(
        kernel_story(dir.path()),
        story(&[
            "user",
            GONE,
            DEFINED,
            "user",
            FOUND,
            "user",
            "crashed row (daemon_restart)",
            "recovered row",
            "interrupted notice",
            "kernel reset notice (restored: false)",
            GONE,
            DEFINED,
            "user",
            FOUND,
        ])
    );
}

/// A cell the daemon dies under is never run again: the revived turn's
/// context answers it as of unknown outcome, with what its code can have
/// done, and the model decides. The revived lifetime's script answers in
/// text, so a second run of the cell could only be the daemon's.
#[test]
fn a_cell_cut_off_by_the_daemons_death_is_answered_as_outcome_unknown_and_not_run_again() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut first = spawn_supervisor(&socket, &agent_dir);
    let session = {
        let mut client = Client::connect(&socket);
        let session = client.create_attached(
            dir.path(),
            "cut-off",
            &json!({
                "engine": "faux",
                "responses": [
                    { "content": [
                        { "type": "toolCall", "name": "eval", "arguments": {
                            "code": format!(
                                "(bash \"echo ran >> counter.txt\") {}",
                                wedged_cell(dir.path())
                            ),
                        } },
                    ] },
                    { "text": "done" },
                ],
            }),
            json!({ "launchEnv": { "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1" } }),
        );
        let prompted = client.request(
            "prompt",
            &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
        );
        assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
        await_wedged(dir.path());
        session
    };
    first.child.kill().expect("kill the daemon");
    first.child.wait().expect("reap the daemon");
    let _ = std::fs::remove_file(&socket);
    std::fs::write(
        dir.path().join("cut-off-script.json"),
        json!({ "engine": "faux", "responses": [ { "text": "continued" }, { "text": "after" } ] })
            .to_string(),
    )
    .expect("rewrite the script");

    let _second = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let after = client.request("after", &turn(&session, "after"));
    assert_eq!(after["success"], true, "the next turn failed: {after}");
    let session_file = common::transcripts(&dir.path().join("sessions"))
        .list(&dir.path().join("sessions"))
        .expect("sessions dir")
        .transcripts
        .into_iter()
        .map(|transcript| transcript.locator)
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("the session file");
    let results: Vec<Value> = common::read_transcript(&session_file)
        .expect("read the session file")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("entry json"))
        .filter(|entry| entry["message"]["role"] == "toolResult")
        .map(|entry| {
            json!([
                entry["message"]["content"][0]["text"],
                entry["message"]["isError"],
                entry["message"]["details"]
            ])
        })
        .collect();
    assert_eq!(
        (
            std::fs::read_to_string(dir.path().join("counter.txt")).expect("the counter"),
            kernel_story(dir.path()),
            results,
        ),
        (
            "ran\n".to_string(),
            story(&[
                "user",
                "crashed row (daemon_restart)",
                "recovered row",
                "cell: [outcome-unknown] The daemon hosting this session stopped while this `eval` call ran, so its outcome is unknown: it may have run in part, in full or not at all, and its result was lost. It was not run again: check its effects before you run it again.",
                "interrupted notice",
                "kernel reset notice (restored: false)",
                "user",
            ]),
            vec![json!([
                "[outcome-unknown] The daemon hosting this session stopped while this `eval` call ran, so its outcome is unknown: it may have run in part, in full or not at all, and its result was lost. It was not run again: check its effects before you run it again.\n\nWhat is known:\n\
                 - it runs `bash`: a command it started may have done part of its work; commands still running were stopped with the session, except in a persistent shell",
                true,
                { "outcome": "unknown" },
            ])],
        )
    );
}

/// The command a cut-off call was waiting for is that call's own: its
/// `[outcome-unknown]` result reports it, so the next kernel does not
/// announce it as lost too (nor runs a turn for it). A background command
/// the call started is announced: the result names no command.
#[test]
fn the_command_a_cut_off_call_waited_for_is_not_announced_again_as_lost() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut first = spawn_supervisor(&socket, &agent_dir);
    let counter = dir.path().join("counter.txt");
    let session = {
        let mut client = Client::connect(&socket);
        let session = client.create_attached(
            dir.path(),
            "lost",
            &json!({
                "engine": "faux",
                "responses": [
                    { "content": [
                        { "type": "toolCall", "name": "eval", "arguments": {
                            "code": "(bash \"/bin/sleep 600\" (hash 'background #t)) \
                                     (bash \"echo hit >> counter.txt && /bin/sleep 600\" (hash 'wait-ms #f))",
                        } },
                    ] },
                    { "text": "done" },
                ],
            }),
            json!({}),
        );
        let prompted = client.request(
            "prompt",
            &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
        );
        assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::fs::read_to_string(&counter).unwrap_or_default() != "hit\n" {
            assert!(Instant::now() < deadline, "the command never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        session
    };
    first.child.kill().expect("kill the daemon");
    first.child.wait().expect("reap the daemon");
    let _ = std::fs::remove_file(&socket);
    std::fs::write(
        dir.path().join("lost-script.json"),
        json!({ "engine": "faux", "responses": [
            { "text": "continued" }, { "text": "noted" }, { "text": "after" },
        ] })
        .to_string(),
    )
    .expect("rewrite the script");

    let _second = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // The next kernel announces its predecessor's commands one after the
    // other: once the background command's notice is saved, a notice for the
    // waited one would already be queued ahead of the next prompt.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !session_entries(dir.path())
        .iter()
        .any(|entry| entry["details"]["command"] == "/bin/sleep 600")
    {
        assert!(
            Instant::now() < deadline,
            "the background command was never announced"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = client.request("after", &turn(&session, "after"));
    assert_eq!(after["success"], true, "the next turn failed: {after}");
    let entries = session_entries(dir.path());
    let lost: Vec<Value> = entries
        .iter()
        .filter(|entry| entry["details"]["lost"] == true)
        .map(|entry| entry["details"]["command"].clone())
        .collect();
    let results: Vec<Value> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "toolResult")
        .map(|entry| entry["message"]["details"].clone())
        .collect();
    let replies: Vec<Value> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "assistant")
        .map(|entry| entry["message"]["content"][0]["text"].clone())
        .collect();
    assert_eq!(
        (
            std::fs::read_to_string(&counter).expect("the counter"),
            lost,
            results,
            replies,
        ),
        (
            "hit\n".to_string(),
            vec![json!("/bin/sleep 600")],
            vec![json!({ "outcome": "unknown" })],
            vec![
                Value::Null,
                json!("continued"),
                json!("noted"),
                json!("after")
            ],
        )
    );
}

#[test]
fn a_restarted_daemon_still_marks_the_session_that_crashed_under_its_predecessor() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let mut first = spawn_supervisor(&socket, &agent_dir);
    let session = {
        let mut client = Client::connect(&socket);
        let session = client.create_attached(
            dir.path(),
            "marked",
            &crashable_script("injected fault"),
            json!({}),
        );
        client.send("crash", &crashing_prompt(&session));
        client.event(&session, "session_recovered");
        let served = client.request("after", &prompt_and_wait(&session));
        assert_eq!(served["success"], true, "the rebuilt session: {served}");
        session
    };
    // The session is idle when the daemon goes (its journal's last word says
    // so): the next daemon does not revive it, and its fault mark has only
    // the durable record to come from.
    let journal = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
        .join(format!("{session}.recovery.jsonl"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let records = std::fs::read_to_string(&journal).unwrap_or_default();
        let last_verdict = records
            .lines()
            .rev()
            .find(|line| line.contains("\"busy\":"));
        if last_verdict.is_some_and(|line| line.contains("\"busy\":false")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the rebuilt session never settled: {records}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    first.child.kill().expect("kill the daemon");
    first.child.wait().expect("reap the daemon");
    let _ = std::fs::remove_file(&socket);
    let _second = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    // Reopening the saved session makes its roster row anew.
    let session_file = common::transcripts(&dir.path().join("sessions"))
        .list(&dir.path().join("sessions"))
        .expect("sessions dir")
        .transcripts
        .into_iter()
        .map(|transcript| transcript.locator)
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("the session file");
    let reopened = client.request(
        "reopen",
        &json!({
            "type": "create",
            "sessionPath": session_file.to_string_lossy(),
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": dir.path().join("sessions").to_string_lossy(),
                "script": dir.path().join("marked-script.json").to_string_lossy(),
            },
        }),
    );
    assert_eq!(reopened["success"], true, "the reopen failed: {reopened}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let fault = loop {
        let roster = client.request("roster", &json!({ "type": "roster_subscribe" }));
        let fault = roster["data"]["roster"].as_array().and_then(|entries| {
            entries
                .iter()
                .map(|entry| &entry["summary"]["lastCrash"])
                .find(|fault| fault.is_object())
                .cloned()
        });
        if let Some(fault) = fault {
            break fault;
        }
        assert!(
            Instant::now() < deadline,
            "the roster never marked the session: {roster}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        json!([
            fault["workerId"],
            fault["kind"],
            fault["message"],
            fault["state"]
        ]),
        json!([session, "panic", "injected fault", "recovered"])
    );
}

#[test]
fn a_session_rebuilt_under_an_abort_stays_stopped_and_says_why() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "stopped",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": wedged_cell(dir.path()),
                    } },
                ] },
                { "text": "done" },
            ],
        }),
        json!({ "launchEnv": {
            "PRIME_AGENT_STUCK_CELL_GRACE_MS": "300",
            "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1",
        } }),
    );
    let prompted = client.request(
        "prompt",
        &json!({ "type": "prompt", "activeSessionId": session, "message": "run the cell" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    await_wedged(dir.path());
    client.send(
        "abort",
        &json!({ "type": "abort", "activeSessionId": session }),
    );
    let recovered = client.event(&session, "session_recovered");
    // The user's abort outlives the rebuild: a plain prompt is refused with
    // the reason, and a prompt that names its delivery resumes the session.
    let plain = client.request("plain", &turn(&session, "go on"));
    let mut named = turn(&session, "go on");
    named["streamingBehavior"] = json!("followUp");
    let resumed = client.request("named", &named);
    assert_eq!(
        json!([
            recovered["queuedInputSuspended"],
            plain["success"],
            plain["error"],
            resumed["success"],
        ]),
        json!([
            true,
            false,
            "Cannot admit a session action while queued session input is suspended. The session was rebuilt after it was stopped, and the stop still holds: resume the queue, or send the prompt with a streamingBehavior.",
            true,
        ]),
        "recovered: {recovered}\nplain: {plain}\nresumed: {resumed}"
    );
}

#[test]
fn a_sessions_panic_is_written_to_that_sessions_log_and_not_to_the_daemons() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "logged",
        &crashable_script("injected fault"),
        json!({}),
    );
    client.send("crash", &crashing_prompt(&session));
    client.event(&session, "session_recovered");
    let session_log = std::fs::read_to_string(
        agent_dir
            .join("logs")
            .join(format!("worker-{session}.stderr.log")),
    )
    .unwrap_or_default();
    let daemon_log =
        std::fs::read_to_string(socket.with_extension("daemon.log")).unwrap_or_default();
    let panic_line = session_log
        .lines()
        .find(|line| line.contains("panicked at"))
        .unwrap_or_default();
    let tag = format!("[session {session}] thread 'session-");
    // The daemon's own log keeps its one line about the crash; the panic's
    // text and place, which the default hook prints as `panicked at`, are in
    // the session's log only.
    assert_eq!(
        (
            panic_line.contains(&tag) && panic_line.contains("scripted.rs:"),
            session_log.contains("\ninjected fault\n"),
            daemon_log.contains("panicked at"),
        ),
        (true, true, false),
        "session log: {session_log}\ndaemon log: {daemon_log}"
    );
}

/// A session's lifetime can end while its kernel's cell waits for a command
/// with a time limit, on the engine's runtime and from a thread of its own.
/// The cell is stopped and has left before that runtime goes: torn down
/// under the wait, the runtime's timer panics the cell's thread, a second
/// fault of a session that had one. The cell here went to the background
/// (it outlived its one-second window inside the wait), so the session is
/// idle when its next turn panics.
#[test]
fn a_cell_waiting_for_a_command_has_left_before_its_crashed_session_is_torn_down() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let started = dir.path().join("command-started");
    let command = format!("echo up > '{}'; /bin/sleep 600", started.to_string_lossy());
    let session = client.create_attached(
        dir.path(),
        "waiting",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": format!(
                            "(bash {} (hash 'timeout-ms 500000))",
                            serde_json::to_string(&command).expect("a Scheme string literal")
                        ),
                        "timeout": 1,
                    } },
                ] },
                { "text": "done" },
            ],
            "panicOn": { "message": "crash now", "panic": "injected fault" },
        }),
        // The wait keeps its command for the whole test instead of handing
        // it over at the session's default window.
        json!({ "launchEnv": { "PRIME_AGENT_BASH_WAIT_MS": "600000" } }),
    );
    let backgrounded = client.request("turn", &prompt_and_wait(&session));
    assert_eq!(backgrounded["success"], true, "turn failed: {backgrounded}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    client.send("crash", &crashing_prompt(&session));
    // The crashed lifetime is torn down before its successor is announced.
    client.event(&session, "session_recovered");
    let session_log = std::fs::read_to_string(
        agent_dir
            .join("logs")
            .join(format!("worker-{session}.stderr.log")),
    )
    .unwrap_or_default();
    // Each panic is logged as its `panicked at` line, then its message.
    let lines: Vec<&str> = session_log.lines().collect();
    let panics: Vec<&str> = lines
        .windows(2)
        .filter(|pair| pair[0].contains("panicked at"))
        .map(|pair| pair[1])
        .collect();
    let crashes: Vec<Value> = crash_log(&agent_dir, &socket)
        .into_iter()
        .map(|crash| crash["message"].clone())
        .collect();
    assert_eq!(
        (panics, crashes),
        (vec!["injected fault"], vec![json!("injected fault")]),
        "session log: {session_log}"
    );
}

/// The rows the session engine adds to a turn itself (a subdirectory's
/// context file after the cell that read below it, a skill whose Scheme
/// source failed to load, a broken per-model prompt map) are what the model
/// was shown: each is saved in the session file once, where a rebuild or a
/// resume reads it, and each reaches the attached client.
#[test]
fn the_rows_the_engine_adds_to_a_turn_are_saved_and_sent_to_the_client() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let nested = dir.path().join("crates").join("foo");
    std::fs::create_dir_all(&nested).expect("nested dir");
    std::fs::write(nested.join("AGENTS.md"), "Run the foo tests.\n").expect("nested AGENTS.md");
    std::fs::write(nested.join("notes.txt"), "notes\n").expect("nested file");
    let skill = agent_dir.join("skills").join("broken");
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: broken\ndescription: A skill whose Scheme source does not parse.\n---\n",
    )
    .expect("SKILL.md");
    std::fs::write(skill.join("skill.scm"), "(define (broken-x").expect("skill.scm");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("model-prompts.toml"), "not = [toml").expect("model prompts");
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let session = client.create_attached(
        dir.path(),
        "rows",
        &json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": "(fs-read \"crates/foo/notes.txt\")",
                    } },
                ] },
                { "text": "read" },
                { "text": "again" },
            ],
        }),
        json!({}),
    );
    // A kernel that starts inside the first turn tells of its skills before
    // the second.
    for (id, message) in [("first", "read the notes"), ("second", "again")] {
        let done = client.request(id, &turn(&session, message));
        assert_eq!(done["success"], true, "{id} failed: {done}");
    }
    let added = [
        "model_prompt_error",
        "nested_context_file",
        "scheme_skills_unavailable",
    ];
    let sent: Vec<Value> = added
        .iter()
        .map(|custom_type| {
            client.read_until(custom_type, |frame| {
                frame["type"] == "session_event"
                    && frame["activeSessionId"] == session.as_str()
                    && frame["event"]["type"] == "message_end"
                    && frame["event"]["message"]["customType"] == *custom_type
            })["event"]["message"]["details"]
                .clone()
        })
        .collect();
    let session_file = common::transcripts(&dir.path().join("sessions"))
        .list(&dir.path().join("sessions"))
        .expect("sessions dir")
        .transcripts
        .into_iter()
        .map(|transcript| transcript.locator)
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("the session file");
    let mut saved: Vec<Value> = common::read_transcript(&session_file)
        .expect("read the session file")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("entry json"))
        .filter(|entry| {
            entry["type"] == "custom_message"
                && added.contains(&entry["customType"].as_str().unwrap_or_default())
        })
        .map(|entry| json!([entry["customType"], entry["details"]]))
        .collect();
    saved.sort_by_key(|row| row[0].as_str().unwrap_or_default().to_string());
    let details = [
        Value::Null,
        json!({ "path": "crates/foo/AGENTS.md", "truncated": false }),
        json!({ "skills": ["broken"] }),
    ];
    assert_eq!(
        (saved, sent),
        (
            added
                .iter()
                .zip(&details)
                .map(|(custom_type, details)| json!([custom_type, details]))
                .collect::<Vec<_>>(),
            details.to_vec(),
        )
    );
}
