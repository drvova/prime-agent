//! Reliability parity on the runtime daemon (phase 7c), ported from the old
//! daemon's `session_crash_e2e`, `orphan_journal_e2e` and
//! `supervisor_restart_e2e` revival onto wire2, with their expectations:
//!
//! - a panic in one session's turn fails that turn with the crash's reason,
//!   is logged and announced (`agent/<id>/crashed`, then `/recovered`), and
//!   the session heals from its transcript while another session runs on;
//! - a session that keeps crashing is given up after the sixth crash in a
//!   row (`agent/<id>/failed`) and closed;
//! - a cell that ignores its interrupt is reported as stuck and its session
//!   rebuilt, and the model is told once that its kernel definitions are
//!   gone;
//! - a runtime daemon killed mid-turn is restarted onto the same agent dir:
//!   the program the cut-off cell left running is reaped, the cell is
//!   answered once as of unknown outcome, and the turn continues behind the
//!   interrupted-turn notice without its prompt being repeated.
//!
//! The crashes use the debug-only seams: the faux script's `panicOn` and the
//! kernel's `(%test-block marker)`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::turns::{cell, Attached};
use super::{ok, turn_rows, Bed, Client};

/// How long a test waits for the daemon's state to change before it fails.
const STATE_WAIT: Duration = Duration::from_secs(60);

/// A plain scripted session whose turn panics with `panic` on the prompt
/// `crash now`.
fn crashable(panic: &str) -> Value {
    json!({
        "responses": ["answered"],
        "repeatLastResponse": true,
        "panicOn": { "message": "crash now", "panic": panic },
    })
}

/// The runtime daemon's crash log records.
fn crash_log(bed: &Bed) -> Vec<Value> {
    let agent_dir = bed.agent_dir();
    let state = pa_core::agent_state::StateDatabase::open(&pa_core::session::store::database_path(
        &agent_dir,
    ))
    .expect("open the agent database");
    state
        .crash_records(
            &agent_dir
                .join("runtime-daemon")
                .join("session-crashes.jsonl"),
        )
        .expect("crash records")
}

/// How many fault rows (`session_fault`) the session's transcript keeps.
async fn fault_rows(session: &mut Attached) -> usize {
    custom_rows(session)
        .await
        .iter()
        .filter(|row| *row == "session_fault")
        .count()
}

/// The `customType`s of the custom rows of the session's transcript, in
/// order.
async fn custom_rows(session: &mut Attached) -> Vec<String> {
    let page = session.query("transcript.page").await;
    page["entries"]
        .as_array()
        .expect("a page of entries")
        .iter()
        .filter(|row| row["type"] == "custom_message")
        .filter_map(|row| row["customType"].as_str().map(str::to_string))
        .collect()
}

/// The text of the session's tool results, in order.
async fn tool_results(session: &mut Attached) -> Vec<String> {
    let page = session.query("transcript.page").await;
    turn_rows(&page)
        .into_iter()
        .filter(|(_, role, _)| role.as_deref() == Some("toolResult"))
        .filter_map(|(_, _, text)| text)
        .collect()
}

/// A crash's fields a test compares (its time and thread vary).
fn crash_fields(crash: &Value) -> Value {
    json!({
        "kind": crash["kind"],
        "message": crash["message"],
        "state": crash["state"],
    })
}

/// Waits until `path` exists.
fn await_file(path: &Path) {
    let deadline = Instant::now() + STATE_WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A cell that enters a native call no interrupt reaches: it writes
/// `marker` once it is inside, and returns once the marker is gone.
fn wedged_cell(marker: &Path) -> String {
    format!(
        "(%test-block {})",
        serde_json::to_string(&marker.to_string_lossy()).expect("a Scheme string literal")
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_in_one_session_fails_its_turn_is_reported_and_heals_while_another_runs() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&crashable("the turn blew up"));
    let mut crashing = Attached::open(&bed, &cwd).await;
    let mut other = Attached::open(&bed, &cwd).await;
    // A first turn saves the transcript the crash is kept in.
    let first = crashing.publish("hello", None).await;
    crashing.done(&first["task"]).await;

    let crashed = crashing.publish("crash now", None).await;
    let failed = crashing.done(&crashed["task"]).await;
    crashing
        .until(|session| !session.payloads("recovered").is_empty())
        .await;
    let beside = other.publish("still here", None).await;
    let beside = other.done(&beside["task"]).await;
    let healed = crashing.publish("after the crash", None).await;
    let healed = crashing.done(&healed["task"]).await;

    let reason = failed["error"].as_str().unwrap_or_default();
    let snapshot = crashing.query("session.snapshot").await;
    let logged: Vec<Value> = crash_log(&bed)
        .iter()
        .map(|record| json!([record["sessionId"], record["kind"], record["message"]]))
        .collect();
    assert_eq!(
        (
            failed["outcome"].clone(),
            reason.starts_with("Session crashed (panic): the turn blew up at "),
            crashing
                .payloads("crashed")
                .iter()
                .map(|crash| json!([crash["type"], crash["kind"], crash["message"]]))
                .collect::<Vec<_>>(),
            crashing.payloads("recovered"),
            crash_fields(&snapshot["lastCrash"]),
            logged,
            fault_rows(&mut crashing).await,
            healed["outcome"].clone(),
            (beside["outcome"].clone(), other.payloads("crashed").len()),
        ),
        (
            json!("failed"),
            true,
            vec![json!(["session_crashed", "panic", "the turn blew up"])],
            vec![json!({
                "type": "session_recovered",
                "failures": 1,
                "queuedInputSuspended": false,
            })],
            json!({ "kind": "panic", "message": "the turn blew up", "state": "recovered" }),
            vec![json!([crashing.id, "panic", "the turn blew up"])],
            2,
            json!("done"),
            (json!("done"), 0),
        ),
        "the failed turn's reason: {reason}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_keeps_crashing_is_given_up_on_and_its_client_is_told() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&crashable("crashes every time"));
    let mut session = Attached::open(&bed, &cwd).await;
    // A first turn saves the transcript the give-up is kept in.
    let first = session.publish("hello", None).await;
    session.done(&first["task"]).await;

    let mut tasks = Vec::new();
    for _ in 0..6 {
        tasks.push(session.publish("crash now", None).await["task"].clone());
    }
    session
        .until(|session| !session.payloads("failed").is_empty())
        .await;
    let mut outcomes = Vec::new();
    for task in &tasks {
        outcomes.push(session.done(task).await["outcome"].clone());
    }
    // The give-up closes the session: it is no longer open.
    let (mut lister, _) = Client::connect(&bed.runtime_socket()).await;
    let deadline = Instant::now() + STATE_WAIT;
    let open = loop {
        let sessions = ok(lister.query("sessions.list", json!({})).await);
        let open = sessions["sessions"]
            .as_array()
            .expect("a session list")
            .iter()
            .find(|row| row["session"] == session.id.as_str())
            .map(|row| row["open"].clone());
        if open != Some(json!(true)) || Instant::now() >= deadline {
            break open;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let snapshot = session.query("session.snapshot").await;
    assert_eq!(
        (
            outcomes,
            session.payloads("crashed").len(),
            session.payloads("recovered").len(),
            session.payloads("failed"),
            open,
            crash_fields(&snapshot["lastCrash"]),
            crash_log(&bed).len(),
            fault_rows(&mut session).await,
        ),
        (
            vec![json!("failed"); 6],
            6,
            5,
            vec![json!({ "type": "session_failed", "failures": 6 })],
            Some(json!(false)),
            json!({ "kind": "panic", "message": "crashes every time", "state": "failed" }),
            6,
            // Each crash's row and what became of the session: five
            // recoveries, then the give-up.
            12,
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_ignores_its_interrupt_is_reported_as_stuck_and_its_session_rebuilt() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let marker = bed.dir.path().join("cell-blocked");
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": [
            cell("Blocking.", &wedged_cell(&marker)),
            "after the rebuild",
        ]}),
        &[
            ("PRIME_AGENT_TEST_BLOCKING_CELL_SEAM", "1"),
            ("PRIME_AGENT_STUCK_CELL_GRACE_MS", "300"),
        ],
    );
    let mut session = Attached::open(&bed, &cwd).await;

    let wedged = session.publish("block the kernel", None).await;
    await_file(&marker);
    // The stop reaches the cell's signal, never the native call: after the
    // grace the cell is reported stuck and the session rebuilt.
    session
        .call("session.abort_and_send_queued", json!({}))
        .await;
    session
        .until(|session| !session.payloads("recovered").is_empty())
        .await;
    session.done(&wedged["task"]).await;
    // The abandoned thread is let go; the rebuilt session's turn runs the
    // cell again on its new kernel, which the test lets through too.
    std::fs::remove_file(&marker).unwrap();
    let after = session.publish("go on", None).await;
    await_file(&marker);
    std::fs::remove_file(&marker).unwrap();
    let after = session.done(&after["task"]).await;

    let snapshot = session.query("session.snapshot").await;
    let rows = custom_rows(&mut session).await;
    assert_eq!(
        (
            session
                .payloads("crashed")
                .iter()
                .map(|crash| crash["kind"].clone())
                .collect::<Vec<_>>(),
            snapshot["lastCrash"]["kind"].clone(),
            snapshot["lastCrash"]["state"].clone(),
            after["outcome"].clone(),
            rows.iter()
                .filter(|row| *row == "ipython_state_restored")
                .count(),
            rows.iter().filter(|row| *row == "session_fault").count(),
        ),
        (
            vec![json!("stuck")],
            json!("stuck"),
            json!("recovered"),
            json!("done"),
            1,
            2,
        )
    );
}

/// Whether process `pid` still runs (an exited one nobody has reaped yet
/// does not: the container's init may never reap it).
fn alive(pid: i32) -> bool {
    let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'))
    });
    !zombie && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Waits until process `pid` has exited; whether it did within the wait.
fn exits(pid: i32) -> bool {
    let deadline = Instant::now() + STATE_WAIT;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_cut_off_by_the_daemons_death_is_continued_and_its_cell_answered_once() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let orphan_pid: PathBuf = cwd.join("orphan.pid");
    let mut first = bed.runtime_daemon(&json!({ "responses": [cell(
        "Starting the long command.",
        "(bash \"/bin/sleep 600 & echo $! > orphan.pid; wait\")",
    )]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let id = session.id.clone();
    let cut_off = session.publish("start the long command", None).await;
    await_file(&orphan_pid);
    let pid: i32 = loop {
        if let Ok(pid) = std::fs::read_to_string(&orphan_pid)
            .unwrap_or_default()
            .trim()
            .parse()
        {
            break pid;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    first.0.kill().expect("kill the runtime daemon");
    first.0.wait().expect("reap the runtime daemon");
    assert!(alive(pid), "the daemon's death leaves its program running");
    drop(session);

    let _second = bed.runtime_daemon(&json!({ "responses": ["continued after the restart"] }));
    let mut session = Attached::attach(&bed, &id).await;
    let done = session.done(&cut_off["task"]).await;
    session
        .until(|session| !session.payloads("recovered").is_empty())
        .await;

    let texts = session.texts().await;
    let results = tool_results(&mut session).await;
    let snapshot = session.query("session.snapshot").await;
    assert_eq!(
        (
            done["outcome"].clone(),
            exits(pid),
            session
                .payloads("crashed")
                .iter()
                .map(|crash| json!([crash["kind"], crash["message"]]))
                .collect::<Vec<_>>(),
            texts
                .iter()
                .filter(|text| *text == "start the long command")
                .count(),
            texts
                .iter()
                .filter(|text| text.starts_with("[turn-interrupted]"))
                .count(),
            texts.last().cloned(),
            results
                .iter()
                .map(|text| (
                    text.starts_with("[outcome-unknown] The daemon hosting this session stopped"),
                    text.contains("1 program the session's commands left running was stopped")
                ))
                .collect::<Vec<_>>(),
            crash_fields(&snapshot["lastCrash"]),
        ),
        (
            json!("done"),
            true,
            vec![json!([
                "daemon_restart",
                "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn continues from what was saved"
            ])],
            1,
            1,
            Some("continued after the restart".to_string()),
            vec![(true, true)],
            json!({
                "kind": "daemon_restart",
                "message": "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn continues from what was saved",
                "state": "recovered",
            }),
        ),
        "the transcript's texts: {texts:?}"
    );
}
