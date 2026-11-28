//! What an ACP client needs of the runtime daemon (phase 7g-2), on wire2:
//!
//! - `session.create` with `options` builds the session with the client's
//!   launch choices (here an appended system prompt), and a session created
//!   again without any keeps the ones it was created with;
//! - `models` lists the signed-in models, and `session.set_model` switches
//!   a running session within the settings' `allowedModels` (a malformed
//!   one refuses every model);
//! - `session.set_mcp_servers` admits one owner's servers and fences them
//!   from another's;
//! - a scheduled job made or cancelled publishes `agent/<id>/schedules`, and
//!   the `sources`, `subscriptions` and `settings` queries answer;
//! - `session.rename` answers the renamed session's summary row, as the old
//!   daemon's rename answers its summary (`prime-agent rename --json` prints
//!   it), and tells the session's followers with the old
//!   `session_info_changed`.

use pa_types::wire2::{ServerFrame, Since};
use serde_json::{json, Value};

use super::{ok, refused, run_turn, Bed, Client};

/// The system prompt tokens the next request carries (both layers).
async fn system_tokens(client: &mut Client, id: &str) -> u64 {
    let breakdown = ok(client
        .query("session.breakdown", json!({ "session": id }))
        .await);
    let categories = &breakdown["categories"];
    let (fixed, dynamic) = categories["systemStatic"]["tokens"]
        .as_u64()
        .zip(categories["systemDynamic"]["tokens"].as_u64())
        .unwrap_or_else(|| panic!("a measured breakdown: {breakdown}"));
    fixed + dynamic
}

#[tokio::test(flavor = "multi_thread")]
async fn create_options_shape_the_build_and_outlive_a_stop() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let plain = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    let appended = "a client-chosen instruction ".repeat(150);
    let chosen = ok(client
        .call(
            "session.create",
            json!({ "options": { "appendSystemPrompt": [appended], "executionMode": "acp" } }),
        )
        .await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    let baseline = system_tokens(&mut client, &plain).await;
    let with_options = system_tokens(&mut client, &chosen).await;
    // chars/4: the appended text is about 1,000 tokens.
    assert!(
        with_options >= baseline + 900,
        "{baseline} -> {with_options}"
    );
    // Stopped and created again without options: built as it was created.
    ok(client
        .call("session.stop", json!({ "session": chosen }))
        .await);
    ok(client
        .call("session.create", json!({ "session": chosen }))
        .await);
    assert_eq!(system_tokens(&mut client, &chosen).await, with_options);
    // An option the daemon does not know is refused, not ignored.
    assert!(refused(
        client
            .call("session.create", json!({ "options": { "temperature": 2 } }))
            .await
    )
    .contains("unknown session option `temperature`"));
}

/// A faux-provider model the session can switch to (the stable `faux`
/// api streams it through the same scripted provider).
fn faux_models() -> Value {
    json!({
        "providers": {
            "faux": {
                "api": "faux",
                "baseUrl": "http://localhost:0",
                "apiKey": "sk-faux",
                "models": [
                    { "id": "faux-1", "name": "Faux Model", "api": "faux",
                      "baseUrl": "http://localhost:0", "contextWindow": 128_000, "maxTokens": 4_096 },
                    { "id": "plain-model", "name": "Plain Model", "api": "faux",
                      "baseUrl": "http://localhost:0", "contextWindow": 128_000, "maxTokens": 4_096 },
                ],
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_session_switches_model_within_the_allowlist() {
    let bed = Bed::new();
    std::fs::write(
        bed.agent_dir().join("models.json"),
        faux_models().to_string(),
    )
    .unwrap();
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "allowedModels": ["faux/faux-1", "faux/plain-model"] }).to_string(),
    )
    .unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let models = ok(client.query("models", json!({})).await);
    let ids: Vec<&str> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(ids.contains(&"plain-model"), "{models}");
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    let switched = ok(client
        .call(
            "session.set_model",
            json!({ "session": id, "provider": "faux", "modelId": "plain-model" }),
        )
        .await);
    assert_eq!(
        (switched["provider"].clone(), switched["modelId"].clone()),
        (json!("faux"), json!("plain-model"))
    );
    let context = ok(client
        .query("session.context", json!({ "session": id }))
        .await);
    assert_eq!(
        context["model"],
        json!({ "provider": "faux", "modelId": "plain-model" }),
        "{context}"
    );
    // The switched session still runs its turns.
    run_turn(&mut client, &id, "after the switch").await;
    // Off the allowlist: refused, the session's model unchanged.
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "allowedModels": ["faux/faux-1"] }).to_string(),
    )
    .unwrap();
    assert!(refused(
        client
            .call(
                "session.set_model",
                json!({ "session": id, "provider": "faux", "modelId": "plain-model" }),
            )
            .await
    )
    .contains("blocked by the daemon model allowlist"));
    // A malformed allowlist is an unknown policy: refused, never open.
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "allowedModels": "faux/*" }).to_string(),
    )
    .unwrap();
    assert!(refused(
        client
            .call(
                "session.set_model",
                json!({ "session": id, "provider": "faux", "modelId": "faux-1" }),
            )
            .await
    )
    .contains("allowedModels is present but is not an array of strings"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_servers_are_fenced_by_their_owner() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    let servers = json!([{
        "type": "stdio", "name": "echo", "command": "/bin/cat", "args": [],
        "cwd": bed.agent_dir(), "env": { "MODE": "test" },
    }]);
    let set =
        |owner: &str, servers: &Value| json!({ "session": id, "owner": owner, "servers": servers });
    assert_eq!(
        ok(client
            .call("session.set_mcp_servers", set("acp-1", &servers))
            .await),
        json!({ "session": id, "changed": true, "servers": ["echo"] })
    );
    assert!(refused(
        client
            .call("session.set_mcp_servers", set("acp-2", &servers))
            .await
    )
    .contains("owned by another client"));
    // Another owner's clear changes nothing; the owner's clears them.
    assert_eq!(
        ok(client
            .call("session.set_mcp_servers", set("acp-2", &json!([])))
            .await),
        json!({ "session": id, "changed": false, "servers": ["echo"] })
    );
    assert_eq!(
        ok(client
            .call("session.set_mcp_servers", set("acp-1", &json!([])))
            .await),
        json!({ "session": id, "changed": true, "servers": [] })
    );
}

/// The payload of the next `agent/<id>/schedules` event: among `early`, the
/// frames that came before the changing call's reply, or else read after.
async fn schedules_change(client: &mut Client, id: &str, early: Vec<ServerFrame>) -> Value {
    let topic = format!("agent/{id}/schedules");
    let change = |frame: ServerFrame| match frame {
        ServerFrame::Event { event, .. } if event.event.topic.to_string() == topic => {
            Some(event.event.payload)
        }
        _ => None,
    };
    // The lane's pass can publish before the call that changed the jobs
    // answers, so the frames that came before its reply are looked at first.
    if let Some(payload) = early.into_iter().find_map(&change) {
        return payload;
    }
    loop {
        if let Some(payload) = change(client.frame().await) {
            return payload;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_change_is_published_and_the_bus_queries_show_its_timer() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .subscribe(&format!("agent/{id}/schedules"), Since::Now)
        .await;
    let mut early = Vec::new();
    let job = ok(client
        .call_keeping(
            "schedule.add",
            json!({ "session": id, "schedule": "every 1h", "prompt": "check in" }),
            &mut early,
        )
        .await)["job"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        schedules_change(&mut client, &id, early).await,
        json!({ "active": 1 })
    );
    let sources = ok(client.query("sources", json!({})).await);
    let timer = sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["name"] == format!("schedule-{job}"))
        .unwrap_or_else(|| panic!("the job's timer: {sources}"))
        .clone();
    // Whether it runs yet is the source task's (it starts as it hears the
    // subscription), so only what is kept is checked.
    assert_eq!(
        (timer["kind"].clone(), timer["kept"].clone()),
        (json!("timer"), json!(true))
    );
    let subscriptions = ok(client.query("subscriptions", json!({})).await);
    assert!(
        subscriptions["subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|subscription| subscription["id"] == format!("schedule:{job}")),
        "{subscriptions}"
    );
    let mut early = Vec::new();
    ok(client
        .call_keeping("schedule.cancel", json!({ "job": job }), &mut early)
        .await);
    assert_eq!(
        schedules_change(&mut client, &id, early).await,
        json!({ "active": 0 })
    );
    let settings = ok(client.query("settings", json!({ "session": id })).await);
    assert_eq!(
        (settings["settings"].is_object(), settings["errors"].clone()),
        (true, json!([]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_answers_the_renamed_sessions_summary() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .subscribe(&format!("agent/{id}/stream"), Since::Now)
        .await;
    let mut frames = Vec::new();
    let renamed = ok(client
        .call_keeping(
            "session.rename",
            json!({ "session": id, "name": "renamed" }),
            &mut frames,
        )
        .await);
    let changed = |frames: &[ServerFrame]| {
        frames.iter().find_map(|frame| match frame {
            ServerFrame::Event { event, .. }
                if event.event.payload["type"] == "session_info_changed" =>
            {
                Some(event.event.payload.clone())
            }
            _ => None,
        })
    };
    let info = loop {
        if let Some(info) = changed(&frames) {
            break info;
        }
        frames.push(client.frame().await);
    };
    let listed = ok(client.query("sessions.list", json!({ "open": true })).await);
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        (
            renamed["activeSessionId"].clone(),
            renamed["sessionName"].clone(),
            renamed.clone(),
            info["name"].clone()
        ),
        (json!(id), json!("renamed"), row, json!("renamed"))
    );
}
