//! The client launch-env e2e (contract audit #1 + #9): the environment a
//! client forwards on its `create` reaches ITS session's worker process
//! (and the kernel under it) under the supervisor's internal identity
//! overrides; `RLM_DEPTH` never reaches a worker — not from the daemon's
//! own boot env, not from a client's forward (a daemon or client running
//! inside an agent must not stamp the new root's recursion identity);
//! one session's forwarded env never bleeds into another's worker; the
//! durable artifacts (descriptor, update-restart roster) never carry the
//! client env; and an `rlm.spawn` child inherits the parent worker's env.
// Pedantic-gate dispositions (fleet-uniform ruling; see kernel_dispose_e2e).
#![allow(clippy::large_futures)]
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Remove one env var at scope end: a test that must run with a var in
/// its own process env (the parent-worker stand-in) restores the
/// process to its pre-test state (the supervisor tests'
/// save-and-restore pattern).
#[cfg(target_os = "linux")]
struct RemoveEnvVarAtDrop(&'static str);

#[cfg(target_os = "linux")]
impl Drop for RemoveEnvVarAtDrop {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
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

/// Boot the real supervisor in the HOSTILE boot env (audit #9): the
/// daemon itself "runs inside an agent" (`RLM_DEPTH=1`) and has NEVER
/// seen the client's marker. `PA_MARKER` is scrubbed so the only path it
/// can ever take into a worker is the create's launch env.
fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let log_file = std::fs::File::create(socket.with_extension("daemon.log")).expect("log file");
    let log_err = log_file.try_clone().expect("clone log file");
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
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env_remove("PRIME_API_KEY")
        .env_remove("PA_MARKER")
        // THE HOSTILE BOOT CONTEXT (audit #9): a supervisor started from
        // inside an agent's shell. Every worker it spawns would inherit
        // the recursion marker without the spawn-time strip.
        .env("RLM_DEPTH", "1")
        // The daemon-base sentinel: a var only the daemon's own env
        // carries. It MUST still reach the workers (the inherited base
        // layer under the client env).
        .env("PA_DAEMON_BASE_SENTINEL", "daemon-boot")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_err));
    // A supervisor killed at teardown must not leak its session workers
    // into later test binaries: the worker's supervisor-lost exit runs
    // on this short window instead of the 5-minute default.
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    Daemon {
        child: command.spawn().expect("spawn pa-daemon supervisor"),
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

/// JSONL supervisor client (command envelopes, id-matched responses).
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let writer = UnixStream::connect(socket).expect("connect");
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        let mut client = Self { reader, writer };
        let hello = client.read_line();
        (client, hello)
    }

    fn send_command(&mut self, id: &str, command: &Value) {
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

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => panic!("daemon closed the socket"),
            Ok(_) => serde_json::from_str(line.trim()).expect("line json"),
            Err(error) => panic!("read failed: {error}"),
        }
    }

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
            assert!(Instant::now() < deadline, "no response for {id}");
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send_command(id, command);
        self.read_response(id)
    }
}

/// One scripted session on the supervisor: create with the given extra
/// create fields, then prompt its single turn and wait for idle.
fn create_session(
    client: &mut Client,
    dir: &Path,
    script: &Path,
    id: &str,
    extra: Value,
) -> (String, String) {
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let mut create = json!({
        "type": "create",
        "name": id,
        "config": {
            "cwd": dir.to_string_lossy(),
            "sessionDir": sessions_dir.to_string_lossy(),
            "script": script.to_string_lossy(),
        },
    });
    if let Value::Object(fields) = extra {
        for (key, value) in fields {
            create[key] = value;
        }
    }
    let created = client.request(&format!("create-{id}"), &create);
    assert_eq!(created["success"], true, "create {id} failed: {created}");
    let data = &created["data"];
    let session_id = data["activeSessionId"]
        .as_str()
        .or_else(|| data["id"].as_str())
        .expect("session id")
        .to_string();
    let session_file = data["sessionFile"]
        .as_str()
        .expect("session file")
        .to_string();
    (session_id, session_file)
}

fn run_one_turn(client: &mut Client, session_id: &str, id: &str) {
    let prompted = client.request(
        &format!("prompt-{id}"),
        &json!({ "type": "prompt_and_wait", "activeSessionId": session_id, "message": "run the turn" }),
    );
    assert_eq!(prompted["success"], true, "prompt {id} failed: {prompted}");
}

/// A plain one-text-turn script (no kernel).
fn write_plain_script(dir: &Path) -> PathBuf {
    let script = dir.join("plain-script.json");
    std::fs::write(
        &script,
        json!({ "responses": [ { "text": "turn done", "delayMs": 10 } ] }).to_string(),
    )
    .expect("write script");
    script
}

/// A faux-engine script whose single kernel cell has a shell command record
/// the session's view of the three variables the contract names, and the
/// receipt that command writes. The command is a real child process of the
/// daemon: what it prints is what the session's commands run with.
fn write_env_receipt_script(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let receipt = dir.join(format!("{name}-receipt.txt"));
    let command = format!(
        "printf '%s\\n%s\\n%s' \"${{PA_MARKER:-ABSENT}}\" \"${{PA_DAEMON_BASE_SENTINEL:-ABSENT}}\" \"${{RLM_DEPTH:-ABSENT}}\" > '{receipt}'",
        receipt = receipt.to_string_lossy(),
    );
    let cell = format!("(bash {command:?})\n(displayln \"cell done\")");
    let script = dir.join(format!("{name}-script.json"));
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": { "code": cell } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write faux script");
    (script, receipt)
}

/// The receipt a session's kernel command wrote, once it exists and is
/// complete (three lines).
fn read_receipt(receipt: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(content) = std::fs::read_to_string(receipt) {
            if content.lines().count() == 3 {
                return content;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the kernel cell receipt never appeared at {}",
            receipt.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The `rlmDepth` of a session file's header line.
#[cfg(target_os = "linux")]
fn session_header_depth(session_file: &Path) -> u64 {
    let file = std::fs::File::open(session_file).expect("session file open");
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first_line)
        .expect("header line");
    let header: Value = serde_json::from_str(first_line.trim()).expect("header json");
    header["rlmDepth"].as_u64().unwrap_or(0)
}

/// Two sessions hosted by ONE daemon process: A is created with a client
/// launch env, B with none. Each session's kernel command sees the
/// daemon's own env as the base, A's sees the client's marker, B's does
/// not (one session's env never bleeds into another's), and neither sees
/// `RLM_DEPTH`. The sessions run inside the daemon: their recorded ids are
/// hosted ids, not processes. Both
/// workers stamp depth-0 root headers, and the durable descriptor for
/// A carries no trace of the client env.
#[test]
fn the_client_launch_env_reaches_only_its_own_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let (script_a, receipt_a) = write_env_receipt_script(dir.path(), "a");
    let (script_b, receipt_b) = write_env_receipt_script(dir.path(), "b");
    let marker = format!("a-{}", uuid::Uuid::new_v4());
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    // CREATE A: the client forwards its whole env (here: just the marker).
    let (session_a, file_a) = create_session(
        &mut client,
        dir.path(),
        &script_a,
        "a",
        json!({ "launchEnv": { "PA_MARKER": marker } }),
    );
    // CREATE B: no launch env at all. Both sessions are live in the same
    // process before either runs its command.
    let (session_b, file_b) = create_session(&mut client, dir.path(), &script_b, "b", json!({}));
    run_one_turn(&mut client, &session_b, "b");
    run_one_turn(&mut client, &session_a, "a");

    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket);
    let hosted = |session: &str| {
        let bytes = std::fs::read_to_string(descriptor_dir.join(format!("{session}.json")))
            .expect("descriptor");
        let descriptor: Value = serde_json::from_str(&bytes).expect("descriptor json");
        u32::try_from(descriptor["pid"].as_u64().expect("recorded id"))
            .is_ok_and(pa_daemon::session_host::is_hosted_pid)
    };
    assert_eq!(
        (
            read_receipt(&receipt_a),
            read_receipt(&receipt_b),
            hosted(&session_a),
            hosted(&session_b),
        ),
        (
            format!("{marker}\ndaemon-boot\nABSENT"),
            "ABSENT\ndaemon-boot\nABSENT".to_string(),
            true,
            true,
        ),
        "each session runs in its own env over the daemon's, inside the daemon"
    );

    // The recursion identity follows: a new root session is stamped
    // depth 0 even though the daemon itself "ran inside an agent".
    assert_eq!(
        session_header_depth(Path::new(&file_a)),
        0,
        "session A's header is stamped with the daemon's RLM_DEPTH"
    );
    assert_eq!(
        session_header_depth(Path::new(&file_b)),
        0,
        "session B's header is stamped with the daemon's RLM_DEPTH"
    );

    // The durable descriptor never carries the client env (TS parity:
    // `withoutSupervisorCreateFields` strips launchEnv from the durable
    // create command).
    let descriptor_bytes =
        std::fs::read_to_string(descriptor_dir.join(format!("{session_a}.json")))
            .expect("descriptor A");
    assert!(
        !descriptor_bytes.contains(&marker),
        "the worker descriptor leaked the client env"
    );
    let descriptor: Value = serde_json::from_str(&descriptor_bytes).expect("descriptor json");
    assert!(
        descriptor["createCommand"].get("launchEnv").is_none()
            && descriptor["createCommand"].get("launch_env").is_none(),
        "the durable create command carries a launch env"
    );
}

/// An `rlm.spawn` child is its own hosted session; TS runs it inside the
/// parent's. Parity: the child create forwards the parent session's own
/// env (filtered), so the child's commands run in it — while the parent's
/// `RLM_DEPTH` never rides along.
#[tokio::test]
async fn an_rlm_spawn_child_inherits_the_parent_worker_env() {
    use pa_core::session_engine::rlm_host::{RlmSpawnRequest, RlmSubagentHost};
    use pa_daemon::rlm_children::{ParentIdentity, SupervisorChildSessions};
    use pa_daemon::supervisor_link::SupervisorLink;

    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let (script, receipt) = write_env_receipt_script(dir.path(), "child");
    let marker = format!("parent-worker-{}", uuid::Uuid::new_v4());
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);

    // The test process stands in for the parent worker: its env is what
    // the spawn arm forwards. The daemon itself was scrubbed of the
    // marker at boot, so the ONLY way the child can see it is the
    // spawn arm's forward. The var never leaks past this test: the
    // guard removes it at scope end.
    std::env::set_var("PA_MARKER", &marker);
    let _marker_guard = RemoveEnvVarAtDrop("PA_MARKER");
    let children = SupervisorChildSessions::new(
        std::sync::Arc::new(SupervisorLink::new(socket.clone())),
        agent_dir.clone(),
        "parent-active-id".to_string(),
        std::sync::Arc::new(pa_daemon::model_allowlist::ModelRefusalTelemetry::new(
            agent_dir.clone(),
            /*telemetry_disabled*/ true,
        )),
        // This process stands in for the parent and has no worker token;
        // this fixture reads the child's env, not authenticated roster joins.
        String::new(),
    );
    children.set_identity(ParentIdentity {
        rlm_depth: 0,
        rlm_max_depth: 2,
        model: Some("faux/faux-1".to_string()),
        cwd: Some(agent_dir.to_string_lossy().to_string()),
        session_id: Some("parent-session-uuid".to_string()),
        session_file: Some(agent_dir.join("parent.jsonl").to_string_lossy().to_string()),
        thinking: None,
        child_script: Some(script.to_string_lossy().to_string()),
    });
    let handle = children
        .spawn(RlmSpawnRequest {
            prompt: "ship the lane".to_string(),
            name: Some("worker-a".to_string()),
            model: None,
            thinking: None,
            cwd: None,
            spawned_by_request_id: None,
            factory: None,
        })
        .await
        .expect("spawn");
    // The registry sits outside a live session turn here: release the
    // detached task prompt at the boundary the session would signal (the
    // existing rlm-children e2e's contract) and wait for the settle. The
    // child's one kernel command wrote the receipt: its own view of its
    // environment.
    children.notify_turn_done();
    let _ = children
        .collect(vec![handle.rlm_child_id.clone()], 30_000)
        .await;
    assert_eq!(
        // Off this runtime's only thread, which serves the child's events.
        tokio::task::spawn_blocking(move || read_receipt(&receipt))
            .await
            .expect("receipt reader"),
        // The supervisor itself "runs inside an agent" (the hostile boot
        // env): the child must not inherit that stale recursion marker.
        format!("{marker}\ndaemon-boot\nABSENT"),
        "the spawned child runs in the parent's env, without RLM_DEPTH"
    );
    let _ = children.delete_subagent(handle.rlm_child_id.clone()).await;
}

/// The update-restart roster's `launch_env` rows respawn workers with the
/// supervisor's INTERNAL identity only: the client env a session was
/// created with never lands in the durable roster.
#[test]
fn the_update_roster_never_carries_the_client_env() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let script = write_plain_script(dir.path());
    let marker = format!("roster-{}", uuid::Uuid::new_v4());
    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let (session_a, _file_a) = create_session(
        &mut client,
        dir.path(),
        &script,
        "a",
        json!({ "launchEnv": { "PA_MARKER": marker } }),
    );
    run_one_turn(&mut client, &session_a, "a");

    // The prepare transaction snapshots every resident and writes the
    // roster (the same file a new binary revives workers from). The
    // update id stays disjoint from the marker: the marker's absence
    // must be about the env rows, not the id string.
    let prepared = client.request(
        "prepare",
        &json!({ "type": "prepare_update_restart", "updateId": format!("u-{}", uuid::Uuid::new_v4()) }),
    );
    assert_eq!(prepared["success"], true, "prepare failed: {prepared}");

    let restarts = agent_dir.join("update-restarts");
    let mut roster: Option<Value> = None;
    for entry in std::fs::read_dir(&restarts)
        .expect("update-restarts dir")
        .flatten()
    {
        let prepared_dir = entry.path().join("prepared");
        for update in std::fs::read_dir(&prepared_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let bytes =
                std::fs::read_to_string(update.path().join("roster.json")).expect("roster.json");
            assert!(
                !bytes.contains(&marker),
                "the update roster leaked the client env:\n{bytes}"
            );
            roster = Some(serde_json::from_str(&bytes).expect("roster json"));
        }
    }
    let roster = roster.expect("the prepared roster file");
    for worker in roster["workers"].as_array().expect("worker rows") {
        let launch_env = worker["launch_env"].as_object().expect("launch_env row");
        // The row is the supervisor's internal identity only: the
        // client's marker key never lands in it (the raw-bytes check
        // above covers the value).
        assert!(
            !launch_env.contains_key("PA_MARKER"),
            "the roster row's launch env carries a client var: {worker}"
        );
        // And the row IS populated: the respawn env is really there.
        assert!(
            launch_env.contains_key(pa_daemon::worker::WORKER_TOKEN_ENV),
            "the roster row's launch env lost the internal identity: {worker}"
        );
    }
}
