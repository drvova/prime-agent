//! Waiting inside the turn, through a real session in the daemon: a scripted
//! `eval` cell that waits for a program it started (`wait-until`) or for the
//! user's answer (`ask`) gets its result in the same call, past the session's
//! cell yield window, and the turn goes on to its reply with no new prompt.

use super::*;

use serde_json::{json, Value};

/// The session's cell yield window: far shorter than either wait, so a wait
/// the window counted would move its cell to the background.
const WINDOW_MS: u64 = 300;

/// Start a daemon whose sessions run `shell` and the short window, create a
/// session that runs `cell` and then replies `reply`, attach as a client that
/// can answer questions, and prompt it. Returns the daemon, the client, the
/// session id, and the lines read so far.
fn start_turn(
    dir: &std::path::Path,
    shell: &str,
    cell: &str,
    reply: &str,
) -> (Daemon, Client, String, std::collections::VecDeque<Value>) {
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "pythonCellTimeoutMs": WINDOW_MS, "shell": shell }).to_string(),
    )
    .expect("write settings");
    let script = dir.join("script.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "id": "call-1",
                      "arguments": { "code": cell } },
                ] },
                { "text": reply },
            ],
        })
        .to_string(),
    )
    .expect("write script");
    let daemon = spawn_daemon_in(&socket, &agent_dir, dir);
    let (mut client, _hello) = Client::connect(&socket);
    client.send_command(
        "c1",
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();
    client.send_command(
        "a1",
        &json!({
            "type": "attach",
            "activeSessionId": session_id,
            "capabilities": [pa_types::question::QUESTIONS_CAPABILITY],
        }),
    );
    assert_eq!(client.read_response("a1")["success"], true, "attach failed");
    client.send_command(
        "p1",
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": "go" }),
    );
    let (ack, lines) = client.read_response_and_lines("p1");
    assert_eq!(ack["success"], true, "prompt failed: {ack}");
    (daemon, client, session_id, lines)
}

/// Read the turn to its end: the cell's result (its text, whether it went to
/// the background) and the assistant's last reply.
fn finish_turn(
    client: &mut Client,
    lines: &mut std::collections::VecDeque<Value>,
) -> (String, bool, String) {
    let mut cell = None;
    let mut reply = String::new();
    loop {
        let event = client.next_line_of_type(lines, "session_event")["event"].clone();
        match event["type"].as_str() {
            Some("tool_execution_end") if event["toolCallId"] == "call-1" => {
                cell = Some((
                    event["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    event["result"]["details"]["backgroundCell"].is_string(),
                ));
            }
            Some("message_end") if event["message"]["role"] == "assistant" => {
                if let Some(text) = event["message"]["content"]
                    .as_array()
                    .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
                {
                    reply = text["text"].as_str().unwrap_or_default().to_string();
                }
            }
            Some("agent_end") => break,
            _ => {}
        }
    }
    let (text, backgrounded) = cell.expect("the cell's tool call ended");
    (text, backgrounded, reply)
}

/// A program started in the background, by its path, writes a file a second
/// later; `wait-until` waits for the file inside the cell, and the turn goes
/// on to its reply. On the built-in shell and on the system shell.
#[test]
fn a_cell_waits_for_a_program_it_started_and_the_turn_goes_on() {
    for shell in ["builtin", "system"] {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let flag = dir.path().join("ready.flag");
        let flag = flag.to_string_lossy();
        // The job's result is read in the cell, so no `[bash-done]` notice
        // adds a turn of its own.
        let cell = format!(
            "(define job (bash \"/bin/sleep 1 && echo ready > '{flag}'\" (hash 'background #t)))
             (define waited (wait-until \"test -f '{flag}'\" #:timeout 30 #:every 0.1
                                        #:label \"the program's file\"))
             (bash-wait job 10)
             (hash-ref waited 'status)"
        );
        let (_daemon, mut client, _session, mut lines) =
            start_turn(dir.path(), shell, &cell, "the program finished");
        let (text, backgrounded, reply) = finish_turn(&mut client, &mut lines);
        assert_eq!(
            (
                text.trim_end().ends_with("\"met\""),
                backgrounded,
                reply.as_str()
            ),
            (true, false, "the program finished"),
            "shell {shell}: the cell's result was {text}"
        );
    }
}

/// A cell asks the user to take a step; the attached client sees the
/// question, answers it after the window has passed, and the same turn goes
/// on with the answer.
#[test]
fn a_cell_asks_the_user_and_the_turn_goes_on_with_the_answer() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let (_daemon, mut client, session_id, mut lines) = start_turn(
        dir.path(),
        "builtin",
        r#"(hash-ref (ask "Approve the prompt on your screen, then pick one" (list "done" "it failed")) 'answer)"#,
        "carrying on",
    );
    let question = loop {
        let line = client.next_line_of_type(&mut lines, "session_event");
        if line["event"]["type"] == pa_types::question::QUESTION_OPEN_EVENT {
            break line["event"]["question"].clone();
        }
    };
    // The user reads the question for longer than the session's window.
    std::thread::sleep(Duration::from_millis(WINDOW_MS * 3));
    client.send_command(
        "q1",
        &json!({
            "type": "answer_question",
            "activeSessionId": session_id,
            "questionId": question["id"],
            "reply": { "status": "answered", "answer": ["done"] },
        }),
    );
    let (answered, mut more) = client.read_response_and_lines("q1");
    lines.append(&mut more);
    let (text, backgrounded, reply) = finish_turn(&mut client, &mut lines);
    assert_eq!(
        (
            question["text"].as_str(),
            question["choices"].clone(),
            answered["success"].as_bool(),
            text.trim_end().ends_with("\"done\""),
            backgrounded,
            reply.as_str(),
        ),
        (
            Some("Approve the prompt on your screen, then pick one"),
            json!(["done", "it failed"]),
            Some(true),
            true,
            false,
            "carrying on"
        ),
        "the cell's result was {text}"
    );
}
