//! What the TUI reads beside a session's turns on the runtime daemon
//! (phase 8d-1b): `get_commands`, `get_mcp_connections`,
//! `get_model_catalog`, the global `get_usage` (both halves), `reload` and
//! `mark_anthropic_warning_shown`, sent through pa-tui's `wire2_compat`,
//! answer as the old daemon (the `pa-daemon` supervisor) answers the same
//! commands on a session over the same agent dir; a reload reads a prompt
//! template written after the session opened on both, and the warning's
//! marker row is written once on both.

use std::path::Path;

use pa_types::daemon::DaemonCommand;
use pa_types::wire2::{ClientFrame, Since};
use serde_json::{json, Value};

use super::saved_list::old_list;
use super::switches::{models, read_as_the_tui_does};
use super::{ok, Bed, Client};

/// The reads and changes before the reload, in order, without their
/// session: a mark twice (the second writes nothing).
fn before_reload() -> Vec<Value> {
    vec![
        json!({ "type": "get_commands" }),
        json!({ "type": "get_mcp_connections" }),
        json!({ "type": "get_model_catalog" }),
        json!({ "type": "get_usage", "section": "spend" }),
        json!({ "type": "get_usage", "section": "quotas", "forceRefresh": true }),
        json!({ "type": "mark_anthropic_warning_shown" }),
        json!({ "type": "mark_anthropic_warning_shown" }),
    ]
}

/// The reload, once a template was written after the session opened, and
/// the catalog it reads.
fn after_reload() -> Vec<Value> {
    vec![
        json!({ "type": "reload" }),
        json!({ "type": "get_commands" }),
    ]
}

/// The agent dir's resources and history: a prompt template and a skill of
/// the user's, and a saved session whose one reply billed a model call
/// today (the spend history's row).
fn resources(agent_dir: &Path, cwd: &Path) {
    std::fs::create_dir_all(agent_dir.join("prompts")).unwrap();
    std::fs::write(
        agent_dir.join("prompts/review.md"),
        "---\ndescription: Review the change\nargument-hint: <path>\n---\nReview $1.\n",
    )
    .unwrap();
    std::fs::create_dir_all(agent_dir.join("skills/demo")).unwrap();
    std::fs::write(
        agent_dir.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: A demo skill.\n---\nDo the demo.\n",
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let now = u64::try_from(now).unwrap();
    let at = pa_core::session::manager::iso_from_unix_ms(now);
    let id = "0199a000-0000-7000-8000-000000000001";
    let rows = [
        json!({ "type": "session", "version": 3, "id": id, "timestamp": at, "cwd": cwd }),
        json!({ "type": "message", "id": "u1", "parentId": null, "timestamp": at,
                "message": { "role": "user", "content": [{ "type": "text", "text": "q" }],
                             "timestamp": now } }),
        json!({ "type": "message", "id": "a1", "parentId": "u1", "timestamp": at,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "a" }],
                    "api": "openai-completions", "provider": "battery", "model": "mock-1",
                    "usage": { "input": 120, "output": 30, "cacheRead": 0, "cacheWrite": 0,
                               "totalTokens": 150, "cost": { "input": 0.01, "output": 0.02,
                               "cacheRead": 0, "cacheWrite": 0, "total": 0.03 } },
                    "stopReason": "stop", "timestamp": now + 1 } }),
    ];
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
    std::fs::write(sessions.join(format!("{id}.jsonl")), text).unwrap();
}

/// A template written after the session opened, which only a reload reads.
fn late_template(agent_dir: &Path) -> std::path::PathBuf {
    let path = agent_dir.join("prompts/late.md");
    std::fs::write(&path, "---\ndescription: Written late\n---\nLate.\n").unwrap();
    path
}

/// The old daemon's responses on a session it creates on `battery/mock-1`,
/// and the session's id.
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
    let mut answers = Vec::new();
    let mut send = |commands: Vec<Value>, answers: &mut Vec<Value>| {
        for (index, mut command) in commands.into_iter().enumerate() {
            command["activeSessionId"] = json!(active);
            let frames = old_list(&socket, &format!("i{}-{index}", answers.len()), &command);
            answers.push(read_as_the_tui_does(frames.last().unwrap()));
        }
    };
    send(before_reload(), &mut answers);
    let late = late_template(&bed.agent_dir());
    send(after_reload(), &mut answers);
    std::fs::remove_file(late).unwrap();
    (answers, saved)
}

/// The runtime daemon's responses to `commands` on session `id`, through
/// the compat layer.
async fn runtime_answers(client: &mut Client, id: &str, commands: Vec<Value>) -> Vec<Value> {
    let mut answers = Vec::new();
    for (index, mut command) in commands.into_iter().enumerate() {
        command["activeSessionId"] = json!(id);
        let kind = command["type"].as_str().unwrap().to_string();
        let command: DaemonCommand = serde_json::from_value(command).unwrap();
        let mut frames =
            pa_tui::wire2_request_frames(format!("i{index}"), command).expect("a mapped command");
        let outcome = match (frames.pop(), frames.pop()) {
            (Some(ClientFrame::Call { method, args, .. }), None) => {
                client.call(&method, args).await
            }
            (Some(ClientFrame::Query { name, args, .. }), None) => client.query(&name, args).await,
            other => panic!("{kind} is one query or call, not {other:?}"),
        };
        answers.push(read_as_the_tui_does(
            &pa_tui::wire2_response_frame(&kind, outcome).unwrap(),
        ));
    }
    answers
}

/// Session `id`'s Anthropic warning marker rows, without their ids and
/// times, as `transcript.page` serves them.
async fn marker_rows(client: &mut Client, id: &str) -> Vec<Value> {
    let page = ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await);
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["type"] == "custom")
        .map(|row| {
            let mut row = row.clone();
            for key in ["id", "parentId", "timestamp"] {
                row.as_object_mut().unwrap().remove(key);
            }
            row
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_info_reads_answer_as_the_old_daemons_do() {
    let bed = Bed::new();
    models(&bed.agent_dir());
    resources(&bed.agent_dir(), bed.dir.path());
    let (old, old_id) = old_answers(&bed);

    let _daemon = bed.runtime_daemon(&json!({ "responses": ["hi"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    let mut answers = runtime_answers(&mut client, &id, before_reload()).await;
    let late = late_template(&bed.agent_dir());
    answers.extend(runtime_answers(&mut client, &id, after_reload()).await);
    std::fs::remove_file(late).unwrap();
    // The reload read the late template on both.
    let late_command = |answer: &Value| {
        answer["data"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "late")
    };
    assert_eq!(
        (late_command(&answers[0]), late_command(&answers[8])),
        (false, true)
    );
    assert_eq!(answers, old);
    // A transcript the session has not written a turn to is kept in memory:
    // one turn stores it.
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    super::run_turn(&mut client, &id, "hello").await;
    let marker = json!({ "type": "custom",
        "customType": pa_core::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE,
        "data": { "shown": true } });
    assert_eq!(
        (
            marker_rows(&mut client, &id).await,
            marker_rows(&mut client, &old_id).await
        ),
        (vec![marker.clone()], vec![marker])
    );
}
