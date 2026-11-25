//! The daemon's restart owner (`pa_daemon::keeper`): every session lives in
//! one daemon process, so something must bring that process back when a
//! fault no panic hook can contain ends it.
//!
//! - **Self-heal with a client attached**: the daemon is killed (SIGKILL) while a
//!   turn runs; it returns on the same socket with nothing asking for it, the
//!   new `daemon_hello` carries the recorded death, and the interrupted
//!   session is revived, reports what happened and serves again.
//! - **Self-heal with no client**: the same death with nobody connected; the
//!   interrupted turn still runs to its end before any client returns.
//! - **A deliberate stop stays stopped**: a `shutdown` ends the restart owner
//!   too, and so does a shutdown that escalated to a kill.
//! - **A crash loop ends**: a daemon that dies at every start is given up on
//!   at the limit, each death is recorded, and the next daemon reports it.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

/// How long any one wait in this file may take before the test fails.
const WAIT: Duration = Duration::from_secs(60);

/// The restart owner process; dropping it reaps the owner and the daemon it
/// keeps.
struct Keeper {
    child: Child,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

/// Start `pa-daemon supervisor` as the restart owner of the daemon on
/// `socket`.
fn spawn_keeper(socket: &Path, agent_dir: &Path) -> Keeper {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join("daemon-stderr.log"))
        .expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .env(pa_daemon::keeper::KEEPER_ENV, "1")
        // Session sockets land under TMPDIR: keep them in the test's own dir.
        .env("TMPDIR", agent_dir.parent().expect("test dir"))
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .env_remove("PRIME_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    Keeper {
        child: command.spawn().expect("spawn the restart owner"),
    }
}

/// JSONL supervisor client. Every read is bounded.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    hello: Value,
    passed: Vec<Value>,
}

impl Client {
    /// Connect once and read the hello; `None` while no daemon answers.
    fn try_connect(socket: &Path) -> Option<Self> {
        let writer = UnixStream::connect(socket).ok()?;
        writer.set_read_timeout(Some(WAIT)).expect("read timeout");
        let mut reader = BufReader::new(writer.try_clone().expect("clone"));
        let mut line = String::new();
        // A daemon killed between the accept and the hello reads as absent.
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let hello: Value = serde_json::from_str(line.trim()).ok()?;
        (hello["type"] == "daemon_hello").then_some(Self {
            reader,
            writer,
            hello,
            passed: Vec::new(),
        })
    }

    /// Wait for a daemon `accepted` by its hello to answer on `socket`.
    fn connect_when(socket: &Path, what: &str, accepted: impl Fn(&Value) -> bool) -> Self {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(client) = Self::try_connect(socket).filter(|client| accepted(&client.hello))
            {
                return client;
            }
            assert!(Instant::now() < deadline, "no daemon answered: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
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

    /// The first frame `wanted` accepts; `None` when the daemon closes the
    /// connection first.
    fn read_until(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(index) = self.passed.iter().position(&wanted) {
            return Some(self.passed.remove(index));
        }
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return None,
                Err(error) => panic!(
                    "waiting for {what}: {error}; the frames read past: {:#?}",
                    self.passed
                ),
            }
            let frame: Value = serde_json::from_str(line.trim()).expect("frame json");
            if wanted(&frame) {
                return Some(frame);
            }
            self.passed.push(frame);
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send(id, command);
        self.read_until(id, |frame| frame["id"] == id)
            .unwrap_or_else(|| panic!("the daemon closed the connection before answering {id}"))
    }

    /// Create a scripted session, attach to it, start its one slow turn and
    /// wait until that turn is running.
    fn start_slow_turn(&mut self, dir: &Path) -> String {
        let script_path = dir.join("script.json");
        std::fs::write(
            &script_path,
            json!({ "responses": [ { "text": "slow answer", "delayMs": 1500 } ] }).to_string(),
        )
        .expect("write script");
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let created = self.request(
            "create",
            &json!({
                "type": "create",
                "name": "survivor",
                "config": {
                    "cwd": dir.to_string_lossy(),
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
        let attached = self.request(
            "attach",
            &json!({ "type": "attach", "activeSessionId": session }),
        );
        assert_eq!(attached["success"], true, "attach failed: {attached}");
        let prompted = self.request(
            "prompt",
            &json!({ "type": "prompt", "activeSessionId": session, "message": "run the turn" }),
        );
        assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
        self.read_until("the turn's start", |frame| {
            frame["type"] == "session_event"
                && frame["activeSessionId"] == session.as_str()
                && frame["event"]["type"] == "agent_start"
        })
        .expect("the turn starts");
        session
    }

    /// The session's `list` row once `settled` accepts it.
    fn row_when(&mut self, session: &str, what: &str, settled: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let listed = self.request("list", &json!({ "type": "list" }));
            let row = listed["data"]["sessions"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == session).cloned());
            if let Some(row) = row.filter(&settled) {
                return row;
            }
            assert!(Instant::now() < deadline, "{what}: {listed}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn hello_pid(hello: &Value) -> u32 {
    u32::try_from(hello["supervisorPid"].as_u64().expect("the daemon's pid")).expect("a pid")
}

fn sigkill(pid: u32) {
    kill(
        Pid::from_raw(i32::try_from(pid).expect("a pid")),
        Signal::SIGKILL,
    )
    .expect("SIGKILL");
}

/// The restart owner's death log for `socket`.
fn death_log(agent_dir: &Path, socket: &Path) -> Vec<Value> {
    std::fs::read_to_string(
        pa_daemon::descriptor::descriptor_dir(agent_dir, socket)
            .join(pa_daemon::keeper::DAEMON_DEATH_LOG),
    )
    .unwrap_or_default()
    .lines()
    .map(|line| serde_json::from_str(line).expect("death record json"))
    .collect()
}

/// Wait for the restart owner to exit and return its exit code.
fn wait_exit(keeper: &mut Keeper) -> Option<i32> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = keeper.child.try_wait().expect("poll the restart owner") {
            return status.code();
        }
        assert!(Instant::now() < deadline, "the restart owner never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The session file's text, wherever under `dir/sessions` the daemon put it.
fn transcripts(dir: &Path) -> String {
    let mut text = String::new();
    let mut pending = vec![dir.join("sessions")];
    while let Some(path) = pending.pop() {
        let Ok(listing) = common::transcripts(&path).list(&path) else {
            continue;
        };
        pending.extend(listing.directories);
        for transcript in listing.transcripts {
            text.push_str(&common::read_transcript(&transcript.locator).unwrap_or_default());
        }
    }
    text
}

#[test]
fn a_killed_daemon_returns_by_itself_reports_its_death_and_the_session_serves_again() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let keeper = spawn_keeper(&socket, &agent_dir);
    let mut client = Client::connect_when(&socket, "the first daemon", |_| true);
    let first_pid = hello_pid(&client.hello);
    let first_hello = (
        client.hello["keeperPid"].clone(),
        client.hello["daemonDeath"].clone(),
    );
    let session = client.start_slow_turn(dir.path());

    // The whole daemon dies mid-turn with a client attached: nothing in it
    // settles anything, and the client only sees its connection end.
    sigkill(first_pid);
    let closed = client.read_until("the connection's end", |_| false);

    // Nothing asks for the daemon; the client only waits for it.
    let mut client = Client::connect_when(&socket, "the restarted daemon", |hello| {
        hello_pid(hello) != first_pid
    });
    let death = client.hello["daemonDeath"].clone();
    let row = client.row_when(&session, "the session never came back", |row| {
        row["lifecycle"] == "live" && row["lastCrash"].is_object()
    });
    // The roster every agents view renders from says the session went down and is back.
    let deadline = Instant::now() + WAIT;
    let roster_fault = loop {
        let roster = client.request("roster", &json!({ "type": "roster_subscribe" }));
        let fault = roster["data"]["roster"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .map(|entry| &entry["summary"]["lastCrash"])
                    .find(|fault| fault["state"] == "recovered")
            })
            .cloned();
        if let Some(fault) = fault {
            break json!([fault["kind"], fault["state"]]);
        }
        assert!(
            Instant::now() < deadline,
            "the roster never showed the recovered session: {roster}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let attached = client.request(
        "reattach",
        &json!({ "type": "attach", "activeSessionId": session }),
    );
    let served = client.request(
        "after",
        &json!({ "type": "prompt_and_wait", "activeSessionId": session, "message": "again" }),
    );
    let log = death_log(&agent_dir, &socket);
    assert_eq!(
        (
            first_hello,
            closed,
            client.hello["keeperPid"].clone(),
            json!({
                "pid": death["pid"], "signal": death["signal"], "exitCode": death["exitCode"],
                "liveSessions": death["liveSessions"], "busySessions": death["busySessions"],
                "consecutive": death["consecutive"], "outcome": death["outcome"],
            }),
            log,
            row["lastCrash"]["kind"].clone(),
            row["lastCrash"]["message"].clone(),
            roster_fault,
            attached["success"].clone(),
            served["success"].clone(),
        ),
        (
            (json!(keeper.child.id()), Value::Null),
            None,
            json!(keeper.child.id()),
            json!({
                "pid": first_pid, "signal": 9, "exitCode": null,
                "liveSessions": 1, "busySessions": 1,
                "consecutive": 1, "outcome": "restarted",
            }),
            vec![death.clone()],
            json!("daemon_restart"),
            json!("the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn was started again"),
            json!(["daemon_restart", "recovered"]),
            json!(true),
            json!(true),
        ),
        "death: {death}\nrow: {row}\nserved: {served}"
    );
}

#[test]
fn a_daemon_killed_with_no_client_attached_returns_and_finishes_the_interrupted_turn() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let _keeper = spawn_keeper(&socket, &agent_dir);
    let (first_pid, session) = {
        let mut client = Client::connect_when(&socket, "the first daemon", |_| true);
        let session = client.start_slow_turn(dir.path());
        (hello_pid(&client.hello), session)
    };
    // The client is gone; the turn is still running in the background.
    assert!(
        !transcripts(dir.path()).contains("slow answer"),
        "the turn must still be running when the daemon is killed"
    );
    sigkill(first_pid);

    // No client connects: the restarted daemon revives the session and runs
    // the interrupted turn to its end on its own, which the session's own
    // file shows.
    let deadline = Instant::now() + WAIT;
    while !transcripts(dir.path()).contains("slow answer") {
        assert!(
            Instant::now() < deadline,
            "the interrupted turn never finished without a client; deaths: {:?}",
            death_log(&agent_dir, &socket)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut client = Client::connect_when(&socket, "the restarted daemon", |_| true);
    let row = client.row_when(&session, "the session is not live", |row| {
        row["lifecycle"] == "live"
    });
    assert_eq!(
        (
            hello_pid(&client.hello) == first_pid,
            client.hello["daemonDeath"]["signal"].clone(),
            row["lastCrash"]["kind"].clone(),
            death_log(&agent_dir, &socket).len(),
        ),
        (false, json!(9), json!("daemon_restart"), 1),
        "hello: {}\nrow: {row}",
        client.hello
    );
}

#[test]
fn a_deliberate_shutdown_is_not_restarted_even_when_it_ends_in_a_kill() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");

    // A shutdown the daemon completes: it exits cleanly and its owner with it.
    let clean_socket = dir.path().join("clean.sock");
    let mut clean = spawn_keeper(&clean_socket, &agent_dir);
    let mut client = Client::connect_when(&clean_socket, "the daemon", |_| true);
    let stopped = client.request("shutdown", &json!({ "type": "shutdown" }));
    let clean_exit = wait_exit(&mut clean);

    // A stop that escalates: the stopper records its intent, then kills the
    // daemon outright (what a forced shutdown does to a daemon that does not
    // answer). The owner reads the kill as the stop it is.
    let killed_socket = dir.path().join("killed.sock");
    let mut killed = spawn_keeper(&killed_socket, &agent_dir);
    let client = Client::connect_when(&killed_socket, "the daemon", |_| true);
    pa_daemon::keeper::record_stop_intent(&agent_dir, &killed_socket).expect("stop intent");
    sigkill(hello_pid(&client.hello));
    let killed_exit = wait_exit(&mut killed);

    assert_eq!(
        (
            stopped["success"].clone(),
            clean_exit,
            killed_exit,
            Client::try_connect(&clean_socket).is_some(),
            Client::try_connect(&killed_socket).is_some(),
            death_log(&agent_dir, &clean_socket),
            death_log(&agent_dir, &killed_socket),
        ),
        (
            json!(true),
            Some(0),
            Some(0),
            false,
            false,
            Vec::<Value>::new(),
            Vec::<Value>::new()
        )
    );
}

#[test]
fn a_daemon_that_dies_at_every_start_is_given_up_on_and_the_next_daemon_reports_it() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    // The socket's directory is a regular file: the daemon cannot bind and
    // exits at once, every time.
    let blocker = dir.path().join("blocked");
    std::fs::write(&blocker, "not a directory").expect("blocker");
    let socket = blocker.join("daemon.sock");
    let mut looping = spawn_keeper(&socket, &agent_dir);
    let gave_up_exit = wait_exit(&mut looping);
    let log = death_log(&agent_dir, &socket);

    // The cause goes away and a client launches the daemon again: the first
    // daemon of the new owner tells its clients what happened before it.
    std::fs::remove_file(&blocker).expect("unblock");
    std::fs::create_dir_all(&blocker).expect("socket dir");
    let _keeper = spawn_keeper(&socket, &agent_dir);
    let client = Client::connect_when(&socket, "the daemon after the give-up", |_| true);

    let summary: Vec<Value> = log
        .iter()
        .map(|record| {
            json!([
                record["consecutive"],
                record["outcome"],
                record["signal"],
                record["exitCode"].is_number()
            ])
        })
        .collect();
    assert_eq!(
        (gave_up_exit, summary, client.hello["daemonDeath"].clone(),),
        (
            Some(1),
            vec![
                json!([1, "restarted", null, true]),
                json!([2, "restarted", null, true]),
                json!([3, "restarted", null, true]),
                json!([4, "restarted", null, true]),
                json!([5, "gave_up", null, true]),
            ],
            log.last().cloned().expect("a final death record"),
        )
    );
}
