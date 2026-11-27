//! A session's whole-session replacements on the runtime daemon (phase
//! 8d-2b): `fork {entryId, position}` (before a user message, at an entry,
//! onto a missing entry, before an entry that is no user message) and
//! `import_jsonl` (a missing file, a file whose working directory is gone,
//! the same file with the working directory overridden), sent through
//! pa-tui's `wire2_compat`, answer as the old daemon (the `pa-daemon`
//! supervisor) answers them on the same transcript, with the old
//! `errorInfo`; each goes on under the same id from the same transcript (a
//! fork copies the path, leaving the transcript it forked from as it was),
//! and on the runtime daemon the session's subscriber hears its next turn,
//! which the imported copy keeps.

use std::path::{Path, PathBuf};

use pa_types::wire2::{ServerFrame, Since};
use serde_json::{json, Value};

use super::info::runtime_answers;
use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::tree::{seed, SEEDED};
use super::{ok, run_turn, Bed, Client};

/// Both daemons run offline, on the same built-in model.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// A transcript file to import, named for the daemon that imports it, kept
/// outside the sessions dir: a question and its answer, in a working
/// directory that does not exist.
fn importable(bed: &Bed, daemon: &str) -> PathBuf {
    let at = "2026-11-02T00:00:00.000Z";
    let suffix = if daemon == "old" { "e" } else { "f" };
    let rows = [
        json!({ "type": "session", "version": 3, "id": format!("0199a000-0000-7000-8000-00000000000{suffix}"),
                "timestamp": at, "cwd": bed.dir.path().join("gone") }),
        json!({ "type": "message", "id": "i1", "parentId": null, "timestamp": at,
                "message": { "role": "user", "content": "imported question", "timestamp": 1 } }),
        json!({ "type": "message", "id": "i2", "parentId": "i1", "timestamp": at,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "imported answer" }],
                    "api": "openai-completions", "provider": "battery", "model": "mock-1",
                    "stopReason": "stop", "timestamp": 2,
                    "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                               "totalTokens": 2, "cost": { "input": 0, "output": 0,
                               "cacheRead": 0, "cacheWrite": 0, "total": 0 } } } }),
    ];
    let dir = bed.dir.path().join("imports");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{daemon}-import.jsonl"));
    let mut text = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    path
}

/// The commands, in order, without their session.
fn commands(bed: &Bed, import: &Path) -> Vec<Value> {
    vec![
        json!({ "type": "fork", "entryId": "u2" }),
        json!({ "type": "get_last_assistant_text" }),
        json!({ "type": "fork", "entryId": "a1", "position": "at" }),
        json!({ "type": "fork", "entryId": "missing", "position": "at" }),
        json!({ "type": "fork", "entryId": "a1" }),
        json!({ "type": "get_user_messages_for_forking" }),
        json!({ "type": "import_jsonl", "inputPath": bed.dir.path().join("imports/none.jsonl") }),
        json!({ "type": "import_jsonl", "inputPath": import }),
        json!({ "type": "import_jsonl", "inputPath": import, "cwdOverride": bed.dir.path() }),
        json!({ "type": "get_last_assistant_text" }),
        json!({ "type": "get_user_messages_for_forking" }),
    ]
}

/// `answers` with the daemon's own import file named as the other's is.
fn named_alike(answers: &[Value], daemon: &str) -> Value {
    serde_json::from_str(
        &Value::from(answers.to_vec())
            .to_string()
            .replace(&format!("{daemon}-import"), "import"),
    )
    .unwrap()
}

/// The message ids of the transcript at `path`, in file order.
fn message_ids(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == "message")
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect()
}

/// The old daemon's answers to [`commands`] on a session it opens on the
/// transcript at `path`, its import file named as the runtime daemon's is.
fn old_answers(bed: &Bed, sessions: &Path, path: &Path) -> Value {
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "sessionPath": path, "config": {
            "cwd": bed.dir.path(), "sessionDir": sessions,
            "provider": "battery", "model": "mock-1",
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let answers: Vec<Value> = commands(bed, &importable(bed, "old"))
        .into_iter()
        .enumerate()
        .map(|(index, mut command)| {
            command["activeSessionId"] = json!(active);
            let frames = old_list(&socket, &format!("f{index}"), &command);
            read_as_the_tui_does(frames.last().unwrap())
        })
        .collect();
    named_alike(&answers, "old")
}

/// What the old daemon answers to [`commands`], as the TUI reads it.
fn expected(bed: &Bed, sessions: &Path) -> Value {
    let gone = bed.dir.path().join("gone");
    let copy = sessions.join("import.jsonl");
    json!([
        { "type": "response", "command": "fork", "success": true,
          "data": { "cancelled": false, "selectedText": "second question" } },
        { "type": "response", "command": "get_last_assistant_text", "success": true,
          "data": { "text": "first answer" } },
        { "type": "response", "command": "fork", "success": true,
          "data": { "cancelled": false } },
        { "type": "response", "command": "fork", "success": false,
          "error": "Invalid entry ID for forking" },
        { "type": "response", "command": "fork", "success": false,
          "error": "Invalid entry ID for forking" },
        { "type": "response", "command": "get_user_messages_for_forking", "success": true,
          "data": { "messages": [{ "entryId": "u1", "text": "first question" }] } },
        { "type": "response", "command": "import_jsonl", "success": false,
          "error": format!("File not found: {}", bed.dir.path().join("imports/none.jsonl").display()),
          "errorInfo": { "code": "session_import_file_not_found",
                         "filePath": bed.dir.path().join("imports/none.jsonl") } },
        { "type": "response", "command": "import_jsonl", "success": false,
          "error": format!(
              "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
              gone.display(), copy.display(), bed.dir.path().display()),
          "errorInfo": { "code": "missing_session_cwd", "issue": {
              "sessionFile": copy, "sessionCwd": gone, "fallbackCwd": bed.dir.path() } } },
        { "type": "response", "command": "import_jsonl", "success": true,
          "data": { "cancelled": false } },
        { "type": "response", "command": "get_last_assistant_text", "success": true,
          "data": { "text": "imported answer" } },
        { "type": "response", "command": "get_user_messages_for_forking", "success": true,
          "data": { "messages": [{ "entryId": "i1", "text": "imported question" }] } },
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn the_replacements_answer_and_fork_as_the_old_daemons_do() {
    let bed = Bed::new();
    let sessions = bed.agent_dir().join("sessions");
    let old_path = seed(
        &bed.agent_dir(),
        bed.dir.path(),
        "0199a000-0000-7000-8000-00000000000c",
    );
    let runtime_path = seed(
        &bed.agent_dir(),
        bed.dir.path(),
        "0199a000-0000-7000-8000-00000000000d",
    );

    let old = old_answers(&bed, &sessions, &old_path);

    let _daemon = bed.runtime_daemon_with(&json!({ "responses": ["after the import"] }), OFFLINE);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client
        .call("session.create", json!({ "path": runtime_path }))
        .await);
    let id = created["session"].as_str().unwrap().to_string();
    // The session's one subscriber, from before its first replacement.
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    let import = importable(&bed, "runtime");
    let answers = runtime_answers(&mut client, &id, commands(&bed, &import)).await;

    // What the old daemon answered, as the TUI reads it.
    assert_eq!(old, expected(&bed, &sessions));
    assert_eq!(named_alike(&answers, "runtime"), old);
    // A fork copies: the transcripts forked from are as they were.
    assert_eq!(
        (message_ids(&runtime_path), message_ids(&old_path)),
        (
            SEEDED.map(str::to_string).to_vec(),
            SEEDED.map(str::to_string).to_vec()
        )
    );

    // The session runs on from the import under its id: its subscriber
    // hears the turn, and the copy keeps it.
    let frames = run_turn(&mut client, &id, "and now").await;
    let streamed = frames.iter().any(|frame| {
        matches!(frame, ServerFrame::Event { event, .. }
            if event.event.topic.to_string() == format!("agent/{id}/stream"))
    });
    let page = ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await);
    let texts: Vec<Option<String>> = super::turn_rows(&page)
        .into_iter()
        .map(|(_, _, text)| text)
        .collect();
    let snapshot = ok(client
        .query("session.snapshot", json!({ "session": id }))
        .await);
    assert_eq!(
        (streamed, texts, snapshot["path"].clone()),
        (
            true,
            [
                "imported question",
                "imported answer",
                "and now",
                "after the import"
            ]
            .map(|text| Some(text.to_string()))
            .to_vec(),
            json!(sessions.join("runtime-import.jsonl")),
        )
    );
}
