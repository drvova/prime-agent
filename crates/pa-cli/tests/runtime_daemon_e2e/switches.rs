//! A session's setting switches on the runtime daemon (phase 8d-1): the
//! model and thinking-level cycles, the scoped model list, the service tier,
//! the transport, the queue modes, auto-compaction and auto-retry, sent
//! through pa-tui's `wire2_compat` (`pa_tui::wire2_request_frames`), answer
//! as the old daemon (the `pa-daemon` supervisor) answers the same commands
//! on a session over the same models, leave the same settings document and
//! record the same model, level and tier rows; a change of the active tier
//! is the old `service_tier_changed` event on the session's stream.

use std::path::Path;

use pa_types::daemon::{DaemonCommand, DaemonOutbound};
use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::{json, Value};

use super::saved_list::old_list;
use super::{ok, Bed, Client};

/// Two signed-in providers: `battery` (a model that does not think and keeps
/// only the default tier) and `openai` (`gpt-5.5`, which thinks and keeps the
/// priority tier).
fn models(agent_dir: &Path) {
    let model = |id: &str, api: &str, reasoning: bool| {
        json!({ "id": id, "name": id, "api": api, "reasoning": reasoning,
                "contextWindow": 128_000, "maxTokens": 4096 })
    };
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": {
            "battery": { "api": "openai-completions", "baseUrl": "http://127.0.0.1:9",
                "apiKey": "sk-battery",
                "models": [model("mock-1", "openai-completions", false),
                           model("mock-2", "openai-completions", false)] },
            "openai": { "api": "openai-responses", "baseUrl": "http://127.0.0.1:9",
                "apiKey": "sk-openai",
                "models": [model("gpt-5.5", "openai-responses", true)] },
        } })
        .to_string(),
    )
    .unwrap();
}

/// The switches, in order, without their session: refusals among them.
fn switches() -> Vec<Value> {
    vec![
        json!({ "type": "set_scoped_models", "scopedModels": [{ "thinkingLevel": "high" }] }),
        json!({ "type": "set_scoped_models", "scopedModels": [
            { "model": { "provider": "battery", "id": "mock-1" } },
            { "model": { "provider": "openai", "id": "gpt-5.5" }, "thinkingLevel": "high" },
        ] }),
        json!({ "type": "set_service_tier", "serviceTier": null }),
        json!({ "type": "set_service_tier", "serviceTier": "priority" }),
        json!({ "type": "cycle_model" }),
        json!({ "type": "cycle_thinking_level" }),
        json!({ "type": "cycle_model", "direction": "backward" }),
        json!({ "type": "cycle_thinking_level" }),
        json!({ "type": "set_transport", "transport": "sse" }),
        json!({ "type": "set_steering_mode", "mode": "sometimes" }),
        json!({ "type": "set_steering_mode", "mode": "one-at-a-time" }),
        json!({ "type": "set_follow_up_mode", "mode": "all" }),
        json!({ "type": "set_auto_compaction", "enabled": false }),
        json!({ "type": "set_auto_retry", "enabled": false }),
    ]
}

/// `frame` in the shape the TUI's client reads it in, without its request
/// id (the compat layer's caller pairs it).
fn read_as_the_tui_does(frame: &Value) -> Value {
    let typed: DaemonOutbound = serde_json::from_value(frame.clone())
        .unwrap_or_else(|error| panic!("{frame} is not an old frame: {error}"));
    let mut frame = serde_json::to_value(typed).unwrap();
    frame.as_object_mut().unwrap().remove("id");
    frame
}

/// The model, level and tier rows of session `id`'s transcript, without
/// their ids and times, as `transcript.page` serves them.
async fn switch_rows(client: &mut Client, id: &str) -> Vec<Value> {
    let page = ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await);
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            matches!(
                row["type"].as_str(),
                Some("model_change" | "thinking_level_change" | "service_tier_change")
            )
        })
        .map(|row| {
            let mut row = row.clone();
            for key in ["id", "parentId", "timestamp"] {
                row.as_object_mut().unwrap().remove(key);
            }
            row
        })
        .collect()
}

/// The global settings document, read and then removed, so the next daemon
/// starts from none.
fn take_settings(agent_dir: &Path) -> Value {
    let path = agent_dir.join("settings.json");
    let settings = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    std::fs::remove_file(path).unwrap();
    settings
}

/// The old daemon's responses to the switches on a session it creates on
/// `battery/mock-1`, and the session's id.
fn old_answers(bed: &Bed) -> (Vec<Value>, String) {
    let _daemon = bed.old_daemon();
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "config": {
            "cwd": bed.dir.path(), "sessionDir": bed.agent_dir().join("sessions"),
            "provider": "battery", "model": "mock-1",
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let saved = created["data"]["sessionId"].as_str().unwrap().to_string();
    let answers = switches()
        .into_iter()
        .enumerate()
        .map(|(index, mut command)| {
            command["activeSessionId"] = json!(active);
            let frames = old_list(&socket, &format!("w{index}"), &command);
            read_as_the_tui_does(frames.last().unwrap())
        })
        .collect();
    (answers, saved)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_setting_switches_answer_and_write_as_the_old_daemons_do() {
    let bed = Bed::new();
    models(&bed.agent_dir());
    let (old, old_id) = old_answers(&bed);
    let old_settings = take_settings(&bed.agent_dir());

    let _daemon = bed.runtime_daemon(&json!({ "responses": ["hi"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    ok(client
        .call(
            "session.set_model",
            json!({ "session": id, "provider": "battery", "modelId": "mock-1" }),
        )
        .await);
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    let mut answers = Vec::new();
    let mut events = Vec::new();
    for (index, mut command) in switches().into_iter().enumerate() {
        command["activeSessionId"] = json!(id);
        let kind = command["type"].as_str().unwrap().to_string();
        let command: DaemonCommand = serde_json::from_value(command).unwrap();
        let mut frames =
            pa_tui::wire2_request_frames(format!("w{index}"), command).expect("a mapped command");
        let (Some(ClientFrame::Call { method, args, .. }), None) = (frames.pop(), frames.pop())
        else {
            panic!("{kind} is one call");
        };
        // The stream's events travel apart from the replies: kept as they
        // arrive.
        let outcome = client.call_keeping(&method, args, &mut events).await;
        answers.push(read_as_the_tui_does(
            &pa_tui::wire2_response_frame(&kind, outcome).unwrap(),
        ));
    }
    assert_eq!(answers, old);
    // The active tier moved with the model, and only then: priority on
    // gpt-5.5 (the first cycle), back to default on mock-1 (the second).
    let tiers = |events: &[ServerFrame]| {
        events
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event { event, .. }
                    if event.event.payload["type"] == "service_tier_changed" =>
                {
                    Some(event.event.payload["serviceTier"].clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    while tiers(&events).len() < 2 {
        events.push(client.frame().await);
    }
    assert_eq!(tiers(&events), [json!("priority"), json!("default")]);
    // A transcript the session has not written a turn to is kept in memory:
    // one turn stores it.
    super::run_turn(&mut client, &id, "hello").await;
    // Both sessions record their start (model, level, tier); the runtime's
    // started on the faux model, and `session.set_model` then moved it to
    // the one the old daemon's started on. From there the rows are the old
    // daemon's.
    let rows = switch_rows(&mut client, &id).await;
    let old_rows = switch_rows(&mut client, &old_id).await;
    let faux = json!({ "type": "model_change", "provider": "faux", "modelId": "faux-1" });
    assert_eq!(
        (&rows[..4], &rows[4..]),
        (
            &[
                faux,
                old_rows[1].clone(),
                old_rows[2].clone(),
                old_rows[0].clone()
            ][..],
            &old_rows[3..]
        )
    );
    assert_eq!(take_settings(&bed.agent_dir()), old_settings);
}
