use serde_json::{json, Value};
use std::fmt::Write as _;

use super::{register_ctx_handlers, SLICE_CHARS_CAP};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::kernel::spill::{SpillCopy, SpillLookup, SpillStore, SPILL_DIR};

fn handlers_over(store: Option<SpillStore>) -> HostRequestHandlers {
    let mut handlers = HostRequestHandlers::new();
    register_ctx_handlers(&mut handlers, store, /*observer*/ None);
    handlers
}

async fn ask(handlers: &HostRequestHandlers, request: &str, data: Value) -> Value {
    let handler = handlers.get(request).expect("registered");
    handler(HostRequestPayload { data }).await.expect("reply")
}

/// `count` numbered lines of about 50 bytes each.
fn numbered(count: usize) -> String {
    (0..count).fold(String::new(), |mut out, n| {
        let _ = writeln!(out, "row {n:06} of a megabyte of build output, kept");
        out
    })
}

#[tokio::test]
async fn page_reads_the_middle_of_a_megabyte_output_whole() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SpillStore::unregistered(dir.path().join(SPILL_DIR), u64::MAX);
    let text = numbered(21_000);
    assert!(text.len() > 1_000_000);
    let handle = format!("out:{}", store.spill(SpillCopy::of_text(&text)).id);
    let handlers = handlers_over(Some(store));

    let reply = ask(
        &handlers,
        "ctx.page",
        json!({"handle": handle, "start": 10_500, "lines": 3}),
    )
    .await;
    assert_eq!(
        reply,
        json!({
            "handle": handle,
            "state": "stored",
            "totalChars": text.len(),
            "lines": 21_000,
            "storedLines": 21_000,
            "start": 10_500,
            "end": 10_503,
            "text": "row 010500 of a megabyte of build output, kept\nrow 010501 of a megabyte of build output, kept\nrow 010502 of a megabyte of build output, kept\n",
            "truncated": false,
        })
    );
    // Paging on from each page's end rebuilds the output byte for byte,
    // every reply within the slice cap.
    let mut rebuilt = String::new();
    let mut start = 0;
    loop {
        let page = ask(
            &handlers,
            "ctx.page",
            json!({"handle": handle, "start": start, "lines": 2000}),
        )
        .await;
        let chunk = page["text"].as_str().expect("text");
        assert!(chunk.chars().count() <= SLICE_CHARS_CAP);
        if chunk.is_empty() {
            break;
        }
        rebuilt.push_str(chunk);
        start = page["end"].as_u64().expect("end");
    }
    assert_eq!(rebuilt, text);
}

#[tokio::test]
async fn grep_and_tail_answer_within_the_cap() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SpillStore::unregistered(dir.path().join(SPILL_DIR), u64::MAX);
    let text = "ok 0\nok 1\nerror: a\nok 3\nok 4\nok 5\nok 6\nerror: b\nok 8\nerror: c\n";
    let handle = format!("out:{}", store.spill(SpillCopy::of_text(text)).id);
    let handlers = handlers_over(Some(store));

    let grep = ask(
        &handlers,
        "ctx.grep",
        json!({"handle": handle, "pattern": "^error", "context": 1, "max_matches": 2}),
    )
    .await;
    let tail = ask(&handlers, "ctx.tail", json!({"handle": handle, "n": 2})).await;
    let base = json!({"handle": handle, "state": "stored", "totalChars": text.len(), "lines": 10, "storedLines": 10});
    let with = |extra: Value| {
        let mut value = base.clone();
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        value
    };
    assert_eq!(
        (grep, tail),
        (
            with(json!({
                "pattern": "^error",
                "matches": 3,
                "shown": 2,
                "text": "1-ok 1\n2:error: a\n3-ok 3\n--\n6-ok 6\n7:error: b\n8-ok 8\n",
                "truncated": true,
            })),
            with(json!({"start": 8, "end": 10, "text": "ok 8\nerror: c\n", "truncated": false})),
        )
    );
}

#[tokio::test]
async fn handles_without_text_answer_their_state() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SpillStore::unregistered(dir.path().join(SPILL_DIR), 10);
    let first = store.spill(SpillCopy::of_text(&numbered(4))).id;
    assert!(matches!(store.lookup(&first).await, SpillLookup::Stored(_)));
    let second = store.spill(SpillCopy::of_text(&numbered(5))).id;
    assert!(matches!(
        store.lookup(&second).await,
        SpillLookup::Stored(_)
    ));
    let handlers = handlers_over(Some(store));
    let ephemeral = handlers_over(None);

    let answers = (
        ask(
            &handlers,
            "ctx.page",
            json!({"handle": format!("out:{first}")}),
        )
        .await,
        ask(&handlers, "ctx.info", json!({"handle": "out:0123456789"})).await,
        ask(&ephemeral, "ctx.info", json!({"handle": "out:0123456789"})).await,
    );
    assert_eq!(
        answers,
        (
            json!({"handle": format!("out:{first}"), "state": "evicted"}),
            json!({"handle": "out:0123456789", "state": "unknown"}),
            json!({"handle": "out:0123456789", "state": "unavailable"}),
        )
    );
}

/// The handle a cut cell left in a saved transcript row still reads after
/// the session is reopened by a fresh process.
#[tokio::test]
async fn a_handle_row_survives_resume() {
    use crate::session::manager::SessionManager;
    use pa_types::session::{AgentMessage, FileEntry};

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let sessions = tmp.path().join("sessions");
    let mut manager = SessionManager::persisted(tmp.path(), &sessions);
    let artifact_dir = manager.get_session_artifact_dir().expect("persisted");
    let text = numbered(3_000);
    let marker = {
        let store = SpillStore::unregistered(artifact_dir.join(SPILL_DIR), u64::MAX);
        let handle = store.spill(SpillCopy::of_text(&text));
        assert!(matches!(
            store.lookup(&handle.id).await,
            SpillLookup::Stored(_)
        ));
        handle.marker(text.len() - 100)
    };
    manager
        .append_message(AgentMessage::Assistant(pa_types::ai::AssistantMessage {
            content: vec![],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-x".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: pa_types::ai::StopReason::ToolUse,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: serde_json::Map::default(),
        }))
        .expect("assistant row");
    manager
        .append_message(AgentMessage::ToolResult(pa_types::ai::ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "python_repl".to_string(),
            content: vec![pa_types::ai::UserContentBlock::Text(
                pa_types::ai::TextContent {
                    text: format!("{}\n{marker}\n", &text[..100]),
                    text_signature: None,
                },
            )],
            details: None,
            is_error: false,
            timestamp: 0,
            rest: serde_json::Map::default(),
        }))
        .expect("tool row");
    let file = manager.get_session_file().expect("file").to_path_buf();
    drop(manager);

    let reopened = SessionManager::open(tmp.path(), &sessions, &file);
    let row_text = reopened
        .retained_entries()
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: AgentMessage::ToolResult(result),
                ..
            } => result.content.iter().find_map(|block| match block {
                pa_types::ai::UserContentBlock::Text(text) => Some(text.text.clone()),
                pa_types::ai::UserContentBlock::Image(_)
                | pa_types::ai::UserContentBlock::Raw(_) => None,
            }),
            _ => None,
        })
        .expect("the tool row was reloaded");
    let handle = row_text
        .split("[out:")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .map(|id| format!("out:{id}"))
        .expect("the row carries its handle");
    let fresh = SpillStore::unregistered(
        reopened
            .get_session_artifact_dir()
            .expect("persisted")
            .join(SPILL_DIR),
        u64::MAX,
    );
    let handlers = handlers_over(Some(fresh));
    let reply = ask(&handlers, "ctx.tail", json!({"handle": handle, "n": 1})).await;
    assert_eq!(
        (reply["state"].clone(), reply["text"].clone()),
        (
            json!("stored"),
            json!("row 002999 of a megabyte of build output, kept\n")
        )
    );
}
