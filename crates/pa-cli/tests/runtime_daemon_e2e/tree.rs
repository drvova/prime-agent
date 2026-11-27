//! A session's tree moves and the reads beside them on the runtime daemon
//! (phase 8d-2): `get_user_messages_for_forking`, `get_last_assistant_text`,
//! `navigate_tree` (onto a user message, onto a reply with a label, onto the
//! leaf, onto no entry, and with the abandoned branch summarized on the
//! session's model), `abort_branch_summary` and `export_html`, sent through
//! pa-tui's `wire2_compat`, answer as the old daemon (the `pa-daemon`
//! supervisor) answers them on the same transcript, record the same label
//! and summary rows, and export the same entries.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};

use super::info::runtime_answers;
use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::{ok, Bed, Client};

/// The seeded rows' ids: two questions and their answers.
const SEEDED: [&str; 4] = ["u1", "a1", "u2", "a2"];

/// The loopback provider is local: no proxy between it and the daemons.
const LOCAL: &[(&str, &str)] = &[("NO_PROXY", "127.0.0.1"), ("no_proxy", "127.0.0.1")];

/// A loopback `openai-completions` provider that answers every request with
/// the same streamed summary.
fn summary_provider() -> u16 {
    use std::io::{BufRead as _, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = std::io::BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let chunk = |delta: Value, finish: Value| {
                json!({ "id": "c", "object": "chat.completion.chunk", "created": 0,
                        "model": "m-1", "choices": [{ "index": 0, "delta": delta,
                        "finish_reason": finish }] })
            };
            let mut end = chunk(json!({}), json!("stop"));
            end["usage"] = json!({ "prompt_tokens": 40, "completion_tokens": 8,
                                   "total_tokens": 48 });
            let events = format!(
                "data: {}\n\ndata: {end}\n\ndata: [DONE]\n\n",
                chunk(
                    json!({ "role": "assistant", "content": "The branch asked a second question." }),
                    Value::Null
                ),
            );
            let _ = write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{events}",
                events.len()
            );
        }
    });
    port
}

/// The loopback provider's one model.
fn models(agent_dir: &Path, port: u16) {
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": { "loop": {
            "api": "openai-completions",
            "baseUrl": format!("http://127.0.0.1:{port}/v1"),
            "apiKey": "sk-loop",
            "models": [{ "id": "m-1", "name": "m-1", "api": "openai-completions",
                         "reasoning": false, "contextWindow": 128_000, "maxTokens": 4096 }],
        } } })
        .to_string(),
    )
    .unwrap();
}

/// A saved session `id` under the agent dir's sessions: two questions and
/// their answers (the second answer in two text blocks).
fn seed(agent_dir: &Path, cwd: &Path, id: &str) -> PathBuf {
    let at = "2026-11-01T00:00:00.000Z";
    let message = |id: &str, parent: Option<&str>, role: &str, content: Value| {
        let mut message = json!({ "role": role, "content": content, "timestamp": 1_000 });
        if role == "assistant" {
            for (key, value) in [
                ("api", json!("openai-completions")),
                ("provider", json!("loop")),
                ("model", json!("m-1")),
                ("stopReason", json!("stop")),
                (
                    "usage",
                    json!({ "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                                  "totalTokens": 2, "cost": { "input": 0, "output": 0,
                                  "cacheRead": 0, "cacheWrite": 0, "total": 0 } }),
                ),
            ] {
                message[key] = value;
            }
        }
        json!({ "type": "message", "id": id, "parentId": parent, "timestamp": at,
                "message": message })
    };
    let text = |text: &str| json!([{ "type": "text", "text": text }]);
    let rows = [
        json!({ "type": "session", "version": 3, "id": id, "timestamp": at, "cwd": cwd }),
        message("u1", None, "user", text("first question")),
        message("a1", Some("u1"), "assistant", text("first answer")),
        message("u2", Some("a1"), "user", json!("second question")),
        message(
            "a2",
            Some("u2"),
            "assistant",
            json!([{ "type": "text", "text": "second" }, { "type": "text", "text": "answer" }]),
        ),
    ];
    let dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut body = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    std::fs::write(&path, body).unwrap();
    path
}

/// The commands, in order, without their session.
fn commands(export: &Path) -> Vec<Value> {
    let navigate = |target: &str| {
        json!({ "type": "navigate_tree", "targetId": target, "summarize": false,
                "restoreFiles": true })
    };
    let mut labelled = navigate("a2");
    labelled["label"] = json!("back");
    let mut summarized = navigate("a1");
    summarized["summarize"] = json!(true);
    summarized["label"] = json!("summarized");
    vec![
        json!({ "type": "get_user_messages_for_forking" }),
        json!({ "type": "get_last_assistant_text" }),
        navigate("u2"),
        json!({ "type": "get_last_assistant_text" }),
        labelled,
        navigate("a2"),
        navigate("missing"),
        summarized,
        json!({ "type": "get_last_assistant_text" }),
        json!({ "type": "abort_branch_summary" }),
        json!({ "type": "export_html", "outputPath": export }),
    ]
}

/// A row's id as the comparison shows it: a seeded one as it is, one a
/// daemon made as `new`.
fn known(id: &Value) -> Value {
    match id.as_str() {
        Some(id) if SEEDED.contains(&id) => json!(id),
        Some(_) => json!("new"),
        None => Value::Null,
    }
}

/// `answer` without what each daemon makes its own: the summary entry's id
/// and time.
fn comparable(mut answer: Value) -> Value {
    if let Some(entry) = answer
        .get_mut("data")
        .and_then(|data| data.get_mut("summaryEntry"))
        .and_then(Value::as_object_mut)
    {
        entry.remove("id");
        entry.remove("timestamp");
    }
    answer
}

/// The label and summary rows of the transcript at `path`, their ids and
/// the ids they name as [`known`] shows them, without their times.
fn tree_rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == "label" || row["type"] == "branch_summary")
        .map(|mut row| {
            for key in ["id", "parentId", "targetId", "fromId"] {
                if row.get(key).is_some() {
                    row[key] = known(&row[key]);
                }
            }
            row.as_object_mut().unwrap().remove("timestamp");
            row
        })
        .collect()
}

/// The exported file's messages, labels and summaries, by kind (what each
/// daemon writes when it opens a session differs), and whether it carries a
/// system prompt.
fn exported(path: &Path) -> (Vec<Value>, bool) {
    let html = std::fs::read_to_string(path).unwrap();
    let start = html
        .find(r#"<script id="session-data" type="application/json">"#)
        .unwrap();
    let blob = &html[start..];
    let blob = &blob[blob.find('>').unwrap() + 1..blob.find("</script>").unwrap()];
    let data: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(blob)
            .unwrap(),
    )
    .unwrap();
    let kinds = data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["type"].clone())
        .filter(|kind| ["message", "label", "branch_summary"].contains(&kind.as_str().unwrap()))
        .collect();
    (
        kinds,
        data["systemPrompt"]
            .as_str()
            .is_some_and(|prompt| !prompt.is_empty()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tree_moves_answer_and_record_as_the_old_daemons_do() {
    let bed = Bed::new();
    models(&bed.agent_dir(), summary_provider());
    let old_path = seed(
        &bed.agent_dir(),
        bed.dir.path(),
        "0199a000-0000-7000-8000-00000000000a",
    );
    let runtime_path = seed(
        &bed.agent_dir(),
        bed.dir.path(),
        "0199a000-0000-7000-8000-00000000000b",
    );
    let export = bed.dir.path().join("export.html");

    let (old, old_export) = {
        let _daemon = bed.old_daemon_with(LOCAL);
        let socket = bed.old_socket();
        let created = old_list(
            &socket,
            "create",
            &json!({ "type": "create", "sessionPath": old_path, "config": {
                "cwd": bed.dir.path(), "sessionDir": bed.agent_dir().join("sessions"),
                "provider": "loop", "model": "m-1",
            } }),
        );
        let created = created.last().unwrap();
        assert_eq!(created["success"], true, "{created}");
        let active = created["data"]["id"].as_str().unwrap().to_string();
        let answers: Vec<Value> = commands(&export)
            .into_iter()
            .enumerate()
            .map(|(index, mut command)| {
                command["activeSessionId"] = json!(active);
                let frames = old_list(&socket, &format!("t{index}"), &command);
                comparable(read_as_the_tui_does(frames.last().unwrap()))
            })
            .collect();
        (answers, exported(&export))
    };

    let _daemon = bed.runtime_daemon_with(&json!({ "responses": ["hi"] }), LOCAL);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client
        .call("session.create", json!({ "path": runtime_path }))
        .await);
    let id = created["session"].as_str().unwrap().to_string();
    ok(client
        .call(
            "session.set_model",
            json!({ "session": id, "provider": "loop", "modelId": "m-1" }),
        )
        .await);
    let answers: Vec<Value> = runtime_answers(&mut client, &id, commands(&export))
        .await
        .into_iter()
        .map(comparable)
        .collect();

    // What the old daemon answered, as the TUI reads it.
    assert_eq!(
        (
            &old[..3],
            &old[6]["error"],
            old[7]["data"]["summaryEntry"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.ends_with("The branch asked a second question."))
        ),
        (
            &[
                json!({ "type": "response", "command": "get_user_messages_for_forking",
                        "success": true, "data": { "messages": [
                            { "entryId": "u1", "text": "first question" },
                            { "entryId": "u2", "text": "second question" },
                        ] } }),
                json!({ "type": "response", "command": "get_last_assistant_text",
                        "success": true, "data": { "text": "second answer" } }),
                json!({ "type": "response", "command": "navigate_tree", "success": true,
                        "data": { "cancelled": false, "editorText": "second question" } }),
            ][..],
            &json!("Entry missing not found"),
            true,
        )
    );
    assert_eq!(answers, old);
    assert_eq!(tree_rows(&runtime_path), tree_rows(&old_path));
    assert_eq!(exported(&export), old_export);
}
