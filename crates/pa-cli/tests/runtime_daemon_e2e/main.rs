//! The runtime daemon (phase 7a) end to end: the real `prime-agent --mode
//! daemon` with `PRIME_AGENT_RUNTIME_DAEMON=1` on an isolated agent dir and
//! the scripted faux provider, driven over wire2 on `daemon2.sock`.
//!
//! - A turn: hello, `session.create`, a publish on `agent/<id>/input`, the
//!   turn's stream and its `done`, then `transcript.page` holds the turn; a
//!   second client subscribing `after` a sequence number replays the same
//!   stored events, gap-free, in order.
//! - Ownership: with the old daemon (`pa-daemon supervisor`) started on the
//!   same agent dir after it (its boot reap spares the runtime daemon), a
//!   session either one holds is refused by the other (the session lease).
//! - One runtime daemon per agent dir.
//! - Interactive turn semantics (phase 7b): [`turns`].
//! - Reliability (phase 7c): crashes, heals, give-ups, stuck cells and the
//!   revival after the daemon's death: [`reliability`].
//! - The restart owner and the boot reap (phase 7c-2): [`keeper`].
//! - Scheduled jobs and heartbeats (phase 7d): [`schedules`].
//! - Subagents, agent messages and the event bus (phase 7e): [`family`].
//! - A TUI create's choices (its credential, profile and API key, its model
//!   scope, a scoped child's parent, its session dir and launch environment),
//!   built as the old daemon builds them (phase 8e2): [`create`]; `prime-agent
//!   shutdown --force` stops a kept runtime daemon: [`keeper`].
//! - What an ACP client needs (phase 7g-2): create options, model switches,
//!   MCP servers, schedule changes and the bus queries: [`client_surface`].
//! - The user's `!` commands (phase 7g-3): [`bash`].
//! - The rest of what an ACP client needs (phase 7g-5): input holds, a
//!   prompt's cancel, the settled wait, the model and level, and the
//!   subagent roster: [`acp_calls`].
//! - The old daemon's session-event payloads on the stream, numbered
//!   (phase 8b-2), and the queue view on it (phase 8b-3): [`stream`].
//! - pa-tui's five inbound goldens replayed through `wire2_compat`
//!   (phase 8b-3), their replies (8c-1) and the agents view's feeds (8c-2):
//!   [`goldens`].
//! - The agents view's roster feed from a session's open to its stop
//!   (phase 8c-2): [`agents_view`].
//! - The agents view's saved-session catalog, streamed through
//!   `wire2_compat` as the old daemon streams it (phase 8c-3):
//!   [`saved_list`].
//! - A session's setting switches, answered and written as the old daemon
//!   answers and writes them (phase 8d-1), and retargeting the session's
//!   next request (8d-1b): [`switches`].
//! - What the TUI reads beside a session's turns (its commands, MCP page,
//!   model catalog, the `/usage` page), its reload and the Anthropic
//!   warning's marker, answered as the old daemon answers them (phase
//!   8d-1b): [`info`].
//! - A session's tree moves and the reads beside them (the fork list, the
//!   copy text, `navigate_tree` with its branch summary, the HTML export),
//!   answered and recorded as the old daemon answers and records them
//!   (phase 8d-2): [`tree`].
//! - A session's queue edits, and the aborts of its compaction and its
//!   retry, answered and run as the old daemon answers and runs them (phase
//!   8d-2c): [`queue_edit`].
//! - A rewind's workspace checkpoints, restored as the old daemon restores
//!   them (phase 8d-2d): [`checkpoints`].
//! - A session's kernel questions, todo list and notifications, waiting for
//!   an attached client that can answer and answered as on the old daemon
//!   (phase 8d-2e): [`questions`].
//! - A session's side runs (`/btw`, `/better` and its outcome), answered
//!   and streamed as the old daemon answers and streams them (phase 8d-3):
//!   [`side_questions`].
//! - The agents view's kernel activity (a session's kernel shell commands,
//!   its factory lane, the persistent shells and their catalog rows),
//!   answered as the old daemon answers it (phase 8d-4a):
//!   [`kernel_activity`].
//! - The agents view's session controls (the agent-message ingest's status,
//!   pause, resume and clear, the RLM depth bound, the subagent delete),
//!   answered as the old daemon answers them (phase 8d-4b):
//!   [`agent_controls`].
//! - A session's heartbeat (its read, set, update and manage, and the
//!   withdrawal of its queued fire), answered as the old daemon answers it
//!   (phase 8d-4c): [`heartbeats`]; the saved catalog's passive subagents:
//!   [`saved_list`].
//! - The daemon's own end (`shutdown`, the stop signal, `restart` mid-turn
//!   and the next daemon's continuation), as the old daemon ends (phase
//!   8d-5a): [`lifecycle`]; a kept daemon's `shutdown` is not restarted:
//!   [`keeper`].
//! - The update restart's prepare and commit (the gate, the roster, the
//!   update's closing), as the old daemon answers and writes them (phase
//!   8d-5b), and the next daemon's restore of the roster (8d-5b2):
//!   [`update`]; a TS updater's restart handoff restored on the selected
//!   daemon (8d-5b2): [`legacy_update`].
//! - A distribution build's boot import of the official build's chats, its
//!   pushes, and what a client that connects mid-import sees, as on the old
//!   daemon (phase 8d-5c): [`history_import`].
//! - A kernel webhook source: an authenticated `POST` runs an untrusted
//!   turn, a bad token none (phase 9): [`webhooks`].
#![cfg(unix)]

mod acp_calls;
mod agent_controls;
mod agents_view;
mod bash;
mod checkpoints;
mod client_surface;
mod create;
mod family;
mod goldens;
mod heartbeats;
mod history_import;
mod info;
mod keeper;
mod kernel_activity;
mod legacy_update;
mod lifecycle;
mod questions;
mod queue_edit;
mod reliability;
mod replace;
mod saved_list;
mod schedules;
mod side_questions;
mod stream;
#[path = "../support/mod.rs"]
mod support;
mod switches;
mod tree;
mod turns;
mod update;
mod webhooks;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::event_log::{Topic, TopicFilter};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// How long one frame may take to arrive before the test fails.
const FRAME_WAIT: Duration = Duration::from_secs(60);

/// Env keys the test's own environment may carry that must not reach the
/// daemons it starts.
const SCRUB_ENV: [&str; 6] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
    "PRIME_AGENT_RUNTIME_DAEMON",
    "PI_PACKAGE_DIR",
];

/// Whether env var `key` can sign a provider in (pa-ai's env keys and its
/// cloud credential variables).
fn credential_env(key: &str) -> bool {
    key.ends_with("_API_KEY")
        || key.ends_with("_TOKEN")
        || key.ends_with("_SECRET_KEY")
        || ["AWS_", "GOOGLE_CLOUD_", "GCLOUD_"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// A temp dir short enough for every socket under it (macOS's `$TMPDIR` is
/// not), canonical so the paths the daemons report compare equal.
fn fixture_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(
        pa_types::platform::canonicalize(Path::new("/tmp")).expect("canonical /tmp"),
    )
    .expect("fixture dir")
}

/// A daemon process, killed at the end of the test.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The isolated agent dir and sockets of one test.
struct Bed {
    dir: tempfile::TempDir,
    /// The agent dir, under `dir`.
    agent: PathBuf,
}

impl Bed {
    fn new() -> Self {
        Bed::with_agent_dir(Path::new("agent"))
    }

    /// A bed whose agent dir is `agent`, relative to its dir (the bed's
    /// dir is the daemons' home).
    fn with_agent_dir(agent: &Path) -> Self {
        let dir = fixture_dir();
        let agent = dir.path().join(agent);
        std::fs::create_dir_all(&agent).unwrap();
        Bed { dir, agent }
    }

    fn agent_dir(&self) -> PathBuf {
        self.agent.clone()
    }

    fn old_socket(&self) -> PathBuf {
        self.dir.path().join("daemon.sock")
    }

    fn runtime_socket(&self) -> PathBuf {
        self.dir.path().join("daemon2.sock")
    }

    /// The isolated environment, and the daemon's standard error to
    /// `<dir>/<log>` for a failure to show.
    fn env(&self, command: &mut Command, log: &str) {
        let log = std::fs::File::create(self.dir.path().join(log)).unwrap();
        command
            .current_dir(self.dir.path())
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env("PRIME_AGENT_AGENT_DIR", self.agent_dir())
            .env("PRIME_AGENT_CODING_AGENT_DIR", self.agent_dir())
            .env("DO_NOT_TRACK", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        for key in SCRUB_ENV {
            command.env_remove(key);
        }
        // Provider credentials the machine carries (a CI runner has none) would
        // make the daemons' signed-in providers depend on where the test runs.
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(credential_env) {
                command.env_remove(key);
            }
        }
    }

    /// The runtime daemon command beside the old daemon's `daemon_socket`,
    /// its faux provider scripted by `script`.
    fn runtime_daemon_command(&self, script: &Value, daemon_socket: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
        self.env(&mut command, "runtime-daemon.log");
        command
            .args(["--mode", "daemon", "--daemon-socket"])
            .arg(daemon_socket)
            .env("PRIME_AGENT_RUNTIME_DAEMON", "1")
            .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string());
        command
    }

    /// The runtime daemon, listening on `daemon2.sock`.
    fn runtime_daemon(&self, script: &Value) -> Daemon {
        self.runtime_daemon_with(script, &[])
    }

    /// The runtime daemon with `env` over the bed's, listening.
    ///
    /// A daemon the test killed can still accept on its socket for a moment
    /// after it was reaped: under load the kernel's last close of the killed
    /// process's listener lags the reap, so a connect succeeds there and the
    /// connection is then reset (or the next connect refused) while the next
    /// daemon is still preparing the socket. The next daemon therefore starts
    /// once the socket refuses; a connect that succeeds after that is its.
    fn runtime_daemon_with(&self, script: &Value, env: &[(&str, &str)]) -> Daemon {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::os::unix::net::UnixStream::connect(self.runtime_socket()).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "a killed runtime daemon still listens on {}",
                self.runtime_socket().display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut command = self.runtime_daemon_command(script, &self.old_socket());
        command.envs(env.iter().copied());
        let mut child = command.spawn().expect("spawn the runtime daemon");
        support::await_listening(&self.runtime_socket(), &mut child);
        Daemon(child)
    }

    /// The old daemon (the `pa-daemon` supervisor binary built beside
    /// `prime-agent`), listening on `daemon.sock`.
    fn old_daemon(&self) -> Daemon {
        self.old_daemon_with(&[])
    }

    /// The old daemon with `env` over the bed's, listening.
    fn old_daemon_with(&self, env: &[(&str, &str)]) -> Daemon {
        let binary = Path::new(env!("CARGO_BIN_EXE_prime-agent"))
            .parent()
            .expect("the profile dir")
            .join("pa-daemon");
        assert!(
            binary.exists(),
            "pa-daemon not built at {}: run `cargo build -p pa-daemon` first",
            binary.display()
        );
        let mut command = Command::new(binary);
        self.env(&mut command, "old-daemon.log");
        command
            .arg("supervisor")
            .arg("--socket")
            .arg(self.old_socket())
            .arg("--agent-dir")
            .arg(self.agent_dir())
            .envs(env.iter().copied());
        let mut child = command.spawn().expect("spawn the old daemon");
        support::await_listening(&self.old_socket(), &mut child);
        Daemon(child)
    }
}

/// A wire2 client.
struct Client {
    frames: PrivateFrameReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    /// Connects and says hello; returns the welcome's last sequence number.
    async fn connect(socket: &Path) -> (Self, u64) {
        let (reader, writer) = tokio::net::UnixStream::connect(socket)
            .await
            .expect("connect to the runtime daemon")
            .into_split();
        let mut client = Client {
            frames: PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS),
            writer,
            next_id: 0,
        };
        client
            .send(&ClientFrame::Hello {
                version: WIRE2_VERSION,
                client: "runtime-daemon-e2e".to_string(),
            })
            .await;
        match client.frame().await {
            ServerFrame::Welcome { last_seq, .. } => (client, last_seq),
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    async fn send(&mut self, frame: &ClientFrame) {
        let bytes = encode_private_frame(
            &serde_json::to_value(frame).unwrap(),
            &[],
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .unwrap();
        self.writer.write_all(&bytes).await.unwrap();
    }

    async fn frame(&mut self) -> ServerFrame {
        let frame = tokio::time::timeout(FRAME_WAIT, self.frames.read_frame())
            .await
            .expect("a frame within the wait")
            .expect("a readable frame")
            .expect("the connection stays open");
        serde_json::from_value(frame.header).expect("a wire2 server frame")
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("r{}", self.next_id)
    }

    /// Waits for the reply to `id`, keeping the events that arrive first.
    async fn reply(&mut self, id: &str, events: &mut Vec<ServerFrame>) -> Outcome {
        loop {
            match self.frame().await {
                ServerFrame::Reply {
                    id: replied,
                    outcome,
                } if replied == id => return outcome,
                other => events.push(other),
            }
        }
    }

    async fn call(&mut self, method: &str, args: Value) -> Outcome {
        self.call_keeping(method, args, &mut Vec::new()).await
    }

    /// `call`, keeping in `events` the frames that arrive before its reply.
    async fn call_keeping(
        &mut self,
        method: &str,
        args: Value,
        events: &mut Vec<ServerFrame>,
    ) -> Outcome {
        let id = self.id();
        self.send(&ClientFrame::Call {
            id: id.clone(),
            method: method.to_string(),
            args,
        })
        .await;
        self.reply(&id, events).await
    }

    async fn query(&mut self, name: &str, args: Value) -> Outcome {
        let id = self.id();
        self.send(&ClientFrame::Query {
            id: id.clone(),
            name: name.to_string(),
            args,
        })
        .await;
        self.reply(&id, &mut Vec::new()).await
    }

    /// Subscribes `filter` from `since`; returns what the replay sent before
    /// its `backfilled`, and the sequence number it covers.
    async fn subscribe(&mut self, filter: &str, since: Since) -> (Vec<ServerFrame>, u64) {
        self.send(&ClientFrame::Subscribe {
            sub_id: "s".to_string(),
            filter: TopicFilter::new(filter).unwrap(),
            since,
        })
        .await;
        let mut replayed = Vec::new();
        loop {
            match self.frame().await {
                ServerFrame::Backfilled { through_seq, .. } => return (replayed, through_seq),
                other => replayed.push(other),
            }
        }
    }
}

/// The result of an `ok` outcome.
fn ok(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Ok { result } => result,
        Outcome::Error { message, .. } => panic!("expected ok, got the error {message}"),
    }
}

/// The message of an `error` outcome.
fn refused(outcome: Outcome) -> String {
    match outcome {
        Outcome::Error { message, .. } => message,
        Outcome::Ok { result } => panic!("expected a refusal, got {result}"),
    }
}

/// The stored events among `frames`, as (seq, topic).
fn stored(frames: &[ServerFrame]) -> Vec<(u64, String)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event { event, .. } if event.seq > 0 => {
                Some((event.seq, event.event.topic.to_string()))
            }
            _ => None,
        })
        .collect()
}

/// Publishes `text` as session `id`'s input and reads the subscription's
/// frames until the task's `done`.
async fn run_turn(client: &mut Client, id: &str, text: &str) -> Vec<ServerFrame> {
    let request = client.id();
    client
        .send(&ClientFrame::Publish {
            id: request.clone(),
            topic: Topic::new(format!("agent/{id}/input")).unwrap(),
            payload: json!({ "text": text }),
        })
        .await;
    let mut frames = Vec::new();
    ok(client.reply(&request, &mut frames).await);
    let done = format!("agent/{id}/done");
    while !frames.iter().any(|frame| {
        matches!(frame, ServerFrame::Event { event, .. } if event.event.topic.to_string() == done)
    }) {
        frames.push(client.frame().await);
    }
    frames
}

/// The `type`s of the transcript rows a page holds, with their message role
/// and text where they carry one.
fn turn_rows(page: &Value) -> Vec<(String, Option<String>, Option<String>)> {
    page["entries"]
        .as_array()
        .expect("a page of entries")
        .iter()
        .filter(|row| row["type"] == "message")
        .map(|row| {
            let message = &row["message"];
            let text = match &message["content"] {
                Value::String(text) => Some(text.clone()),
                Value::Array(blocks) => blocks
                    .iter()
                    .find_map(|block| block["text"].as_str().map(str::to_string)),
                _ => None,
            };
            (
                row["type"].as_str().unwrap_or_default().to_string(),
                message["role"].as_str().map(str::to_string),
                text,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_streams_to_done_pages_its_transcript_and_replays_gap_free() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["first answer"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"]
        .as_str()
        .expect("a session id")
        .to_string();
    let (_, start) = client.subscribe(&format!("agent/{id}/#"), Since::Now).await;

    let frames = run_turn(&mut client, &id, "say something").await;
    let streamed = frames
        .iter()
        .filter(|frame| matches!(frame, ServerFrame::Event { event, .. } if event.seq == 0))
        .count();
    assert!(streamed > 0, "the turn streamed live events: {frames:?}");
    let page = ok(client
        .query("transcript.page", json!({ "session": id }))
        .await);
    assert_eq!(
        turn_rows(&page),
        vec![
            (
                "message".to_string(),
                Some("user".to_string()),
                Some("say something".to_string())
            ),
            (
                "message".to_string(),
                Some("assistant".to_string()),
                Some("first answer".to_string())
            ),
        ]
    );

    // A second client from the sequence number before the turn: the same
    // stored events, in order, with nothing missing.
    let (mut second, _) = Client::connect(&bed.runtime_socket()).await;
    let (replayed, through) = second
        .subscribe(&format!("agent/{id}/#"), Since::After { seq: start })
        .await;
    let live = stored(&frames);
    assert_eq!(
        (stored(&replayed), through >= live.last().unwrap().0),
        (live, true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_old_and_the_runtime_daemon_never_hold_one_session() {
    let bed = Bed::new();
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(&script, json!({ "responses": ["old answer"] }).to_string()).unwrap();
    // The runtime daemon first: the old daemon's boot reap, which takes a
    // command line naming its socket for a predecessor, must spare it.
    let _runtime = bed.runtime_daemon(&json!({ "responses": ["runtime answer"] }));
    let _old = bed.old_daemon();

    // The old daemon holds a session: the runtime daemon cannot open it.
    let old_file = {
        let socket = bed.old_socket();
        let (cwd, session_dir, script) = (
            bed.dir.path().to_path_buf(),
            bed.agent_dir().join("sessions"),
            script.clone(),
        );
        tokio::task::spawn_blocking(move || {
            old_wire::create(&socket, "old", &cwd, &session_dir, &script, None)
        })
        .await
        .unwrap()
        .expect("the old daemon creates a session")
    };
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let refusal = refused(
        client
            .call("session.create", json!({ "path": old_file }))
            .await,
    );
    assert!(
        refusal.contains("currently open in another"),
        "the lease refuses the runtime daemon with the hold refusal: {refusal}"
    );

    // The runtime daemon holds a session: the old daemon cannot open it.
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    let runtime_file = PathBuf::from(created["path"].as_str().unwrap());
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "write the file").await;
    let old_refusal = {
        let socket = bed.old_socket();
        let (cwd, session_dir) = (
            bed.dir.path().to_path_buf(),
            bed.agent_dir().join("sessions"),
        );
        tokio::task::spawn_blocking(move || {
            old_wire::create(
                &socket,
                "reopen",
                &cwd,
                &session_dir,
                &script,
                Some(&runtime_file),
            )
        })
        .await
        .unwrap()
        .expect_err("the old daemon is refused")
    };
    assert!(
        old_refusal.contains("already active") || old_refusal.contains("session_already_active"),
        "the lease refuses the old daemon: {old_refusal}"
    );
}

#[test]
fn a_second_runtime_daemon_on_one_agent_dir_refuses_to_start() {
    let bed = Bed::new();
    let script = json!({ "responses": [] });
    let _first = bed.runtime_daemon(&script);
    // Its own socket, so only the agent dir is shared.
    let mut second =
        bed.runtime_daemon_command(&script, &bed.dir.path().join("other").join("daemon.sock"));
    second.stderr(Stdio::piped());
    let output = second.output().expect("run the second runtime daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        (
            output.status.code(),
            stderr.contains("a runtime daemon is already running")
        ),
        (Some(1), true),
        "stderr: {stderr}"
    );
}

/// The old daemon's line protocol, enough to create a session.
mod old_wire {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    /// Creates the session `name` on the scripted engine (opening
    /// `session_path` when given); its session file, or the refusal's text.
    pub(super) fn create(
        socket: &Path,
        name: &str,
        cwd: &Path,
        session_dir: &Path,
        script: &Path,
        session_path: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let stream = UnixStream::connect(socket).expect("connect to the old daemon");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let config = json!({
            "cwd": cwd,
            "sessionDir": session_dir,
            "script": script,
        });
        let mut command = json!({ "type": "create", "name": name, "config": config });
        if let Some(path) = session_path {
            command["sessionPath"] = json!(path);
        }
        let envelope = json!({
            "type": "command",
            "id": "create",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        writer
            .write_all(format!("{envelope}\n").as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_mins(1);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => panic!("the old daemon closed the connection"),
                Ok(_) => {
                    let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if frame["id"] != "create" {
                        continue;
                    }
                    if frame["success"] != true {
                        return Err(frame.to_string());
                    }
                    let file = frame["data"]["sessionFile"]
                        .as_str()
                        .or_else(|| frame["data"]["session"]["sessionFile"].as_str())
                        .unwrap_or_else(|| panic!("no session file in {frame}"));
                    return Ok(PathBuf::from(file));
                }
                Err(_) => assert!(Instant::now() < deadline, "the old daemon never replied"),
            }
        }
    }
}
