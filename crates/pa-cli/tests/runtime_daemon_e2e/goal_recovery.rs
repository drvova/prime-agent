//! A goal survives its session's rebuild on the runtime daemon (the
//! supervisor's `goal_recovery_e2e`): the transcript keeps the
//! `thread_goal_state` rows a started goal writes, and a session whose
//! lifetime crashed mid-goal is rebuilt with the goal rehydrated from them
//! (its objective, status and continuation count), never a fresh engine's
//! empty goal; a turn after the rebuild counts on from that base. A prompt
//! queued while the goal's continuations run (the pause) runs once the turn
//! it waits behind ends: no continuation runs ahead of it, as the old
//! daemon's queued session input owned the turn's end.

use serde_json::{json, Value};

use super::turns::Attached;
use super::Bed;

const OBJECTIVE: &str = "ship the goal-recovery port";

/// How long a prompt queued behind a running goal may take to run.
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The data of the session's last `thread_goal_state` row.
async fn latest_goal_row(session: &mut Attached) -> Value {
    let page = session
        .query_with("transcript.page", json!({ "limit": 500 }))
        .await;
    page["entries"]
        .as_array()
        .expect("a page of entries")
        .iter()
        .rfind(|row| row["type"] == "custom" && row["customType"] == "thread_goal_state")
        .map_or(Value::Null, |row| row["data"].clone())
}

/// The goal the session's connection state serves (the old
/// `get_connection_state`'s).
async fn served_goal(session: &mut Attached) -> Value {
    session
        .query_with("session.snapshot", json!({ "shape": "connection" }))
        .await["goal"]
        .clone()
}

/// Runs `text` as a prompt and waits until the session is idle again. (A
/// started or resumed goal mints continuation turns until a pause follows
/// it: only a pause settles.)
async fn settle(session: &mut Attached, text: &str) {
    let task = session.publish(text, None).await;
    // The goal's continuations stream while the prompt waits: the wait is
    // for the prompt's own `done`, which a loop that never yields to the
    // lane never sends.
    tokio::time::timeout(SETTLE_WAIT, session.done(&task["task"]))
        .await
        .unwrap_or_else(|_| panic!("{text:?} never ran behind the goal's continuations"));
    session.call("session.wait_idle", json!({})).await;
}

/// Reads frames until the stream announced a goal that used `count`
/// continuations.
async fn until_minted(session: &mut Attached, count: u64) {
    session
        .until(|session| {
            session.payloads("stream").iter().any(|event| {
                event["type"] == "goal_update"
                    && event["goal"]["continuationsUsed"].as_u64() >= Some(count)
            })
        })
        .await;
}

/// The fields of a goal the rebuild must keep.
fn kept(goal: &Value) -> Value {
    json!({
        "objective": goal["objective"],
        "status": goal["status"],
        "continuationsUsed": goal["continuationsUsed"],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_rebuilt_mid_goal_rehydrates_the_goal_with_its_counts() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({
        "responses": ["working on it"],
        "repeatLastResponse": true,
        "panicOn": { "message": "crash now", "panic": "injected fault" },
    }));
    let mut session = Attached::open(&bed, &cwd).await;

    // A started goal mints its continuations until a pause stops the loop;
    // the pause is durable.
    session.publish(&format!("/goal {OBJECTIVE}"), None).await;
    until_minted(&mut session, 1).await;
    settle(&mut session, "/goal pause").await;
    let before = latest_goal_row(&mut session).await;
    let count = before["continuationsUsed"].as_u64().unwrap_or_default();

    // The crash ends the session's lifetime; the rebuild reads the goal back
    // from the transcript.
    let crashed = session.publish("crash now", None).await;
    session.done(&crashed["task"]).await;
    session
        .until(|session| !session.payloads("recovered").is_empty())
        .await;
    let rebuilt = served_goal(&mut session).await;

    // A resumed goal counts on from the rehydrated base.
    session.publish("/goal resume", None).await;
    until_minted(&mut session, count + 1).await;
    settle(&mut session, "/goal pause").await;
    let after = latest_goal_row(&mut session).await;
    assert_eq!(
        (
            kept(&before),
            count >= 1,
            kept(&rebuilt),
            after["objective"].clone(),
            after["status"].clone(),
            after["continuationsUsed"].as_u64().unwrap_or_default() > count,
        ),
        (
            json!({ "objective": OBJECTIVE, "status": "paused", "continuationsUsed": count }),
            true,
            json!({ "objective": OBJECTIVE, "status": "paused", "continuationsUsed": count }),
            json!(OBJECTIVE),
            json!("paused"),
            true,
        ),
        "the goal before the crash {before}, served after it {rebuilt}, and after a resume {after}"
    );
}
