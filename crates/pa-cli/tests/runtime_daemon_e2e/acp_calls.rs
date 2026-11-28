//! The rest of what the ACP client asks of the runtime daemon (phase 7g-5),
//! on wire2:
//!
//! - `session.pause_input` holds a session's input for the calling
//!   connection (input is admitted `parked`), the same key answering the same
//!   pause; `session.release_input` lets it run, and so does the holding
//!   connection's end; another connection's release of it is refused, as
//!   the old daemon's is;
//! - `session.cancel_task` takes one queued prompt out of the lane, which
//!   ends with its cancelled `done`;
//! - `session.wait_idle` with `"subagents": false` answers the autonomous
//!   accounting and the live background jobs;
//! - `session.model` answers the model, level and supported levels, and
//!   `session.set_effort` clamps to them;
//! - `session.children` and `session.cancel_child` answer a session's
//!   subagent roster;
//! - `session.create {"ephemeral": true}` (ACP's `--no-session`, phase 7g-6)
//!   runs turns without a transcript file and is gone once stopped.

use pa_types::event_log::Topic;
use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::{json, Value};

use super::{ok, refused, run_turn, Bed, Client};

/// Publishes `text` as session `id`'s input; the publish's reply.
async fn publish(client: &mut Client, id: &str, text: &str) -> Value {
    let request = client.id();
    client
        .send(&ClientFrame::Publish {
            id: request.clone(),
            topic: Topic::new(format!("agent/{id}/input")).unwrap(),
            payload: json!({ "text": text }),
        })
        .await;
    ok(client.reply(&request, &mut Vec::new()).await)
}

/// Reads frames until task `task`'s `agent/<id>/done`; its payload.
async fn done_of(client: &mut Client, id: &str, task: &Value) -> Value {
    let topic = format!("agent/{id}/done");
    loop {
        if let ServerFrame::Event { event, .. } = client.frame().await {
            if event.event.topic.to_string() == topic && event.event.payload["task"] == *task {
                return event.event.payload;
            }
        }
    }
}

/// The user prompts session `id`'s transcript holds.
async fn user_prompts(client: &mut Client, id: &str) -> Vec<Value> {
    let page = ok(client
        .query("transcript.page", json!({ "session": id }))
        .await);
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["type"] == "message" && row["message"]["role"] == "user")
        .map(|row| match &row["message"]["content"] {
            Value::String(text) => json!(text),
            blocks => blocks[0]["text"].clone(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_input_hold_parks_the_lane_until_released_or_its_connection_ends() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    let (mut holder, _) = Client::connect(&bed.runtime_socket()).await;
    let pause = |key: &str| json!({ "session": id, "key": key });
    let held = ok(holder.call("session.pause_input", pause("stop")).await);
    let again = ok(holder.call("session.pause_input", pause("stop")).await);
    let parked = publish(&mut client, &id, "first").await;
    let lane = ok(client.query("tasks", json!({ "session": id })).await);
    let released = ok(holder
        .call(
            "session.release_input",
            json!({ "session": id, "pauseId": held["pauseId"] }),
        )
        .await);
    let first = done_of(&mut client, &id, &parked["task"]).await;
    assert_eq!(
        (
            again["pauseId"] == held["pauseId"],
            parked["admission"].clone(),
            lane["queued"].as_array().map(Vec::len),
            released["released"].clone(),
            first["outcome"].clone(),
        ),
        (true, json!("parked"), Some(1), json!(true), json!("done"))
    );
    // Held again, the holder leaves without releasing: its end releases it.
    ok(holder.call("session.pause_input", pause("stop")).await);
    let parked = publish(&mut client, &id, "second").await;
    drop(holder);
    let second = done_of(&mut client, &id, &parked["task"]).await;
    assert_eq!(
        (parked["admission"].clone(), second["outcome"].clone()),
        (json!("parked"), json!("done"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn another_connection_cannot_release_an_input_hold() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut holder, _) = Client::connect(&bed.runtime_socket()).await;
    let held = ok(holder
        .call("session.pause_input", json!({ "session": id, "key": "k" }))
        .await);
    let release = json!({ "session": id, "pauseId": held["pauseId"] });
    let foreign = refused(client.call("session.release_input", release.clone()).await);
    let parked = publish(&mut client, &id, "still held").await;
    let owned = ok(holder.call("session.release_input", release).await);
    assert_eq!(
        (
            foreign,
            parked["admission"].clone(),
            owned["released"].clone()
        ),
        (
            format!(
                "Session input pause is owned by another client: {}",
                held["pauseId"].as_str().unwrap()
            ),
            json!("parked"),
            json!(true)
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_task_wait_idle_model_effort_and_the_roster_answer_the_acp_client() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    let session = json!({ "session": id });

    // A queued prompt cancelled: it never runs; the next one does.
    let held = ok(client
        .call("session.pause_input", json!({ "session": id, "key": "k" }))
        .await);
    let dropped = publish(&mut client, &id, "never").await;
    let cancel = json!({ "session": id, "task": dropped["task"] });
    // Its `done` can come ahead of the cancel's reply.
    let mut early = Vec::new();
    let cancelled = ok(client
        .call_keeping("session.cancel_task", cancel.clone(), &mut early)
        .await);
    let again = ok(client
        .call_keeping("session.cancel_task", cancel, &mut early)
        .await);
    let done_topic = format!("agent/{id}/done");
    let early_done = early.into_iter().find_map(|frame| match frame {
        ServerFrame::Event { event, .. }
            if event.event.topic.to_string() == done_topic
                && event.event.payload["task"] == dropped["task"] =>
        {
            Some(event.event.payload)
        }
        _ => None,
    });
    let dropped_done = match early_done {
        Some(done) => done,
        None => done_of(&mut client, &id, &dropped["task"]).await,
    };
    ok(client
        .call(
            "session.release_input",
            json!({ "session": id, "pauseId": held["pauseId"] }),
        )
        .await);
    let kept = publish(&mut client, &id, "kept").await;
    done_of(&mut client, &id, &kept["task"]).await;
    let idle = ok(client
        .call(
            "session.wait_idle",
            json!({ "session": id, "subagents": false }),
        )
        .await);
    assert_eq!(
        (
            cancelled["cancelled"].clone(),
            again["cancelled"].clone(),
            dropped_done["outcome"].clone(),
            user_prompts(&mut client, &id).await,
            idle["autonomous"]["enabled"].clone(),
            idle["outstandingBackgroundJobs"].clone(),
        ),
        (
            json!(true),
            json!(false),
            json!("cancelled"),
            vec![json!("kept")],
            json!(false),
            json!(0)
        )
    );

    // The faux model does not think: its only level is `off`, and a level
    // asked for is clamped to it.
    let effort = ok(client
        .call(
            "session.set_effort",
            json!({ "session": id, "level": "high" }),
        )
        .await);
    let model = ok(client.query("session.model", session.clone()).await);
    assert_eq!(
        (
            effort["level"].clone(),
            model["model"]["provider"].clone(),
            model["thinkingLevel"].clone(),
            model["availableThinkingLevels"].clone(),
        ),
        (json!("off"), json!("faux"), json!("off"), json!(["off"]))
    );

    // No subagents: an empty roster, and a cancel of an unknown child is
    // answered, not refused.
    let children = ok(client.query("session.children", session).await);
    let child = ok(client
        .call(
            "session.cancel_child",
            json!({ "session": id, "child": "nobody" }),
        )
        .await);
    assert_eq!(
        (children["children"].clone(), child["cancelled"].clone()),
        (json!([]), json!(false))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_session_keeps_no_transcript_and_is_gone_once_stopped() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client
        .call("session.create", json!({ "ephemeral": true }))
        .await);
    let id = created["session"].as_str().unwrap().to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "hello").await;
    let model = ok(client
        .query("session.model", json!({ "session": id }))
        .await);
    let open = ok(client.query("sessions.list", json!({ "open": true })).await);
    ok(client.call("session.stop", json!({ "session": id })).await);
    let listed = ok(client.query("sessions.list", json!({})).await);
    let with_path = refused(
        client
            .call(
                "session.create",
                json!({ "ephemeral": true, "path": created["path"] }),
            )
            .await,
    );
    let listed_ids = |list: &Value| -> Vec<Value> {
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].clone())
            .collect()
    };
    assert_eq!(
        (
            super::support::saved_transcripts(&bed.agent_dir()).is_empty(),
            model["model"]["provider"].clone(),
            listed_ids(&open),
            listed_ids(&listed).contains(&json!(id)),
            with_path.contains("no `path`"),
        ),
        (true, json!("faux"), vec![json!(id)], false, true)
    );
}
