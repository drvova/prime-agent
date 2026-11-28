//! The session-event stream on the runtime daemon (phase 8b-2): a session's
//! `agent/<id>/stream` carries the old daemon's `session_event` payloads,
//! each numbered with the session's own `meta.sequence`, which
//! `session.snapshot` reports as `lastEventSequence`.

use serde_json::{json, Value};

use super::turns::{cell, Attached};
use super::Bed;

/// One turn with a cell and a reply after it streams the old worker's frames
/// in its order: the reply opens with a `start`, its deltas coalesce (a
/// parked one consumes its number, so the numbers only rise), a tool result
/// is a start and end pair, the settled assistant row carries its billing
/// stamp, and every settled conversation row its context reading. A tool's
/// update and end name no tool, as the old frames do not.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_streams_the_old_session_events_numbered_in_order() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("computing", "(display 42)"),
        "The cell printed 42.",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let prompt = session.publish("compute it", None).await;
    session.done(&prompt["task"]).await;
    let snapshot = session.query("session.snapshot").await;

    let stream = session.payloads("stream");
    let sequences: Vec<u64> = stream
        .iter()
        .map(|event| {
            event["meta"]["sequence"]
                .as_u64()
                .unwrap_or_else(|| panic!("a numbered frame: {event}"))
        })
        .collect();
    // The frames' kinds, a run of coalesced updates read as one.
    let mut kinds: Vec<String> = Vec::new();
    for event in &stream {
        let mut kind = event["type"].as_str().unwrap_or_default().to_string();
        if let Some(role) = event["message"]["role"].as_str() {
            kind = format!("{kind} {role}");
        }
        if kinds.last() != Some(&kind) || kind != "message_update assistant" {
            kinds.push(kind);
        }
    }
    let first = |kind: &str, role: &str| -> &Value {
        stream
            .iter()
            .find(|event| event["type"] == kind && event["message"]["role"] == role)
            .unwrap_or_else(|| panic!("a {kind} of a {role} row: {stream:?}"))
    };
    let tool = |kind: &str| -> &Value {
        stream
            .iter()
            .find(|event| event["type"] == kind)
            .unwrap_or_else(|| panic!("a {kind}: {stream:?}"))
    };
    assert_eq!(
        (
            sequences.windows(2).all(|pair| pair[0] < pair[1]),
            snapshot["lastEventSequence"].as_u64(),
            kinds,
            first("message_start", "assistant")["assistantMessageEvent"].clone(),
            first("message_end", "assistant")["message"]["usage"]
                .get("billing")
                .is_some(),
            first("message_end", "user")["contextUsage"]["tokens"].is_u64(),
            first("message_end", "toolResult")["contextUsage"]["tokens"].is_u64(),
            (
                tool("tool_execution_start")["toolName"].clone(),
                tool("tool_execution_end").get("toolName").cloned(),
                tool("tool_execution_end")["isError"].clone(),
            ),
        ),
        (
            true,
            sequences.last().copied(),
            [
                "agent_start",
                "turn_start",
                "message_start user",
                "message_end user",
                "message_start assistant",
                "message_update assistant",
                "message_end assistant",
                "tool_execution_start",
                "tool_execution_update",
                "tool_execution_end",
                "message_start toolResult",
                "message_end toolResult",
                "turn_end assistant",
                "turn_start",
                "message_start assistant",
                "message_update assistant",
                "message_end assistant",
                "turn_end assistant",
                "agent_end",
            ]
            .map(str::to_string)
            .to_vec(),
            json!({ "type": "start" }),
            true,
            true,
            true,
            (json!("eval"), None, json!(false)),
        ),
        "the stream: {stream:?}"
    );
}

/// The queue view (phase 8b-3), as the old worker sends it: a prompt that
/// runs at once is never listed; one queued behind the running turn is
/// listed in its lane, then is the active turn as it leaves the lane,
/// `committing` once its row settles and `running` from its reply's first
/// frame, and the view ends idle before its `done`. Each view is one
/// numbered `session_action_update`.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_queued_behind_a_turn_streams_the_old_queue_view_through_its_phases() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("Sleeping.", "(bash \"sleep 2\" (hash 'wait-ms #f))\n(displayln \"slept\")"),
        "slept",
        "the queued reply",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    session.publish("sleep first", None).await;
    session.until_streamed("tool_execution_start").await;
    let queued = session.publish("then this", Some("follow_up")).await;
    session.done(&queued["task"]).await;

    let stream = session.payloads("stream");
    let views: Vec<Value> = stream
        .iter()
        .filter(|event| event["type"] == "session_action_update")
        .map(|event| event["actions"].clone())
        .collect();
    // The frame each view follows.
    let after = |view: &Value| -> String {
        let at = stream
            .iter()
            .position(|event| event["actions"] == *view)
            .unwrap_or_else(|| panic!("{view} is not among the views {views:?}"));
        let event = &stream[at - 1];
        format!(
            "{} {}",
            event["type"].as_str().unwrap_or_default(),
            event["message"]["role"].as_str().unwrap_or_default()
        )
    };
    let active = |phase: &str| {
        json!({ "queuedCount": 0, "steering": [], "followUps": [],
            "active": { "kind": "turn", "phase": phase, "label": "then this" } })
    };
    assert_eq!(
        (
            views.clone(),
            after(&active("committing")),
            after(&active("running")),
            stream
                .iter()
                .filter(|event| event["type"] == "session_action_update")
                .all(|event| event["meta"]["sequence"].is_u64()),
        ),
        (
            vec![
                json!({ "queuedCount": 1, "steering": [], "followUps": ["then this"] }),
                active("preparing"),
                active("committing"),
                active("running"),
                json!({ "queuedCount": 0, "steering": [], "followUps": [] }),
            ],
            "message_end user".to_string(),
            "message_start assistant".to_string(),
            true,
        ),
        "the stream: {stream:?}"
    );
}

/// A steer the running turn takes is listed under `steering` while it waits
/// on the agent, as the old worker's view listed its agent's steers, and
/// leaves the view once its row settles: a client never reads the session
/// as idle while a steer it sent is still to run. It arrives after the run's
/// first reply, so the running turn reads it at its next steering point.
#[tokio::test(flavor = "multi_thread")]
async fn a_steer_the_running_turn_takes_is_listed_until_its_row_settles() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("First.", "(displayln \"one\")"),
        cell("Sleeping.", "(bash \"sleep 2\" (hash 'wait-ms #f))\n(displayln \"slept\")"),
        "the steered reply",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let prompt = session.publish("sleep first", None).await;
    session
        .until(|session| {
            session
                .payloads("stream")
                .iter()
                .filter(|event| event["type"] == "tool_execution_start")
                .count()
                == 2
        })
        .await;
    let steer = session.publish("steer this", Some("steer")).await;
    session.done(&prompt["task"]).await;

    let stream = session.payloads("stream");
    let views: Vec<(Value, String)> = stream
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "session_action_update")
        .map(|(at, event)| {
            let before = &stream[at - 1];
            (
                event["actions"].clone(),
                format!(
                    "{} {}",
                    before["type"].as_str().unwrap_or_default(),
                    before["message"]["role"].as_str().unwrap_or_default()
                ),
            )
        })
        .collect();
    assert_eq!(
        (
            steer["admission"].clone(),
            views
                .iter()
                .map(|(view, _)| view.clone())
                .collect::<Vec<_>>(),
            views.last().map(|(_, after)| after.clone()),
        ),
        (
            json!("steered"),
            vec![
                json!({ "queuedCount": 1, "steering": ["steer this"], "followUps": [] }),
                json!({ "queuedCount": 0, "steering": [], "followUps": [] }),
            ],
            Some("message_end user".to_string()),
        ),
        "the stream: {stream:?}"
    );
}

/// A model call that fails to start (its stream function returns an error:
/// here a model on an api no provider serves) streams its failed reply: a
/// settled assistant row with the error, which the TUI draws as the turn's
/// error row, as the old worker's failed turn showed one. The run failure's
/// row converts to the wire like any other.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_call_that_fails_to_start_streams_its_error_row() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        bed.agent_dir().join("models.json"),
        json!({ "providers": { "unserved": {
            "api": "no-such-api", "baseUrl": "http://localhost:0", "apiKey": "sk-unserved",
            "models": [{ "id": "unserved-1", "name": "Unserved", "api": "no-such-api",
                "baseUrl": "http://localhost:0", "contextWindow": 128_000, "maxTokens": 4_096 }],
        } } })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["never served"] }));
    let mut session = Attached::open(&bed, &cwd).await;
    session
        .call(
            "session.set_model",
            json!({ "provider": "unserved", "modelId": "unserved-1" }),
        )
        .await;
    let prompt = session.publish("hello", None).await;
    session.done(&prompt["task"]).await;
    let failed: Vec<(Value, bool)> = session
        .payloads("stream")
        .iter()
        .filter(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
        .map(|event| {
            (
                event["message"]["stopReason"].clone(),
                event["message"]["errorMessage"]
                    .as_str()
                    .is_some_and(|text| text.contains("no-such-api")),
            )
        })
        .collect();
    assert_eq!(
        failed,
        vec![(json!("error"), true)],
        "one settled failed reply naming the failure: {:?}",
        session.payloads("stream")
    );
}
