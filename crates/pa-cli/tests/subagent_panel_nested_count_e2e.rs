//! End-to-end verifier for the subagent panel's nested counts: a
//! two-level spawn on the runtime daemon (a root session's child that
//! itself spawns a grandchild, each by its own kernel's `rlm-spawn` on the
//! faux model) must surface BOTH descendants on every count surface (TS
//! `countRosterSubagentStatuses`).
#![cfg(unix)]

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::path::Path;

use pa_tui::subagents::{count_descendants, SessionIdentity};
use serde_json::{json, Value};

/// One faux turn that runs `code` in an `eval` cell.
fn cell(text: &str, code: &str) -> Value {
    json!({ "content": [
        { "type": "text", "text": text },
        { "type": "toolCall", "name": "eval", "id": "toolu_cell01", "arguments": { "code": code } },
    ]})
}

/// The faux script: the root spawns `worker-b`, which spawns `worker-c`.
fn write_script(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("script.json");
    std::fs::write(
        &script,
        json!({
            "responses": [
                cell("spawning", r#"(rlm-spawn "ship the lane" #:name "worker-b")"#),
                "root done",
            ],
            "repeatLastResponse": true,
            "children": {
                "worker-b": {
                    "responses": [
                        cell("spawning", r#"(rlm-spawn "audit the lane" #:name "worker-c")"#),
                        "child answer",
                    ],
                    "repeatLastResponse": true,
                },
                "worker-c": { "responses": ["grandchild answer"], "repeatLastResponse": true },
            },
        })
        .to_string(),
    )
    .expect("write script");
    script
}

/// The summary value of one roster entry (the wire shape the TUI walks).
fn entry_summary(entry: &Value) -> &Value {
    entry.get("summary").unwrap_or(entry)
}

fn entry_field<'a>(entry: &'a Value, field: &str) -> Option<&'a str> {
    entry_summary(entry)
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// The parent keys of one roster entry (TS `getParentKeys`).
fn entry_parent_keys(entry: &Value) -> Vec<String> {
    [
        entry_field(entry, "parentActiveSessionId").map(|id| format!("active:{id}")),
        entry_field(entry, "parentSessionId").map(|id| format!("session:{id}")),
        entry_field(entry, "parentSessionPath").map(|file| format!("file:{file}")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The identity a summary-box walk starts from: the row's live ids and its
/// transcript.
fn identity_of(entry: &Value) -> SessionIdentity {
    SessionIdentity::new(
        entry_field(entry, "activeSessionId").map(str::to_string),
        entry_field(entry, "sessionId").map(str::to_string),
        entry_field(entry, "sessionFile").map(str::to_string),
    )
}

/// A root session spawns a child, the child spawns a grandchild, and
/// every count surface reports the whole subtree: the summary-box walk
/// counts two descendants from the root (one from the child).
#[test]
fn a_two_level_spawn_counts_the_whole_tree() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let script = write_script(dir.path());
    let daemon = RuntimeDaemon::spawn_scripted(dir.path(), &script);
    let mut client = daemon.client();
    let created = client
        .call("session.create", &json!({}))
        .expect("session.create");
    let root_id = created["session"]
        .as_str()
        .expect("the session id")
        .to_string();
    client.run_turn(&root_id, "dispatch the lane");
    // At rest once the family below it has settled and the notice turns its
    // settles woke have run.
    client
        .call("session.wait_idle", &json!({ "session": root_id }))
        .expect("session.wait_idle");

    // Both descendants are on the roster, settled (the roster is composed
    // from the sessions as they stand), the grandchild under the child.
    let roster = client
        .query("runtime.roster", &json!({}))
        .expect("runtime.roster")["roster"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let settled = |name: &str| {
        roster
            .iter()
            .find(|entry| {
                entry_field(entry, "sessionName") == Some(name)
                    && entry.get("status").and_then(Value::as_str) == Some("idle")
            })
            .cloned()
            .unwrap_or_else(|| panic!("{name} is on the roster, settled: {roster:?}"))
    };
    let (child_row, grandchild_row) = (settled("worker-b"), settled("worker-c"));
    let root_row = roster
        .iter()
        .find(|entry| entry_field(entry, "activeSessionId") == Some(root_id.as_str()))
        .expect("the root's row");
    assert_eq!(
        entry_summary(&grandchild_row)
            .get("rlmDepth")
            .and_then(Value::as_u64),
        Some(2),
        "the grandchild runs at depth 2"
    );
    let child_active = entry_field(&child_row, "activeSessionId").expect("the child's live id");
    let child_file = entry_field(&child_row, "sessionFile").expect("the child's transcript");
    assert_eq!(
        entry_parent_keys(&grandchild_row),
        vec![
            format!("active:{child_active}"),
            format!("session:{child_active}"),
            format!("file:{child_file}"),
        ],
        "the grandchild links to its parent's live id and transcript"
    );

    // The summary-box walk counts the whole subtree from either root.
    assert_eq!(
        (
            count_descendants(&roster, &identity_of(root_row)).total,
            count_descendants(&roster, &identity_of(&child_row)).total,
        ),
        (2, 1),
        "the root counts its child and grandchild; the child its own child"
    );
}
