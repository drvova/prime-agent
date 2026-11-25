//! Graceful daemon shutdown regressions: an interrupted turn continues after
//! the restart, a lost shutdown verdict never archives the session, and a
//! revived user-parked queue stays parked.
use super::*;

/// The first tool's delay outlasts the shutdown or abort that lands mid-turn.
fn busy_turn() -> Value {
    json!({
        "text": "busy-turn",
        "toolCalls": [
            { "toolCallId": "call-1", "toolName": "bash", "args": {}, "result": "listed", "delayMs": 5_000 },
            { "toolCallId": "call-2", "toolName": "bash", "args": {}, "result": "unfinished" },
        ],
    })
}

/// The scripted session whose first turn is [`busy_turn`].
fn busy_script() -> Value {
    json!({ "responses": [busy_turn(), { "text": "next-turn", "delayMs": 10 }] })
}

/// Boot a supervisor in `dir`, create one session from `script`, and prompt
/// it: (daemon, client, session id, session file).
fn boot_with_prompted_session(dir: &Path, script: &Value) -> (Daemon, Client, String, String) {
    let sessions_dir = dir.join("agent").join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let daemon = spawn_supervisor(&dir.join("daemon.sock"), &dir.join("agent"));
    wait_socket_ready(&dir.join("daemon.sock"));
    let (mut client, _hello) = Client::connect(&dir.join("daemon.sock"));
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).expect("write script");
    let created = call(
        &mut client,
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let session = created["data"]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let session_file = created["data"]["sessionFile"]
        .as_str()
        .expect("session file")
        .to_string();
    let prompted = call(
        &mut client,
        &json!({ "type": "prompt", "activeSessionId": session, "message": "go" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    (daemon, client, session, session_file)
}

fn call(client: &mut Client, command: &Value) -> Value {
    client.send_command("cmd", command);
    client.read_response("cmd")
}

fn rows(session_file: &str) -> Vec<Value> {
    crate::common::read_transcript(&session_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn message_rows(session_file: &str) -> Vec<Value> {
    rows(session_file)
        .into_iter()
        .filter(|row| row["type"] == "message")
        .collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_assistant_row(session_file: &str) {
    wait_for("the turn never started", || {
        rows(session_file)
            .iter()
            .any(|row| row["message"]["role"] == "assistant")
    });
}

fn shut_down(client: &mut Client, daemon: &mut Daemon) {
    let shutdown = call(client, &json!({ "type": "shutdown" }));
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");
    wait_for("the supervisor never exited", || {
        daemon.child.try_wait().expect("try wait").is_some()
    });
}

#[test]
fn graceful_shutdown_continues_the_aborted_turn_after_restart() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let (mut daemon, mut client, _session, session_file) =
        boot_with_prompted_session(dir.path(), &busy_script());
    // The shutdown lands during call-1, so call-2 never starts and the turn
    // ends aborted, not completed.
    wait_for_assistant_row(&session_file);
    shut_down(&mut client, &mut daemon);

    let mut daemon = spawn_supervisor(&dir.path().join("daemon.sock"), &dir.path().join("agent"));
    wait_socket_ready(&dir.path().join("daemon.sock"));
    // The revived worker runs the continuation prompt: a second user row
    // followed by an assistant reply that was not aborted.
    wait_for("the interrupted turn never continued", || {
        let rows = rows(&session_file);
        let Some(continuation) = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row["message"]["role"] == "user")
            .nth(1)
            .map(|(at, _)| at)
        else {
            return false;
        };
        rows[continuation + 1..].iter().any(|row| {
            row["message"]["role"] == "assistant" && row["message"]["stopReason"] != "aborted"
        })
    });
    let (mut client, _hello) = Client::connect(&dir.path().join("daemon.sock"));
    shut_down(&mut client, &mut daemon);
}

/// The daemon-wide shutdown persists the resumable stop tombstone before it
/// tells the session, so a boot that finishes the stop after the verdict was
/// lost keeps the session.
#[test]
fn a_lost_shutdown_verdict_never_archives_the_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let (socket, agent_dir) = (dir.path().join("daemon.sock"), dir.path().join("agent"));
    let (mut daemon, mut client, session, session_file) =
        boot_with_prompted_session(dir.path(), &busy_script());
    wait_for_busy_journal_evidence(&agent_dir, &socket, &session);
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket);
    let descriptor_path = descriptor_dir.join(format!("{session}.json"));
    let tombstone = || {
        std::fs::read_to_string(&descriptor_path)
            .ok()
            .and_then(|content| serde_json::from_str::<Value>(&content).ok())
            .filter(|descriptor| descriptor["stopRequestedAt"].is_string())
    };

    // The daemon dies inside the stop pass, the session it hosts with it: the
    // kill lands as soon as the tombstone is on disk, which is before the
    // session has been told to stop.
    client.send_command("shutdown", &json!({ "type": "shutdown" }));
    let deadline = Instant::now() + Duration::from_secs(30);
    while tombstone().is_none() {
        assert!(
            Instant::now() < deadline,
            "the shutdown never persisted its stop tombstone"
        );
        std::thread::yield_now();
    }
    daemon.child.kill().expect("kill supervisor");
    daemon.child.wait().expect("reap supervisor");
    // The state under test is the one a death before the verdict leaves. A
    // session that settled its shutdown inside the kill's latency wrote the
    // verdict after all: drop it, so every run boots from the same state.
    let journal_path = descriptor_dir.join(format!("{session}.recovery.jsonl"));
    let journal = std::fs::read_to_string(&journal_path).expect("recovery journal");
    let unsettled: Vec<&str> = journal
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .is_ok_and(|record| record["operation"] != "shutdown")
        })
        .collect();
    std::fs::write(&journal_path, unsettled.join("\n") + "\n").expect("journal without a verdict");
    assert_eq!(
        tombstone().map(|descriptor| descriptor["archiveOnStop"].clone()),
        Some(json!(false)),
        "the daemon died holding the resumable stop tombstone"
    );

    // The next boot finishes the tombstoned stop (the descriptor goes away).
    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    wait_for("the tombstoned stop never finished", || {
        !descriptor_path.exists()
    });
    let archived = rows(&session_file)
        .iter()
        .any(|row| row["type"] == "session_state" && row["state"]["status"] == "archived");
    assert!(!archived, "a lost shutdown verdict archived the session");
    let (mut client, _hello) = Client::connect(&socket);
    shut_down(&mut client, &mut daemon);
}

/// A crash after a user abort revives the session from the abort-settle
/// journal row; its restored queue keeps the suspension instead of running
/// the parked prompt.
#[test]
fn a_revived_user_parked_queue_stays_parked() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let (socket, agent_dir) = (dir.path().join("daemon.sock"), dir.path().join("agent"));
    let (mut daemon, mut client, session, session_file) =
        boot_with_prompted_session(dir.path(), &busy_script());
    wait_for_assistant_row(&session_file);
    let queued = call(
        &mut client,
        &json!({ "type": "follow_up", "activeSessionId": session, "message": "remain parked" }),
    );
    assert_eq!(queued["success"], true, "follow_up failed: {queued}");
    let aborted = call(
        &mut client,
        &json!({ "type": "abort", "activeSessionId": session }),
    );
    assert_eq!(aborted["success"], true, "abort failed: {aborted}");
    let get_state = json!({ "type": "get_state", "activeSessionId": session });
    wait_for("the aborted turn never settled", || {
        call(&mut client, &get_state)["data"]["isStreaming"] == false
    });
    let parked_rows = message_rows(&session_file);

    // kill -9 the daemon: the session dies with it, idle, and the
    // abort-settle row is the last thing in its journal.
    daemon.child.kill().expect("kill supervisor");
    daemon.child.wait().expect("reap supervisor");

    let restart_before = pa_daemon::util::now_iso();
    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let log_path = pa_daemon::paths::daemon_log_path(&socket, &agent_dir);
    wait_for("the parked session was not revived", || {
        distinct(workers_registered_since(&log_path, &restart_before)) == vec![session.clone()]
    });
    let (mut client, _hello) = Client::connect(&socket);
    let queue = call(
        &mut client,
        &json!({ "type": "get_queue", "activeSessionId": session }),
    );
    assert_eq!(
        queue["data"]["followUp"],
        json!(["remain parked"]),
        "{queue}"
    );
    let state = call(&mut client, &get_state);
    assert_eq!(state["data"]["isStreaming"], false, "{state}");
    assert_eq!(
        message_rows(&session_file),
        parked_rows,
        "the revived session ran the parked input without a resume"
    );
    shut_down(&mut client, &mut daemon);
}
