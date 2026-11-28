//! `rlm.create_session` (a kernel's `(rlm-spawn prompt #:root #t)`) on the
//! runtime daemon, ported from the old daemon's `rlm_children_parent_death_e2e`
//! and `rlm_children_replacement_e2e` root cases, with their expectations:
//!
//! - a depth-0 session's kernel creates a sibling root that runs its task: its
//!   handle names it, and its agents-view row (a top-level session at depth 0)
//!   and its `sessions.list` row name its creator's transcript, while the creator's family (its
//!   ledger edges, its children) stays empty;
//! - the created root keeps running when its creator crashes: it is no child
//!   of the creator's.
//!
//! Every session of the daemon answers the same script from a queue of its
//! own, so the create cell creates only in the session named `creator`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::family::{ledger_ops, record, recorded};
use super::reliability::{await_file, wedged_cell};
use super::turns::{cell, Attached};
use super::{ok, turn_rows, Bed, Client};

/// The id of the session whose kernel creates the root.
const CREATOR: &str = "creator";

/// A cell that creates the root `rootkid` in the creator (recording its
/// handle as `create`), and does nothing in any other session.
fn create_cell(records: &Path) -> Value {
    let create = record(
        r#"(rlm-spawn "root session task" #:name "rootkid" #:root #t)"#,
        "create",
        records,
    );
    cell(
        "Creating.",
        &format!(
            r#"(if (equal? (session-info "id") "{CREATOR}") {create} (displayln "not the creator"))"#
        ),
    )
}

/// Creates the creator in `cwd`, runs its create turn, and answers the
/// root's handle once the root's own task is done.
async fn create_root(bed: &Bed, records: &Path) -> (Attached, Value, Attached) {
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut creator = Attached::create(bed, json!({ "session": CREATOR, "cwd": cwd })).await;
    let task = creator.publish("create a root", None).await;
    creator.done(&task["task"]).await;
    let handle = recorded(records, "create").await;
    let mut root = Attached::attach(bed, handle["session_id"].as_str().unwrap()).await;
    root.until(|root| !root.payloads("done").is_empty()).await;
    (creator, handle, root)
}

/// The transcript of session `id`.
fn transcript(bed: &Bed, id: &str) -> PathBuf {
    bed.agent_dir().join("sessions").join(format!("{id}.jsonl"))
}

/// The rows of `sessions.list` by session: its name and lineage, sorted.
async fn listed(session: &mut Attached) -> Vec<Value> {
    sorted(
        session.query("sessions.list").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| json!([row["session"], row["sessionName"], row["parentSessionPath"]]))
            .collect(),
    )
}

/// `rows` in a stable order.
fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_key(Value::to_string);
    rows
}

/// The listing of the creator and the root `root_id` it created.
fn creator_and_root(bed: &Bed, root_id: &str) -> Vec<Value> {
    sorted(vec![
        json!([CREATOR, Value::Null, Value::Null]),
        json!([root_id, "rootkid", transcript(bed, CREATOR)]),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_depth_0_session_creates_a_sibling_root_that_runs_its_task() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [create_cell(&records), "created"],
        "repeatLastResponse": true,
    }));
    let (mut creator, handle, mut root) = create_root(&bed, &records).await;
    let root_id = handle["session_id"].as_str().unwrap().to_string();
    let root_path = transcript(&bed, &root_id);
    // The agents view's row of the root (the old `get_state` summary).
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let summary = ok(client
        .query(
            "session.snapshot",
            json!({ "session": root_id, "shape": "state" }),
        )
        .await);
    let page = root.query("transcript.page").await;
    let children = creator.query("session.children").await;
    assert_eq!(
        (
            json!([
                handle["active_session_id"],
                handle["name"],
                handle["session_file"],
                handle["model"]
            ]),
            json!([
                summary["parentSessionPath"],
                summary["rlmDepth"],
                summary["runtimeKind"],
                summary["sessionName"]
            ]),
            turn_rows(&page)
                .into_iter()
                .filter(|(_, role, _)| role.as_deref() == Some("user"))
                .map(|(_, _, text)| text)
                .collect::<Vec<_>>(),
            listed(&mut creator).await,
            ledger_ops(&bed.agent_dir()),
            children["children"].clone(),
        ),
        (
            json!([root_id, "rootkid", root_path, "faux/faux-1"]),
            json!([transcript(&bed, CREATOR), 0, "top-level", "rootkid"]),
            vec![Some("root session task".to_string())],
            creator_and_root(&bed, &root_id),
            Vec::<(String, String)>::new(),
            json!([]),
        )
    );
}

/// The old daemon's `a_parent_crash_keeps_a_created_root_session_running`:
/// the creator's crash and rebuild leave the root it created running.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_root_keeps_running_when_its_creator_crashes() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let marker = bed.dir.path().join("cell-blocked");
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": [
            create_cell(&records),
            "created",
            cell("Blocking.", &wedged_cell(&marker)),
            "after the rebuild",
        ]}),
        &[
            ("PRIME_AGENT_TEST_BLOCKING_CELL_SEAM", "1"),
            ("PRIME_AGENT_STUCK_CELL_GRACE_MS", "300"),
        ],
    );
    let (mut creator, handle, _root) = create_root(&bed, &records).await;
    let root_id = handle["session_id"].as_str().unwrap().to_string();
    let before = listed(&mut creator).await;

    // The creator's next cell wedges; the stop finds it still on the engine
    // after the grace, and the creator is rebuilt.
    let wedged = creator.publish("block the kernel", None).await;
    await_file(&marker);
    creator
        .call("session.abort_and_send_queued", json!({}))
        .await;
    creator
        .until(|creator| !creator.payloads("recovered").is_empty())
        .await;
    creator.done(&wedged["task"]).await;
    std::fs::remove_file(&marker).unwrap();

    assert_eq!(
        (
            creator.payloads("crashed")[0]["kind"].clone(),
            before,
            listed(&mut creator).await,
        ),
        (
            json!("stuck"),
            creator_and_root(&bed, &root_id),
            creator_and_root(&bed, &root_id),
        )
    );
}
