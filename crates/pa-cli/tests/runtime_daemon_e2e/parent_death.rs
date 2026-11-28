//! A subagent's parent dying, and returning, on the runtime daemon, ported
//! from the old daemon's `rlm_children_parent_death_e2e`, with its
//! expectations:
//!
//! - a parent's crash closes its running child: the child's session ends
//!   (its ledger edge stays, nothing is tombstoned), and the rebuilt parent
//!   lists it again as a settled child whose cut-off run failed, a passive
//!   row under the parent in `sessions.list`;
//! - a spawned child's semantic-edge ledger names its parent and the
//!   request that spawned it, and the parent's ledger records the child's
//!   return (its last committed request) after the spawning request finished
//!   and before the request the child's settle notice starts.
//!
//! The old daemon's third case, a created root that survives its creator's
//! crash, is `root_sessions::a_created_root_keeps_running_when_its_creator_crashes`.

use std::path::Path;
use std::time::Duration;

use pa_core::session_engine::semantic_edges::SemanticEdgeLedgerEvent as LedgerEvent;
use serde_json::{json, Value};

use super::family::{ledger_ops, record, recorded};
use super::reliability::{await_file, wedged_cell};
use super::schedules::until;
use super::turns::{cell, Attached};
use super::Bed;

/// How long a test waits for a record or a state change before it fails.
const WAIT: Duration = Duration::from_secs(90);

/// The rows of session `parent`'s `session.children`: handle, session and
/// status.
async fn children(parent: &mut Attached) -> Vec<Value> {
    parent.query("session.children").await["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| json!([child["id"], child["activeSessionId"], child["status"]]))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parent_crash_closes_its_running_child_and_relists_it_as_failed() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let marker = bed.dir.path().join("cell-blocked");
    let spawn = record(r#"(rlm-spawn "hold on" #:name "kid")"#, "spawn", &records);
    let _daemon = bed.runtime_daemon_with(
        &json!({
            "responses": [
                cell("spawning", &spawn),
                "spawn turn done",
                cell("Blocking.", &wedged_cell(&marker)),
                "after the rebuild",
            ],
            "children": { "kid": { "responses": [{ "text": "kid done", "delayMs": 600_000 }] } },
        }),
        &[
            ("PRIME_AGENT_TEST_BLOCKING_CELL_SEAM", "1"),
            ("PRIME_AGENT_STUCK_CELL_GRACE_MS", "300"),
        ],
    );
    let mut parent = Attached::open(&bed, &cwd).await;
    let task = parent.publish("spawn the kid", None).await;
    parent.done(&task["task"]).await;
    let handle = recorded(&records, "spawn").await["rlm_child_id"]
        .as_str()
        .unwrap()
        .to_string();
    let running = children(&mut parent).await;
    let kid = running[0][1].as_str().unwrap().to_string();

    // The parent's next cell wedges; the stop finds it still on the engine
    // after the grace, and the parent's lifetime ends as crashed.
    let wedged = parent.publish("block the kernel", None).await;
    await_file(&marker);
    parent
        .call("session.abort_and_send_queued", json!({}))
        .await;
    parent
        .until(|parent| !parent.payloads("recovered").is_empty())
        .await;
    parent.done(&wedged["task"]).await;
    std::fs::remove_file(&marker).unwrap();

    let relisted = until(WAIT, "the relisted child", async || {
        let rows = children(&mut parent).await;
        (rows.first().is_some_and(|row| row[2] == "error")).then_some(rows)
    })
    .await;
    let listed: Vec<Value> = parent.query("sessions.list").await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["rlmChildId"] == json!(handle))
        .map(|row| {
            json!([
                row["session"],
                row["open"],
                row["isSessionActive"],
                row["parentActiveSessionId"],
                row["runtimeKind"],
                row["sessionName"]
            ])
        })
        .collect();
    assert_eq!(
        (running, relisted, listed, ledger_ops(&bed.agent_dir())),
        (
            vec![json!([handle, kid, "running"])],
            vec![json!([handle, kid, "error"])],
            vec![json!([kid, false, false, parent.id, "subagent", "kid"])],
            vec![("spawn".to_string(), handle.clone())],
        )
    );
}

/// The events of the semantic-edge ledger at `path` (none while it is not
/// there yet).
fn semantic_ledger(path: &Path) -> Vec<LedgerEvent> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("a ledger line"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawned_childs_ledger_names_its_spawning_request_and_the_parent_records_its_return() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let spawn = record(
        r#"(rlm-spawn "child task text" #:name "kid")"#,
        "spawn",
        &records,
    );
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawn), "spawn turn done"],
        "repeatLastResponse": true,
        "children": { "kid": { "responses": ["child reply"] } },
    }));
    let mut parent = Attached::open(&bed, &cwd).await;
    let task = parent.publish("spawn the kid", None).await;
    parent.done(&task["task"]).await;
    let spawned = recorded(&records, "spawn").await;
    let child_ledger_path =
        Path::new(spawned["session_dir"].as_str().unwrap()).join("semantic-edges.jsonl");
    let parent_ledger_path = bed
        .agent_dir()
        .join("session-artifacts")
        .join(&parent.id)
        .join("semantic-edges.jsonl");

    // The child settles, then its no-reply notice starts the parent's next
    // request: the parent's ledger holds the return and a request after it.
    let (parent_ledger, child_ledger) = until(WAIT, "the recorded return", async || {
        let parent_ledger = semantic_ledger(&parent_ledger_path);
        let returned = parent_ledger
            .iter()
            .position(|event| matches!(event, LedgerEvent::ChildReturned { .. }))?;
        parent_ledger[returned + 1..]
            .iter()
            .any(|event| matches!(event, LedgerEvent::RequestStarted { .. }))
            .then(|| (parent_ledger.clone(), semantic_ledger(&child_ledger_path)))
    })
    .await;
    let spawning_request = parent_ledger
        .iter()
        .find_map(|event| match event {
            LedgerEvent::RequestStarted { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the parent's first request");
    let child_session = child_ledger
        .iter()
        .find_map(|event| match event {
            LedgerEvent::SessionRegistered { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .expect("the child registered");
    let child_last_committed = child_ledger
        .iter()
        .rev()
        .find_map(|event| match event {
            LedgerEvent::RequestFinished { request_id } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the child committed its reply");
    let returned_at = parent_ledger
        .iter()
        .position(|event| matches!(event, LedgerEvent::ChildReturned { .. }))
        .unwrap();
    assert_eq!(
        (
            child_ledger.first().cloned(),
            parent_ledger[returned_at].clone(),
            parent_ledger[..returned_at].iter().any(|event| matches!(
                event,
                LedgerEvent::RequestFinished { request_id } if *request_id == spawning_request
            )),
        ),
        (
            Some(LedgerEvent::SessionRegistered {
                session_id: child_session.clone(),
                parent_session_id: Some(parent.id.clone()),
                spawned_by_request_id: Some(spawning_request),
            }),
            LedgerEvent::ChildReturned {
                session_id: parent.id.clone(),
                child_session_id: child_session,
                request_id: child_last_committed,
            },
            true,
        )
    );
}
