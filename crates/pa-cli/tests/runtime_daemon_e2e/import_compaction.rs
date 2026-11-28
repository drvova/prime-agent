//! A session grown through `session.import_jsonl` compacts on the runtime
//! daemon (the supervisor's `import_compaction_e2e`): the import keeps every
//! row the TS loader keeps (an assistant's raw `stopReason: "tool_calls"`, a
//! tool result without `toolName`), so the compaction's walk finds a cut
//! inside the imported transcript and records it. The rows are read as the
//! store keeps them.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::turns::Attached;
use super::Bed;

/// How many user, assistant and tool-result turns the import holds: enough
/// that the default 20,000-token keep window leaves history before the cut.
const TURNS: usize = 1200;

/// The grown import: a parent-chained transcript of `TURNS` turns in the
/// foreign shapes the TS loader keeps.
fn grown_fixture(dir: &Path) -> PathBuf {
    let fixture = dir.join("grown-import.jsonl");
    let mut lines = vec![json!({
        "type": "session", "id": "grown-import", "version": 3,
        "timestamp": "2026-01-01T00:00:00.000Z", "cwd": dir, "rlmDepth": 0,
    })
    .to_string()];
    let mut parent: Option<String> = None;
    for turn in 0..TURNS {
        let at = 1_789_976_028_471_u64 + turn as u64;
        for (id, message) in [
            (
                format!("u{turn}"),
                json!({ "role": "user", "timestamp": at,
                        "content": [{ "type": "text", "text": format!("please do task number {turn}") }] }),
            ),
            (
                format!("a{turn}"),
                json!({
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": format!("task {turn}: run the corpus command") },
                        { "type": "text", "text": format!("Running the tool for task {turn}.") },
                    ],
                    "toolCalls": [{ "id": format!("call-{turn}"), "name": "ipython",
                                    "arguments": { "code": format!("print('corpus {turn}')") } }],
                    "api": "openai-completions", "provider": "prime-inference", "model": "mock-1",
                    "usage": { "input": 100, "output": 20, "cacheRead": 10, "cacheWrite": 0,
                               "totalTokens": 130,
                               "cost": { "input": 0.1, "output": 0.02, "cacheRead": 0,
                                         "cacheWrite": 0, "total": 0.12 } },
                    "stopReason": "tool_calls",
                    "timestamp": at,
                }),
            ),
            (
                format!("t{turn}"),
                json!({ "role": "toolResult", "toolCallId": format!("call-{turn}"),
                        "content": [{ "type": "text", "text": format!("corpus {turn}\n[0, 1, 2]\n") }],
                        "isError": false, "timestamp": at }),
            ),
        ] {
            lines.push(
                json!({ "type": "message", "id": id, "parentId": parent,
                        "timestamp": "2026-01-01T00:00:01.000Z", "message": message })
                .to_string(),
            );
            parent = Some(id);
        }
    }
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    fixture
}

/// The rows of `kind` of the session's stored transcript, as stored.
async fn rows(session: &mut Attached, kind: &str) -> Vec<Value> {
    let state = session
        .query_with("session.snapshot", json!({ "shape": "state" }))
        .await;
    let file = state["sessionFile"]
        .as_str()
        .expect("the session's transcript");
    super::support::saved_text(Path::new(file))
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["type"] == kind)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_imported_session_compacts_inside_the_imported_transcript() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    // A window the import's ~80,000 tokens stay well under: only the
    // `session.compact` compacts.
    let _daemon = bed.runtime_daemon(&json!({
        "responses": ["scripted reply"],
        "repeatLastResponse": true,
        "contextWindow": 1_000_000,
    }));
    let mut session = Attached::open(&bed, &cwd).await;
    let fixture = grown_fixture(bed.dir.path());
    let imported = session
        .call(
            "session.import_jsonl",
            json!({ "inputPath": fixture, "cwdOverride": cwd }),
        )
        .await;

    // Every row the TS loader keeps is the session's.
    let messages = rows(&mut session, "message").await;
    let imported_rows = (
        messages.len(),
        messages.iter().any(|row| {
            row["message"]["role"] == "assistant" && row["message"]["stopReason"] == "tool_calls"
        }),
        messages.iter().any(|row| {
            row["message"]["role"] == "toolResult" && row["message"].get("toolName").is_none()
        }),
    );
    let turn = session
        .publish("one turn on the imported session", None)
        .await;
    session.done(&turn["task"]).await;
    let compacted = session.call("session.compact", json!({})).await;
    let compaction = &compacted["compaction"];
    let first_kept = compaction["result"]["firstKeptEntryId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // A row id of the fixture past its first turn: the cut is inside the
    // imported transcript.
    let kept_turn: Option<usize> = first_kept
        .strip_prefix(['u', 'a', 't'])
        .and_then(|index| index.parse().ok());
    let durable: Vec<Value> = rows(&mut session, "compaction")
        .await
        .iter()
        .map(|row| row["firstKeptEntryId"].clone())
        .collect();
    assert_eq!(
        (
            imported,
            imported_rows,
            compacted["outcome"].clone(),
            compaction["result"]["summary"].is_string(),
            kept_turn.is_some_and(|turn| turn > 0),
            durable,
        ),
        (
            json!({ "cancelled": false }),
            (TURNS * 3, true, true),
            json!("done"),
            true,
            true,
            vec![json!(first_kept)],
        ),
        "the compaction: {compacted}"
    );
}
