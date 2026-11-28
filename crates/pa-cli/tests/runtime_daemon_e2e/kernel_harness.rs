//! A saved session's kernel on the runtime daemon writes the session-local
//! harness store (`harness-put` without `#:global`, kept under the session's
//! artifact dir), and the agent dir's global store only when asked (the
//! supervisor's `kernel_harness_env_e2e`).

use std::path::Path;

use serde_json::{json, Value};

use super::turns::{cell, Attached};
use super::Bed;

/// The cell: puts and reads back a session-local memory, then puts a global
/// one, and writes `ok` or the kernel's error to `receipt`.
fn harness_cell(receipt: &Path) -> String {
    format!(
        r#"(define receipt
  (with-handler
    (lambda (e) (hash 'ok #f 'error (to-string e)))
    (begin
      (harness-put "memory" "kernel-local-memory" "persisted from the session kernel" #:title "kernel session-local memory")
      (hash-ref (hash-ref (hash-ref (harness-query #:view "state") 'entries) 'memory) 'kernel-local-memory)
      (harness-put "memory" "kernel-global-memory" "persisted from the session kernel" #:title "kernel global memory" #:global #t)
      (hash 'ok #t 'error ""))))
(fs-write {receipt:?} (value->jsexpr-string receipt))
"#,
        receipt = receipt.display().to_string(),
    )
}

/// Memory `id` of the harness state document at `path`, its timestamps
/// masked.
fn stored_memory(bed: &Bed, path: &Path, id: &str) -> Value {
    let agent_dir = bed.agent_dir();
    let state = pa_core::agent_state::StateDatabase::open(&pa_core::session::store::database_path(
        &agent_dir,
    ))
    .expect("open the agent database");
    let document = state
        .document(pa_core::agent_state::StateFile::HarnessState, path)
        .expect("read the harness state")
        .unwrap_or_else(|| panic!("no harness state at {}", path.display()));
    let state: Value = serde_json::from_str(&document).expect("a harness state document");
    let mut entry = state["entries"]["memory"][id].clone();
    for key in ["created_at", "updated_at"] {
        if entry.get(key).is_some() {
            entry[key] = json!("<timestamp>");
        }
    }
    entry
}

/// The memory entry the cell stores as `id` in `scope`.
fn memory(id: &str, title: &str, scope: &str) -> Value {
    json!({
        "id": id,
        "kind": "memory",
        "title": title,
        "content": "persisted from the session kernel",
        "path": "general",
        "scope": scope,
        "reference": {},
        "arguments": {},
        "metadata": {},
        "source": "agent",
        "version": 1,
        "created_at": "<timestamp>",
        "updated_at": "<timestamp>",
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_sessions_kernel_writes_the_session_local_harness_store() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = bed.dir.path().join("harness-receipt.json");
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("Writing the harness.", &harness_cell(&receipt)),
        "done",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let task = session.publish("run the cell", None).await;
    session.done(&task["task"]).await;

    let receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).expect("the cell's receipt"))
            .expect("a JSON receipt");
    // The saved session's artifact dir: `session-artifacts/<id>` beside its
    // sessions dir.
    let local = bed
        .agent_dir()
        .join("session-artifacts")
        .join(&session.id)
        .join("harness")
        .join("harness_state.json");
    let global = bed.agent_dir().join("harness").join("harness_state.json");
    assert_eq!(
        (
            receipt,
            stored_memory(&bed, &local, "kernel-local-memory"),
            stored_memory(&bed, &global, "kernel-global-memory"),
        ),
        (
            json!({ "ok": true, "error": "" }),
            memory(
                "kernel-local-memory",
                "kernel session-local memory",
                "local"
            ),
            memory("kernel-global-memory", "kernel global memory", "global"),
        )
    );
}
