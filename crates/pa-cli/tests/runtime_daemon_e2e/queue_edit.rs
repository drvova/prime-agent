//! A session's queue edits and aborts on the runtime daemon (phase 8d-2c):
//! `mutate_queued_message` (moves, rewrites and deletes of follow-ups and of
//! the steers the running turn took, a steer moved from the follow-ups, the
//! rejections and the malformed requests), `abort_compaction` mid-summary
//! and `abort_retry` mid-wait, sent through pa-tui's `wire2_compat`, answer
//! as the old daemon (the `pa-daemon` supervisor) answers them, leave the
//! same queue view after each edit, and the edited queue runs into the same
//! turns.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::Path;
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;
use pa_types::wire2::ClientFrame;
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::{ok, Bed, Client};

/// The runtime daemon runs offline, as the old daemon did for its golden.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// How long a queue view may take to show what was queued, or the wedge
/// cell to start.
const VIEW_WAIT: Duration = Duration::from_secs(30);

/// The replies: a first one whose cell runs until it is aborted, so
/// everything after it queues, then one per turn the edited queue runs.
fn responses() -> Value {
    json!([
        cell(
            "Running the wedge cell.",
            "(bash \"sleep 300\" (hash 'wait-ms #f))"
        ),
        "one",
        "two",
        "three",
    ])
}

/// The edits, in order, each without its session.
fn edits() -> Vec<Value> {
    let edit = |lane: &str, index: u64, expected: &str, mutation: Value| {
        json!({ "type": "mutate_queued_message", "lane": lane, "index": index,
                "expectedText": expected, "mutation": mutation })
    };
    vec![
        edit(
            "followUp",
            0,
            "f1",
            json!({ "type": "move", "direction": 1 }),
        ),
        edit(
            "followUp",
            2,
            "f3",
            json!({ "type": "move", "direction": 1 }),
        ),
        edit(
            "followUp",
            2,
            "f3",
            json!({ "type": "replace", "text": "f3 edited" }),
        ),
        edit("followUp", 0, "stale", json!({ "type": "delete" })),
        edit("followUp", 0, "f2", json!({ "type": "delete" })),
        edit(
            "followUp",
            1,
            "f3 edited",
            json!({ "type": "move", "direction": -1 }),
        ),
        edit("followUp", 0, "f3 edited", json!({ "type": "replace" })),
        edit("steering", 0, "f1", json!({ "type": "delete" })),
        edit("sideways", 0, "f1", json!({ "type": "delete" })),
        edit("followUp", 0, "f1", json!({ "type": "swap" })),
    ]
}

/// The queue view once the follow-ups are queued behind the running cell.
fn queued() -> Value {
    json!({ "steering": [], "followUps": ["f1", "f2", "f3"] })
}

/// A queue view's rows, as `sessionActions` lists them.
fn rows(actions: &Value) -> Value {
    json!({ "steering": actions["steering"], "followUps": actions["followUps"] })
}

/// The user messages' texts among `messages`, in order.
fn user_texts(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            content => content[0]["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// `command` (without its session) sent to open session `id` through the
/// compat layer, as the TUI reads its answer.
async fn compat(client: &mut Client, id: &str, index: usize, mut command: Value) -> Value {
    command["activeSessionId"] = json!(id);
    let kind = command["type"].as_str().unwrap().to_string();
    let command: DaemonCommand = serde_json::from_value(command).unwrap();
    let mut frames =
        pa_tui::wire2_request_frames(format!("q{index}"), command).expect("a mapped command");
    let Some(ClientFrame::Call { method, args, .. }) = frames.pop() else {
        panic!("{kind} is a call");
    };
    let outcome = client.call(&method, args).await;
    read_as_the_tui_does(&pa_tui::wire2_response_frame(&kind, outcome).unwrap())
}

/// Open session `id`'s queue view, as its snapshot shows it.
async fn runtime_rows(client: &mut Client, id: &str) -> Value {
    let state = ok(client
        .query(
            "session.snapshot",
            json!({ "session": id, "shape": "state" }),
        )
        .await);
    rows(&state["sessionActions"])
}

#[tokio::test(flavor = "multi_thread")]
async fn the_queue_edits_answer_and_run_as_the_old_daemons_do() {
    let bed = Bed::new();
    let (old, old_texts): (Vec<Value>, Vec<String>) =
        OldReference::open("queue_edits", &bed).take("edits");

    let _daemon = bed.runtime_daemon_with(&json!({ "responses": responses() }), OFFLINE);
    let mut session = Attached::open(&bed, bed.dir.path()).await;
    let id = session.id.clone();
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    session.publish("first", None).await;
    session.until_streamed("tool_execution_start").await;
    for text in ["f1", "f2", "f3"] {
        session.publish(text, Some("follow_up")).await;
    }
    let deadline = Instant::now() + VIEW_WAIT;
    while runtime_rows(&mut client, &id).await != queued() {
        assert!(
            Instant::now() < deadline,
            "the queue never showed {}",
            queued()
        );
        tokio::task::yield_now().await;
    }
    let mut answers = Vec::new();
    for (index, edit) in edits().into_iter().enumerate() {
        let answer = compat(&mut client, &id, index, edit).await;
        answers.push(json!({ "answer": answer, "rows": runtime_rows(&mut client, &id).await }));
    }
    ok(client.call("session.abort", json!({ "session": id })).await);
    ok(client
        .call("session.resume_queue", json!({ "session": id }))
        .await);
    ok(client
        .call("session.wait_idle", json!({ "session": id }))
        .await);
    let page = ok(client
        .query(
            "transcript.page",
            json!({ "session": id, "shape": "messages" }),
        )
        .await);
    let texts = user_texts(&page["messages"]);

    let answered = |status: &str, follow_ups: Value| {
        json!({ "answer": { "type": "response", "command": "mutate_queued_message",
                            "success": true, "data": { "status": status } },
                "rows": { "steering": [], "followUps": follow_ups } })
    };
    let failed = |error: &str| {
        json!({ "answer": { "type": "response", "command": "mutate_queued_message",
                            "success": false, "error": error },
                "rows": { "steering": [], "followUps": ["f3 edited", "f1"] } })
    };
    let expected = json!([
        answered("applied", json!(["f2", "f1", "f3"])),
        answered("rejected", json!(["f2", "f1", "f3"])),
        answered("applied", json!(["f2", "f1", "f3 edited"])),
        answered("rejected", json!(["f2", "f1", "f3 edited"])),
        answered("applied", json!(["f1", "f3 edited"])),
        answered("applied", json!(["f3 edited", "f1"])),
        answered("rejected", json!(["f3 edited", "f1"])),
        answered("rejected", json!(["f3 edited", "f1"])),
        failed("mutate_queued_message requires lane \"steering\" or \"followUp\""),
        failed("mutate_queued_message requires mutation.type \"delete\", \"move\", or \"replace\""),
    ]);
    assert_eq!(Value::from(old), expected);
    assert_eq!(Value::from(answers), expected);
    // The edited queue runs into the same turns, one per follow-up.
    assert_eq!(texts, old_texts);
    assert_eq!(old_texts, ["first", "f3 edited", "f1"].map(str::to_string));
}

/// The settings that hold a failed request's retry for ten minutes, and
/// compact all but the last token (the compaction golden's).
fn slow_retries(agent_dir: &Path) {
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "compaction": { "keepRecentTokens": 1 }, "retry": { "enabled": true, "maxRetries": 3, "baseDelayMs": 600_000,
                           "maxRetryDelayMs": 3_600_000 } })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_and_a_retry_stop_when_aborted() {
    let bed = Bed::new();
    slow_retries(&bed.agent_dir());
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": [
            "First answer, long enough to be worth compacting later on.",
            "Second answer, so the history has an older turn to summarize.",
            { "content": [{ "type": "text", "text": "" }], "stopReason": "error",
              "errorMessage": "503 upstream unavailable" },
            { "text": "a summary", "delayMs": 600_000 },
        ] }),
        OFFLINE,
    );
    let mut session = Attached::open(&bed, bed.dir.path()).await;
    let id = session.id.clone();
    for text in ["hello", "and again"] {
        let accepted = session.publish(text, None).await;
        session.done(&accepted["task"]).await;
    }
    let (mut compacting, _) = Client::connect(&bed.runtime_socket()).await;
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;

    // The failed request's retry waits; the abort ends it.
    let accepted = session.publish("fail", None).await;
    session.until_streamed("auto_retry_start").await;
    let stopped = compat(&mut client, &id, 0, json!({ "type": "abort_retry" })).await;
    session.done(&accepted["task"]).await;

    // The compaction's summary request waits; the abort ends it.
    let request = compacting.id();
    compacting
        .send(&ClientFrame::Call {
            id: request.clone(),
            method: "session.compact".to_string(),
            args: json!({ "session": id }),
        })
        .await;
    session.until_streamed("compaction_start").await;
    let aborted = compat(&mut client, &id, 1, json!({ "type": "abort_compaction" })).await;
    let compacted = ok(compacting.reply(&request, &mut Vec::new()).await);
    let retry_end = session
        .payloads("stream")
        .into_iter()
        .find(|event| event["type"] == "auto_retry_end")
        .map(|mut event| {
            event.as_object_mut().unwrap().remove("meta");
            event
        });

    // The old daemon's answers, captured from it (pa-daemon's
    // `supervisor_e2e` compaction test held it to the same golden; this copy
    // outlived it).
    let golden: Value = serde_json::from_str(include_str!(
        "../fixtures/old_daemon/compaction-live-ts.json"
    ))
    .unwrap();
    assert_eq!(
        (
            aborted,
            compacted["error"].clone(),
            compacted["compaction"].clone()
        ),
        (
            json!({ "type": "response", "command": "abort_compaction", "success": true }),
            golden["compact"]["abortedError"].clone(),
            golden["compactionEndAborted"].clone(),
        )
    );
    assert_eq!(
        (stopped, retry_end),
        (
            json!({ "type": "response", "command": "abort_retry", "success": true }),
            Some(
                json!({ "type": "auto_retry_end", "success": false, "attempt": 1,
                         "finalError": "Retry cancelled" })
            ),
        )
    );
    // Nothing was compacted.
    let page = ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await);
    assert!(
        !page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["type"] == "compaction"),
        "{page}"
    );
}
