//! Cross-session features on the runtime daemon (phase 7e), ported from the
//! old daemon's `rlm_children_e2e`, `rlm_quiescence_barrier_e2e`,
//! `agent_message_wait_e2e` and `agent_family_e2e` onto wire2, with their
//! expectations. Every family is made by the sessions' own kernels:
//!
//! - a spawned child is recorded in the old daemon's ledger and display
//!   file, its parent's task ends only once the child has settled, the
//!   parent's context tree and breakdown show it, and after the daemon is
//!   killed and started again the reopened parent finds it (roster, observe,
//!   delete, whose tombstone the ledger records);
//! - a child that settles, or messages its parent, while the parent is idle
//!   wakes the parent for a turn over the row, with no prompt of its own;
//! - `session.wait_idle` answers only once the subagents below the session
//!   have settled and the notice turn their settle woke it for has run, not
//!   when its own task ends;
//! - `agent_message.wait` ends on the awaited sibling's message, answers a
//!   wait that would close a cycle at once, and times out;
//! - a kernel timer source keeps firing for its subscription after the
//!   daemon restarts;
//! - a stop cuts off a subagent's running turn, as the old worker closes its
//!   children at a daemon-wide stop, so the daemon exits at once;
//! - `sessions.list` lists a running subagent with its place in its family,
//!   as the old supervisor listed a child's session;
//! - a stop that deletes a running subagent (the old `kill
//!   {rlmLedgerDelete}`) writes its ledger tombstone and ends it, and its
//!   retry writes no second one; a plain stop ends it, keeps its edge and
//!   writes its transcript's catalog sidecar, as the old child worker's
//!   lease release did;
//! - `session.wait_idle` waits on a subagent's session too.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pa_types::wire2::{Outcome, ServerFrame, Since};
use serde_json::{json, Value};

use super::schedules::until;
use super::turns::cell;
use super::{ok, run_turn, Bed, Client, Daemon};

/// How long a test waits for a record or a state change before it fails.
const WAIT: Duration = Duration::from_secs(90);

/// A form that writes `call`'s value as JSON to `<dir>/<name>.json`, or its
/// error to `<name>.error`, with the call's wall time (`elapsedMs`) beside a
/// hash result.
fn record(call: &str, name: &str, dir: &Path) -> String {
    let path = dir.join(format!("{name}.json")).display().to_string();
    let error = dir.join(format!("{name}.error")).display().to_string();
    format!(
        r"(with-handler
  (lambda (e)
    (fs-write {error:?} (to-string e))
    (error (to-string e)))
  (let* ([started (current-milliseconds)]
         [result {call}]
         [result (if (hash? result) (hash-insert result 'elapsedMs (- (current-milliseconds) started)) result)])
    (fs-write {path:?} (value->jsexpr-string result))))
"
    )
}

/// The recorded value once it lands (an `.error` record fails the test).
async fn recorded(dir: &Path, name: &str) -> Value {
    until(WAIT, name, async || {
        if let Ok(error) = std::fs::read_to_string(dir.join(format!("{name}.error"))) {
            panic!("{name} raised: {error}");
        }
        let content = std::fs::read_to_string(dir.join(format!("{name}.json"))).ok()?;
        Some(serde_json::from_str(&content).expect("a JSON record"))
    })
    .await
}

/// A form that waits until `marker` exists (the readiness a sibling signals).
fn await_marker(marker: &Path) -> String {
    format!(
        "(bash {:?})",
        format!("until [ -f '{}' ]; do sleep 0.05; done", marker.display())
    )
}

/// A form that writes `marker`.
fn mark(marker: &Path) -> String {
    format!("(fs-write {:?} \"ready\")", marker.display().to_string())
}

/// Session `id`'s transcript, as one JSON string.
async fn transcript_text(client: &mut Client, id: &str) -> String {
    ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await)
    .to_string()
}

/// The records of the spawn ledger under `agent_dir`, as `(op, childId)`.
fn ledger_ops(agent_dir: &Path) -> Vec<(String, String)> {
    let Ok(dir) = std::fs::read_dir(agent_dir.join("rlm-ledger")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record["op"] != "meta")
                .map(|record| {
                    (
                        record["op"].as_str().unwrap_or_default().to_string(),
                        record["childId"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The display file's status of child `child_id` of session `parent`.
fn display_status(agent_dir: &Path, parent: &str, child_id: &str) -> Option<String> {
    let path: PathBuf = agent_dir
        .join("session-artifacts")
        .join(parent)
        .join(child_id)
        .join("rlm-subagent.json");
    let display: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    display["status"].as_str().map(str::to_string)
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "the family's life reads in order: spawn, the records, the restart, the reopen, the delete"
)]
async fn a_childs_family_is_recorded_shown_and_reopened_after_a_restart() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let spawn = format!(
        "{}\n(rlm-collect \"alpha\" #:timeout-ms 60000)",
        record(
            r#"(rlm-spawn "count the files" #:name "alpha")"#,
            "spawn",
            &records,
        )
    );
    let children =
        json!({ "alpha": { "responses": ["alpha answer"], "repeatLastResponse": true } });
    let daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawn), "spawned"],
        "repeatLastResponse": true,
        "children": children,
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "delegate").await;

    // The parent's collect returned once its child had settled: the child's
    // display entry already reads completed, and its edge is the ledger's.
    let child_id = recorded(&records, "spawn").await["rlm_child_id"]
        .as_str()
        .expect("a child id")
        .to_string();
    let agent_dir = bed.agent_dir();
    assert_eq!(
        (
            ledger_ops(&agent_dir),
            display_status(&agent_dir, &id, &child_id)
        ),
        (
            vec![("spawn".to_string(), child_id.clone())],
            Some("completed".to_string())
        )
    );
    let tree = ok(client
        .query("session.context_tree", json!({ "session": id }))
        .await);
    let breakdown = ok(client
        .query("session.breakdown", json!({ "session": id }))
        .await);
    assert_eq!(
        (
            tree["children"][0]["id"].clone(),
            tree["children"][0]["label"].clone(),
            tree["children"][0]["status"].clone(),
            breakdown["agents"][0]["label"].clone(),
            breakdown["agents"][0]["depth"].clone(),
        ),
        (
            json!(child_id),
            json!("alpha"),
            json!("completed"),
            json!("alpha"),
            json!(1)
        )
    );
    drop(client);
    // Killed the hard way, as a crash would.
    drop(daemon);

    // The next daemon opens the parent again: its child comes back with it.
    let observe = record("(agent-roster)", "observe", &records);
    let delete = record(r#"(rlm-delete "alpha")"#, "delete", &records);
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("observing", &format!("{observe}\n{delete}")), "deleted"],
        "repeatLastResponse": true,
        "children": children,
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    ok(client
        .call("session.create", json!({ "session": id }))
        .await);
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    // Its tree answers once its engine is built, and holds the child once
    // the child is open again.
    until(WAIT, "the reopened child", async || {
        match client
            .query("session.context_tree", json!({ "session": id }))
            .await
        {
            Outcome::Ok { result } => {
                (result["children"][0]["id"] == json!(child_id)).then_some(())
            }
            Outcome::Error { .. } => None,
        }
    })
    .await;
    run_turn(&mut client, &id, "check on alpha").await;
    let observed = recorded(&records, "observe").await;
    assert_eq!(
        (
            observed["agents"][0]["sessionName"].clone(),
            observed["agents"][0]["relationship"].clone(),
            observed["agents"][0]["rlmChildId"].clone(),
        ),
        (json!("alpha"), json!("child"), json!(child_id))
    );
    recorded(&records, "delete").await;
    assert_eq!(
        (
            ledger_ops(&agent_dir),
            display_status(&agent_dir, &id, &child_id)
        ),
        (
            vec![
                ("spawn".to_string(), child_id.clone()),
                ("delete".to_string(), child_id.clone())
            ],
            Some("deleted".to_string())
        )
    );
}

/// A row delivered to an idle root runs as its own turn (the old daemon's
/// notice turn): the child is held until the parent's task has ended, and
/// its settle's terminal notice is answered with no prompt sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_child_settling_while_its_parent_is_idle_wakes_the_parent() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let release = records.join("release");
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [
            cell("spawning", r#"(rlm-spawn "wait to be released" #:name "kid")"#),
            "spawn turn done",
            "notice seen",
        ],
        "children": {
            "kid": { "responses": [cell("waiting", &await_marker(&release)), "kid done"] },
        },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    // The parent is idle now: its task ended with the child still held.
    std::fs::write(&release, "go").unwrap();
    until(WAIT, "the parent's notice turn", async || {
        transcript_text(&mut client, &id)
            .await
            .contains("notice seen")
            .then_some(())
    })
    .await;
}

/// An agent message to an idle root wakes it (a wake of the sender's
/// budget): the child messages its parent once the parent's task has ended,
/// and the parent answers it with no prompt sent. The child's reply
/// withholds its terminal notice, so the message is the parent's one row.
/// The delivery is the traffic pane's entry, on the traffic feed as it
/// lands and in what `runtime.traffic` keeps.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_an_idle_parent_wakes_it() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let release = records.join("release");
    let reply = format!(
        "{}\n{}",
        await_marker(&release),
        record(
            r#"(agent-send "kid reply" #:to "parent")"#,
            "send",
            &records,
        )
    );
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [
            cell("spawning", r#"(rlm-spawn "reply when released" #:name "kid")"#),
            "spawn turn done",
            "message seen",
        ],
        "children": { "kid": { "responses": [cell("replying", &reply), "kid done"] } },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    let (mut traffic, _) = Client::connect(&bed.runtime_socket()).await;
    traffic.subscribe("runtime/traffic", Since::Now).await;
    std::fs::write(&release, "go").unwrap();
    let sent = recorded(&records, "send").await;
    assert_eq!(sent["deliveryStatus"], json!("delivered"), "{sent}");
    let kid = ok(client
        .query("session.children", json!({ "session": id }))
        .await)["children"][0]["activeSessionId"]
        .clone();
    let mut fed = match traffic.frame().await {
        pa_types::wire2::ServerFrame::Event { event, .. } => event.event.payload,
        other => panic!("expected the traffic entry, got {other:?}"),
    };
    let mut kept = ok(traffic.query("runtime.traffic", json!({})).await);
    // The time is the delivery's.
    fed["timestampMs"] = json!(0);
    kept["entries"][0]["timestampMs"] = json!(0);
    let entry = json!({
        "rootActiveSessionId": id, "from": "kid", "fromActiveSessionId": kid, "to": id,
        "toActiveSessionId": id, "relationship": "child", "kind": "direct",
        "outcome": "delivered", "chars": 9, "preview": "kid reply", "timestampMs": 0,
    });
    assert_eq!((fed, kept), (entry.clone(), json!({ "entries": [entry] })));
    until(WAIT, "the parent's message turn", async || {
        transcript_text(&mut client, &id)
            .await
            .contains("message seen")
            .then_some(())
    })
    .await;
}

/// `session.wait_idle` waits for the subagents too (the old daemon's
/// `wait_for_idle` with `waitForRlmQuiescence`): the parent's task ends while
/// its child's held turn still runs, and the wait answers only once the child
/// has settled and the notice turn its settle woke the parent for has run.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_idle_waits_for_the_subagents_to_settle() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let spawn = record(
        r#"(rlm-spawn "run the lane task" #:name "kid")"#,
        "spawn",
        &records,
    );
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawn), "spawn turn done", "notice seen"],
        "children": { "kid": { "responses": [{ "text": "kid done", "delayMs": 5_000 }] } },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    recorded(&records, "spawn").await;
    let status = async |client: &mut Client| {
        ok(client
            .query("session.context_tree", json!({ "session": id }))
            .await)["children"][0]["status"]
            .clone()
    };
    // The parent's task is done; its child's held turn runs on.
    assert_eq!(status(&mut client).await, json!("running"));
    // The answer also carries the run's accounting; the wait is what counts.
    assert_eq!(
        ok(client
            .call("session.wait_idle", json!({ "session": id }))
            .await)["session"],
        json!(id)
    );
    assert_eq!(
        (
            status(&mut client).await,
            transcript_text(&mut client, &id)
                .await
                .contains("notice seen")
        ),
        (json!("completed"), true)
    );
}

/// The drain cuts off a subagent's running turn (the old worker closes its
/// children at a daemon-wide stop): the daemon exits at once, not when the
/// child's held turn would have ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_cuts_off_a_running_subagents_turn() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let spawn = record(r#"(rlm-spawn "hold on" #:name "kid")"#, "spawn", &records);
    let mut daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawn), "spawn turn done"],
        "children": { "kid": { "responses": [{ "text": "kid done", "delayMs": 600_000 }] } },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    recorded(&records, "spawn").await;
    assert_eq!(
        ok(client
            .query("session.context_tree", json!({ "session": id }))
            .await)["children"][0]["status"],
        json!("running")
    );
    ok(client.call("daemon.shutdown", json!({})).await);
    assert_eq!(super::lifecycle::exit_code(&mut daemon), Some(0));
}

/// A daemon whose session `id` has spawned `kid`, whose turn is held: the
/// daemon, its client following the parent, and the kid's handle and id.
async fn held_kid(bed: &Bed) -> (Daemon, Client, String, String, String) {
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let spawn = record(r#"(rlm-spawn "hold on" #:name "kid")"#, "spawn", &records);
    let daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawn), "spawn turn done"],
        "children": { "kid": { "responses": [{ "text": "kid done", "delayMs": 600_000 }] } },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    let handle = recorded(&records, "spawn").await["rlm_child_id"]
        .as_str()
        .expect("a child id")
        .to_string();
    let children = ok(client
        .query("session.children", json!({ "session": id }))
        .await);
    let kid = children["children"][0]["activeSessionId"]
        .as_str()
        .expect("the kid's session")
        .to_string();
    (daemon, client, id, handle, kid)
}

/// `sessions.list` lists a running subagent as the old supervisor's list
/// did its child's session: its row names its handle, its depth and its
/// parent, open or saved alike.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_lists_a_running_subagent_in_its_family() {
    let bed = Bed::new();
    let (_daemon, mut client, id, handle, kid) = held_kid(&bed).await;
    let family = |listed: Value| -> Vec<Value> {
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["rlmChildId"].is_string())
            .map(|row| {
                json!([
                    row["session"],
                    row["rlmChildId"],
                    row["rlmDepth"],
                    row["parentActiveSessionId"],
                    row["sessionName"],
                    row["runtimeKind"]
                ])
            })
            .collect()
    };
    let expected = vec![json!([kid, handle, 1, id, "kid", "subagent"])];
    assert_eq!(
        (
            family(ok(client
                .query("sessions.list", json!({ "open": true }))
                .await)),
            family(ok(client.query("sessions.list", json!({})).await)),
        ),
        (expected.clone(), expected)
    );
}

/// A stop that deletes a running subagent (`rlmLedgerDelete`, the old
/// supervisor's `kill` marker) tombstones its ledger edge and ends its
/// session; a retry finds the boundary standing and writes no second one.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_that_deletes_a_running_subagent_writes_its_boundary_once() {
    let bed = Bed::new();
    let (_daemon, mut client, id, handle, kid) = held_kid(&bed).await;
    let delete = json!({ "session": kid, "rlmLedgerDelete": "user", "rlmChildId": handle });
    let first = ok(client.call("session.stop", delete.clone()).await);
    let retry = ok(client.call("session.stop", delete).await);
    let children = ok(client
        .query("session.children", json!({ "session": id }))
        .await);
    assert_eq!(
        (
            first,
            retry,
            ledger_ops(&bed.agent_dir()),
            children["children"].clone()
        ),
        (
            json!({ "session": kid, "stopped": true }),
            json!({ "session": kid, "stopped": true }),
            vec![
                ("spawn".to_string(), handle.clone()),
                ("delete".to_string(), handle)
            ],
            json!([])
        )
    );
}

/// A plain stop of a running subagent (the old `kill` of its session) ends
/// it and keeps its ledger edge, so it lists as a passive child; its
/// transcript's catalog sidecar is written as it closes.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_stop_of_a_running_subagent_keeps_its_edge() {
    let bed = Bed::new();
    let (_daemon, mut client, id, handle, kid) = held_kid(&bed).await;
    let listed = ok(client.query("sessions.list", json!({ "open": true })).await);
    let file = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == json!(kid))
        .and_then(|row| row["sessionFile"].as_str())
        .map(PathBuf::from)
        .expect("the kid's row");
    let stopped = ok(client.call("session.stop", json!({ "session": kid })).await);
    let children = ok(client
        .query("session.children", json!({ "session": id }))
        .await);
    assert_eq!(
        (
            stopped,
            ledger_ops(&bed.agent_dir()),
            children["children"].clone(),
            file.with_extension("info-cache.json").is_file(),
        ),
        (
            json!({ "session": kid, "stopped": true }),
            vec![("spawn".to_string(), handle)],
            json!([]),
            true
        )
    );
}

/// `session.wait_idle` of a subagent answers once its own work is done, as
/// the old daemon's `wait_for_idle` of a child's session did.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_idle_waits_on_a_subagents_session() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", r#"(rlm-spawn "say done" #:name "kid")"#), "spawned"],
        "children": { "kid": { "responses": ["kid done"] } },
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "spawn the kid").await;
    let children = ok(client
        .query("session.children", json!({ "session": id }))
        .await);
    let kid = children["children"][0]["activeSessionId"].clone();
    let idle = ok(client
        .call("session.wait_idle", json!({ "session": kid }))
        .await);
    assert_eq!(idle["session"], kid);
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "the waits read in order: the family, the roster's wait edge, the cycle, the message, the timeout"
)]
async fn waits_end_on_a_message_answer_a_cycle_at_once_and_time_out() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let (spawned, parked) = (records.join("spawned"), records.join("parked"));
    let body = format!("the contract is ready: {}", "x".repeat(300));
    let parent = format!(
        "(rlm-spawn \"wait for beta\" #:name \"alpha\")\n(rlm-spawn \"talk to alpha\" #:name \"beta\")\n{}\n(rlm-collect #:timeout-ms 120000)",
        mark(&spawned)
    );
    // Alpha parks on beta once both are spawned.
    let alpha = format!(
        "{}\n{}",
        await_marker(&spawned),
        record(
            r#"(event-wait #:sender "beta" #:timeout-ms 60000)"#,
            "alpha-wait",
            &records
        )
    );
    // Beta waits on alpha (a cycle) once the roster shows alpha parked on
    // it, sends to it, then waits again (no one sends: a timeout).
    let beta = [
        await_marker(&parked),
        record(
            r#"(event-wait #:sender "alpha" #:timeout-ms 60000)"#,
            "beta-cycle",
            &records,
        ),
        record(
            &format!(r#"(agent-send {body:?} #:to "alpha")"#),
            "beta-send",
            &records,
        ),
        record(
            r#"(event-wait #:sender "alpha" #:timeout-ms 300)"#,
            "beta-timeout",
            &records,
        ),
    ]
    .join("\n");
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &parent), "spawned"],
        "repeatLastResponse": true,
        "children": {
            "alpha": { "responses": [cell("waiting", &alpha), "absorbed"], "repeatLastResponse": true },
            "beta": { "responses": [cell("talking", &beta), "absorbed"], "repeatLastResponse": true },
        },
    }));
    // The readiness beta's cycle check needs: alpha's wait edge, as the
    // roster reports it (its `waitingOn` naming beta), not alpha's word that
    // it is about to wait, which beta's check could still overtake.
    let (mut watcher, _) = Client::connect(&bed.runtime_socket()).await;
    watcher.subscribe("runtime/roster", Since::Now).await;
    let parked_on_beta = tokio::spawn(async move {
        let mut names = std::collections::HashMap::new();
        loop {
            let ServerFrame::Event { event, .. } = watcher.frame().await else {
                continue;
            };
            let changed = event.event.payload["changed"].as_array().cloned();
            for summary in changed.iter().flatten().map(|entry| &entry["summary"]) {
                if let (Some(id), Some(name)) = (
                    summary["activeSessionId"].as_str(),
                    summary["sessionName"].as_str(),
                ) {
                    names.insert(id.to_string(), name.to_string());
                }
                let on = summary["waitingOn"].as_str().and_then(|on| names.get(on));
                if summary["sessionName"] == "alpha" && on.is_some_and(|on| on == "beta") {
                    std::fs::write(&parked, "ready").expect("the parked marker");
                    return;
                }
            }
        }
    });
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "set them talking").await;
    parked_on_beta.await.expect("the roster watch");

    let cycle = recorded(&records, "beta-cycle").await;
    assert!(
        cycle["elapsedMs"].as_u64().expect("elapsed") < 10_000,
        "{cycle}"
    );
    assert_eq!(
        cycle,
        json!({
            "outcome": "cycle",
            "messageId": null,
            "senderName": null,
            "senderRole": null,
            "preview": null,
            "blockedVia": ["alpha"],
            "elapsedMs": cycle["elapsedMs"],
        })
    );
    let sent = recorded(&records, "beta-send").await;
    let waited = recorded(&records, "alpha-wait").await;
    let preview: String = body.chars().take(239).chain(['…']).collect();
    assert_eq!(
        waited,
        json!({
            "outcome": "message",
            "messageId": sent["id"],
            "senderName": "beta",
            "senderRole": "sibling",
            "preview": preview,
            "blockedVia": null,
            "elapsedMs": waited["elapsedMs"],
        })
    );
    let timeout = recorded(&records, "beta-timeout").await;
    assert_eq!(timeout["outcome"], "timeout", "{timeout}");
    assert!(
        timeout["elapsedMs"].as_u64().expect("elapsed") >= 300,
        "{timeout}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kernel_timer_keeps_firing_for_its_subscription_after_a_restart() {
    let bed = Bed::new();
    let arm = r#"(event-define "tick" #f #:timer (hash 'every "2s"))
(event-subscribe "tick" "Check the build.")"#;
    let daemon = bed.runtime_daemon(&json!({
        "responses": [cell("arming", arm), "armed", "ticked"],
        "repeatLastResponse": true,
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "arm the timer").await;
    // The turns the timer's events start, by their prompt.
    let ticks = async |client: &mut Client| {
        let page = ok(client
            .query("transcript.page", json!({ "session": id, "limit": 500 }))
            .await);
        page["entries"]
            .as_array()
            .expect("a page of entries")
            .iter()
            .filter(|row| row.to_string().contains("[agent-message from event tick]"))
            .count()
    };
    until(WAIT, "a tick before the restart", async || {
        (ticks(&mut client).await > 0).then_some(())
    })
    .await;
    drop(client);
    // Killed the hard way, as a crash would.
    drop(daemon);

    let _daemon =
        bed.runtime_daemon(&json!({ "responses": ["ticked"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    ok(client
        .call("session.create", json!({ "session": id }))
        .await);
    let before = ticks(&mut client).await;
    until(WAIT, "a tick after the restart", async || {
        (ticks(&mut client).await > before).then_some(())
    })
    .await;
}
