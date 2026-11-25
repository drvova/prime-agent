//! Scheduled-job wake e2e across a daemon kill -9 + relaunch. Sessions are
//! hosted inside the daemon and die with it: the boot revives the one that
//! was mid-turn, whose heartbeat fires again, and never the one a client had
//! killed. (A detached command cannot outlive the daemon that hosted its
//! session — the boot reaps it, see `supervisor_restart_e2e` — so no
//! completion notice crosses a restart.)
#![allow(clippy::large_futures)]
// 64-bit-only targets; the narrowing casts sit at bounded OS boundaries.
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
    #[allow(dead_code)]
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
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
        .env_remove("PRIME_API_KEY")
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    // Listening, not merely bound: the socket file appears before `listen`.
    let deadline = Instant::now() + Duration::from_secs(15);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        child,
        socket: socket.to_path_buf(),
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).expect("connect");
        let writer = stream.try_clone().expect("clone");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_mins(2);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
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

    fn request(&mut self, id: &str, command: &Value) -> Value {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .unwrap_or_else(|error| panic!("write command {id}: {error}"));
        let deadline = Instant::now() + Duration::from_mins(2);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }

    fn listed_sessions(&mut self) -> Vec<(String, String)> {
        let response = self.request("list", &json!({ "type": "list" }));
        assert_eq!(response["success"], true, "list failed: {response}");
        response["data"]["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|summary| {
                (
                    summary["id"].as_str().unwrap_or_default().to_string(),
                    summary["sessionId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    }
}

fn wait_until<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "condition never became true");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A session's armed heartbeat must keep firing across a daemon death with
/// nobody reopening the session.
#[test]
fn a_heartbeat_keeps_firing_across_a_daemon_restart() {
    let root = tempfile::TempDir::new().expect("temp dir");
    let dir = root.path().to_path_buf();
    let agent_dir = dir.join("agent");
    let socket = dir.join("stop.sock");
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
                { "text": "the lane works on" },
            ]
        })
        .to_string(),
    )
    .expect("write faux script");

    let mut supervisor = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let created = client.request(
        "c1",
        &json!({
            "type": "create",
            "name": "hb-lane",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let active_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["activeSessionId"].as_str())
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let session_file = agent_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let heartbeat = client.request(
        "hb",
        &json!({
            "type": "heartbeat_set",
            "activeSessionId": active_id,
            "schedule": "every 10s",
            "prompt": "liveness ping",
        }),
    );
    assert_eq!(
        heartbeat["success"], true,
        "heartbeat_set failed: {heartbeat}"
    );
    let first_turn = client.request(
        "p1",
        &json!({
            "type": "prompt_and_wait",
            "activeSessionId": active_id,
            "message": "warm the lane",
        }),
    );
    assert_eq!(
        first_turn["success"], true,
        "first turn failed: {first_turn}"
    );

    let rows_before_restart = wait_until(Duration::from_secs(20), || {
        let rows = session_rows_containing(&session_file, "heartbeat_prompt");
        (rows > 0).then_some(rows)
    });

    drop(client);
    // The daemon dies hard, and the idle session with it.
    let _ = supervisor.child.kill();
    let _ = supervisor.child.wait();
    drop(supervisor);
    std::fs::remove_file(&socket).ok();
    let _supervisor = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    wait_until(Duration::from_secs(30), || {
        client
            .listed_sessions()
            .iter()
            .any(|(active, id)| *active == active_id || *id == session_id)
            .then_some(())
    });

    // THE ASSERT: the heartbeat keeps firing after the restart — the row count
    // grows past the pre-restart snapshot.
    let grew = wait_until(Duration::from_secs(30), || {
        let rows = session_rows_containing(&session_file, "heartbeat_prompt");
        (rows > rows_before_restart).then_some(rows)
    });
    assert!(
        grew > rows_before_restart,
        "no heartbeat fired after the restart ({rows_before_restart} rows)"
    );

    client.request(
        "k1",
        &json!({ "type": "kill", "activeSessionId": active_id }),
    );
}

/// A heartbeat whose time passes while no daemon runs is fired once by the next daemon,
/// which wakes its idle session for it, and its next run is counted from that fire (the
/// store's rule for a late job: no fire per missed interval).
#[test]
fn a_heartbeat_that_came_due_while_the_daemon_was_down_fires_once_at_the_next_boot() {
    let root = tempfile::TempDir::new().expect("temp dir");
    let dir = root.path().to_path_buf();
    let agent_dir = dir.join("agent");
    let socket = dir.join("late.sock");
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [{ "text": "the lane works on" }], "repeatLastResponse": true })
            .to_string(),
    )
    .expect("write faux script");

    let mut supervisor = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let created = client.request(
        "c1",
        &json!({
            "type": "create",
            "name": "late-lane",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let active_id = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let session_file = sessions.join(format!("{session_id}.jsonl"));
    let heartbeat = client.request(
        "hb",
        &json!({
            "type": "heartbeat_set",
            "activeSessionId": active_id,
            "schedule": "every 10s",
            "prompt": "liveness ping",
        }),
    );
    assert_eq!(
        heartbeat["success"], true,
        "heartbeat_set failed: {heartbeat}"
    );
    // The session's own record of its job: when it next runs.
    let jobs_file = agent_dir
        .join("session-artifacts")
        .join(&session_id)
        .join("scheduled-jobs.json");
    let next_run = || {
        pa_core::cron::store::read_scheduled_jobs_artifact(&jobs_file)
            .first()
            .and_then(|job| job.next_run_at.as_deref())
            .and_then(pa_daemon::util::iso_to_unix_ms)
    };
    // One fire has landed and the session is idle again, with its next run recorded.
    let rows_before = wait_until(Duration::from_secs(30), || {
        let rows = session_rows_containing(&session_file, "heartbeat_prompt");
        (rows > 0).then_some(rows)
    });
    let journal = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
        .join(format!("{active_id}.recovery.jsonl"));
    let due = wait_until(Duration::from_secs(30), || {
        (!pa_daemon::journal::WorkerRecoveryJournal::read_interrupted(&journal))
            .then(next_run)
            .flatten()
            .filter(|due| *due > pa_daemon::util::now_ms())
    });

    drop(client);
    let _ = supervisor.child.kill();
    let _ = supervisor.child.wait();
    drop(supervisor);
    std::fs::remove_file(&socket).ok();
    // No daemon runs while the job's time passes.
    wait_until(Duration::from_secs(30), || {
        (pa_daemon::util::now_ms() > due).then_some(())
    });
    assert_eq!(
        (
            session_rows_containing(&session_file, "heartbeat_prompt"),
            next_run()
        ),
        (rows_before, Some(due)),
        "nothing fired, and nothing moved the job, while the daemon was down"
    );
    let booted = pa_daemon::util::now_ms();
    let _supervisor = spawn_daemon(&socket, &agent_dir);
    wait_until(Duration::from_secs(60), || {
        (session_rows_containing(&session_file, "heartbeat_prompt") > rows_before).then_some(())
    });
    let rescheduled = wait_until(Duration::from_secs(30), || {
        next_run().filter(|next| *next != due)
    });
    assert!(
        rescheduled >= booted + 10_000,
        "the late job's next run ({rescheduled}) is a whole interval after the boot ({booted}), \
         not one of the runs missed since {due}"
    );

    let mut client = Client::connect(&socket);
    client.request(
        "k1",
        &json!({ "type": "kill", "activeSessionId": active_id }),
    );
}

/// Boot-time wake vs the storm gates (the ~200-agent boot crash class): the session the
/// boot REVIVES (it was mid-turn) fires its due job, the KILLED sibling never resurrects
/// (#2592/#2642 gates hold).
#[test]
fn a_boot_fires_the_revived_session_due_job_and_never_resurrects_the_killed_sibling() {
    let root = tempfile::TempDir::new().expect("temp dir");
    let dir = root.path().to_path_buf();
    let agent_dir = dir.join("agent");
    let socket = dir.join("storm.sock");
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                // Every turn is slow enough for the daemon to die inside one.
                { "text": "the lane works on", "delayMs": 3000 },
            ],
            "repeatLastResponse": true,
        })
        .to_string(),
    )
    .expect("write faux script");

    let mut supervisor = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let mut created_ids = Vec::new();
    for name in ["wake-lane", "dead-lane"] {
        let created = client.request(
            name,
            &json!({
                "type": "create",
                "name": name,
                "config": {
                    "cwd": dir.to_string_lossy(),
                    "sessionDir": sessions.to_string_lossy(),
                    "script": script.to_string_lossy(),
                },
            }),
        );
        assert_eq!(created["success"], true, "create failed: {created}");
        created_ids.push((
            created["data"]["id"]
                .as_str()
                .or_else(|| created["data"]["activeSessionId"].as_str())
                .expect("active id")
                .to_string(),
            created["data"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string(),
        ));
    }
    let (wake_active, wake_session) = created_ids[0].clone();
    let (dead_active, dead_session) = created_ids[1].clone();
    let wake_file = agent_dir
        .join("sessions")
        .join(format!("{wake_session}.jsonl"));

    // The wake lane carries the due heartbeat; the dead lane is wire-killed (its
    // jobs cancel and its file archives — the stop gates).
    let heartbeat = client.request(
        "hb",
        &json!({
            "type": "heartbeat_set",
            "activeSessionId": wake_active,
            "schedule": "every 10s",
            "prompt": "liveness ping",
        }),
    );
    assert_eq!(
        heartbeat["success"], true,
        "heartbeat_set failed: {heartbeat}"
    );
    let killed = client.request(
        "kill",
        &json!({ "type": "kill", "activeSessionId": dead_active }),
    );
    assert_eq!(killed["success"], true, "kill failed: {killed}");

    wait_until(Duration::from_secs(20), || {
        (session_rows_containing(&wake_file, "heartbeat_prompt") > 0).then_some(())
    });
    // The wake lane is mid-turn when the daemon dies (the journal's busy record is the
    // durable evidence), so the boot owes it a revival.
    let prompted = client.request(
        "busy",
        &json!({ "type": "prompt", "activeSessionId": wake_active, "message": "in flight" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    let journal = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
        .join(format!("{wake_active}.recovery.jsonl"));
    wait_until(Duration::from_secs(20), || {
        pa_daemon::journal::WorkerRecoveryJournal::read_interrupted(&journal).then_some(())
    });

    // The daemon dies hard, both lanes' sessions with it, and relaunches: the wake lane
    // is revived, the killed lane's stop finishes at the boot scan instead of resurrecting.
    drop(client);
    let _ = supervisor.child.kill();
    let _ = supervisor.child.wait();
    drop(supervisor);
    // Nothing writes the session file between the two daemons.
    let rows_before_restart = session_rows_containing(&wake_file, "heartbeat_prompt");
    std::fs::remove_file(&socket).ok();
    let _supervisor = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    wait_until(Duration::from_secs(30), || {
        client
            .listed_sessions()
            .iter()
            .any(|(active, id)| *active == wake_active || *id == wake_session)
            .then_some(())
    });

    // THE STORM GATE: the killed lane never resurrects across the boot window,
    // while the revived session's due fire keeps landing.
    let window = Instant::now() + Duration::from_secs(20);
    while Instant::now() < window {
        let roster = client.listed_sessions();
        assert!(
            !roster
                .iter()
                .any(|(active, id)| *active == dead_active || *id == dead_session),
            "the killed session resurrected at boot: {roster:?}"
        );
    }
    let grew = wait_until(Duration::from_secs(30), || {
        let rows = session_rows_containing(&wake_file, "heartbeat_prompt");
        (rows > rows_before_restart).then_some(rows)
    });
    assert!(
        grew > rows_before_restart,
        "no heartbeat fired after the restart ({rows_before_restart} rows)"
    );

    client.request(
        "k1",
        &json!({ "type": "kill", "activeSessionId": wake_active }),
    );
}

fn session_rows_containing(session_file: &Path, needle: &str) -> usize {
    common::read_transcript(&session_file).map_or(0, |content| {
        content.lines().filter(|line| line.contains(needle)).count()
    })
}
