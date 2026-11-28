//! A session's kernel questions, todo list and notifications on the runtime
//! daemon (phase 8d-2e), against the old daemon (the `pa-daemon` supervisor)
//! on the same faux turns:
//!
//! - a turn's `ask` while no client that can answer is attached settles as
//!   `no_user` at once;
//! - with a client attached that declared `user_questions` (pa-tui's attach,
//!   through `wire2_compat` on the runtime daemon), a turn's `todo-set`,
//!   `notify` and `ask` reach it as the old `todo_update`,
//!   `kernel_notification`, `question_open` and `question_closed` session
//!   events, the question waits, a client attaching meanwhile reads it as
//!   `state.pendingQuestion`, and `answer_question` settles it with the old
//!   reply; answering it again is the old refusal.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;
use pa_types::wire2::ClientFrame;
use serde_json::{json, Value};

use super::info::runtime_answers;
use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::{Bed, Client};

/// Both daemons run offline.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// The capabilities pa-tui's attach declares that matter here.
const TUI_CAPABILITIES: [&str; 3] = ["attach_snapshot", "event_sequence", "user_questions"];

/// The session events the interaction surface sends.
const INTERACTION_EVENTS: [&str; 4] = [
    "todo_update",
    "kernel_notification",
    "question_open",
    "question_closed",
];

/// Two turns: one that asks with nobody to answer, one that sets the todo
/// list, notifies and asks.
fn responses() -> Value {
    json!([
        cell(
            "Asking first.",
            "(hash-ref (ask \"Ready?\" (list \"yes\" \"no\") #:default \"no\") 'status)"
        ),
        "asked first",
        cell(
            "Asking second.",
            "(begin (todo-set (list \"ship\")) (notify \"ready\") \
             (hash-ref (ask \"Proceed?\" (list \"yes\" \"no\")) 'answer))"
        ),
        "asked second",
    ])
}

/// What both daemons are compared on.
#[derive(Debug, PartialEq)]
struct Seen {
    /// Whether the first turn's cell came back `no_user`.
    first_no_user: bool,
    /// The interaction events of the second turn, in order, without their
    /// `meta`.
    events: Vec<Value>,
    /// `state.pendingQuestion` of an attach while the question waits.
    pending: Value,
    /// The answer's response, and the response to answering it again.
    answered: Value,
    stale: Value,
}

/// `value` with the question's id `id` written as `<question>`.
fn masked(value: &Value, id: &str) -> Value {
    serde_json::from_str(&value.to_string().replace(id, "<question>")).unwrap()
}

/// Whether a tool result of the `get_messages` answer `messages` (its data)
/// reads `no_user`.
fn answered_no_user(messages: &Value) -> bool {
    messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "toolResult")
        .any(|message| message["content"].to_string().contains("no_user"))
}

/// One persistent connection to the old daemon.
struct OldClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl OldClient {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to the old daemon");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        OldClient {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }

    fn send(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        self.writer
            .write_all(format!("{envelope}\n").as_bytes())
            .unwrap();
    }

    /// The next frame the daemon sends.
    fn frame(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_mins(1);
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("the old daemon closed the connection"),
                Ok(_) => {
                    if let Ok(frame) = serde_json::from_str::<Value>(line.trim()) {
                        return frame;
                    }
                }
                Err(_) => assert!(Instant::now() < deadline, "the old daemon sent nothing"),
            }
        }
    }

    /// Reads frames until the response to request `id`, keeping each
    /// session event's `event` in `events`; the response.
    fn response(&mut self, id: &str, events: &mut Vec<Value>) -> Value {
        loop {
            let frame = self.frame();
            if frame["type"] == "response" && frame["id"] == id {
                return frame;
            }
            if frame["type"] == "session_event" {
                events.push(frame["event"].clone());
            }
        }
    }

    /// Reads frames until a session event of `kind`, keeping each in
    /// `events`.
    fn until_event(&mut self, kind: &str, events: &mut Vec<Value>) {
        while !events.iter().any(|event| event["type"] == kind) {
            let frame = self.frame();
            if frame["type"] == "session_event" {
                events.push(frame["event"].clone());
            }
        }
    }
}

/// The interaction events among `events`, without their `meta`.
fn interaction_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| INTERACTION_EVENTS.contains(&event["type"].as_str().unwrap_or_default()))
        .map(|event| {
            let mut event = event.clone();
            event.as_object_mut().unwrap().remove("meta");
            event
        })
        .collect()
}

/// The open question's id among `events`.
fn question_id(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["type"] == "question_open")
        .and_then(|event| event["question"]["id"].as_str())
        .expect("a question opened")
        .to_string()
}

/// The answer to the question `id`.
fn answer(id: &str) -> Value {
    json!({ "type": "answer_question", "questionId": id,
            "reply": { "status": "answered", "answer": ["yes"] } })
}

fn attach(client_id: &str, capabilities: &[&str]) -> Value {
    json!({ "type": "attach", "clientId": client_id, "capabilities": capabilities })
}

/// The old daemon's side of the flow.
fn old_flow(bed: &Bed) -> Seen {
    let workspace = bed.dir.path().join("old-project");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000, "responses": responses() })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "config": {
            "cwd": workspace, "sessionDir": bed.agent_dir().join("sessions"), "script": script,
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let with_session = |mut command: Value| {
        command["activeSessionId"] = json!(active);
        command
    };
    let first = old_list(
        &socket,
        "first",
        &with_session(json!({ "type": "prompt_and_wait", "message": "first" })),
    )
    .pop()
    .unwrap();
    assert_eq!(first["success"], true, "{first}");
    let messages = old_list(
        &socket,
        "messages",
        &with_session(json!({ "type": "get_messages" })),
    )
    .pop()
    .unwrap();
    let first_no_user = answered_no_user(&messages["data"]);

    let mut tui = OldClient::connect(&socket);
    let mut events = Vec::new();
    tui.send("attach", &with_session(attach("tui:1", &TUI_CAPABILITIES)));
    let attached = tui.response("attach", &mut events);
    assert_eq!(attached["success"], true, "{attached}");
    tui.send(
        "second",
        &with_session(json!({ "type": "prompt", "message": "second" })),
    );
    tui.until_event("question_open", &mut events);
    let question = question_id(&events);
    let late = old_list(
        &socket,
        "late",
        &with_session(attach("late:1", &["attach_snapshot"])),
    )
    .pop()
    .unwrap();
    let pending = late["data"]["snapshot"]["state"]["pendingQuestion"].clone();
    tui.send("answer", &with_session(answer(&question)));
    let answered = read_as_the_tui_does(&tui.response("answer", &mut events));
    tui.until_event("agent_end", &mut events);
    tui.send("stale", &with_session(answer(&question)));
    let stale = read_as_the_tui_does(&tui.response("stale", &mut events));
    Seen {
        first_no_user,
        events: masked(&json!(interaction_events(&events)), &question)
            .as_array()
            .unwrap()
            .clone(),
        pending: masked(&pending, &question),
        answered,
        stale: masked(&stale, &question),
    }
}

/// `command` on session `id` through the compat layer, as one call sent on
/// `session`'s connection; the response the TUI reads.
async fn compat_call(session: &mut Attached, mut command: Value) -> Value {
    command["activeSessionId"] = json!(session.id);
    let kind = command["type"].as_str().unwrap().to_string();
    let command: DaemonCommand = serde_json::from_value(command).unwrap();
    let request = session.request_id();
    let frames = pa_tui::wire2_request_frames(request, command).expect("a mapped command");
    let [frame @ ClientFrame::Call { .. }] = frames.as_slice() else {
        panic!("{kind} is one call, not {frames:?}");
    };
    // The frames the call publishes before its reply stay with the session.
    let outcome = session.request(frame).await;
    read_as_the_tui_does(&pa_tui::wire2_response_frame(&kind, outcome).unwrap())
}

/// The session events of `session`'s stream, as the TUI reads them.
fn stream_events(session: &Attached) -> Vec<Value> {
    let events = session.events("stream");
    pa_tui::wire2_live_frames(&events)
        .expect("mapped frames")
        .into_iter()
        .filter(|frame| frame["type"] == "session_event")
        .map(|frame| frame["event"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn questions_wait_for_an_answering_client_as_on_the_old_daemon() {
    let bed = Bed::new();
    let old = old_flow(&bed);

    let workspace = bed.dir.path().join("runtime-project");
    std::fs::create_dir_all(&workspace).unwrap();
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": responses() }), OFFLINE);
    let mut session = Attached::open(&bed, &workspace).await;
    let accepted = session.publish("first", None).await;
    session.done(&accepted["task"]).await;
    let id = session.id.clone();
    let (mut reader, _) = Client::connect(&bed.runtime_socket()).await;
    let messages = runtime_answers(&mut reader, &id, vec![json!({ "type": "get_messages" })]).await;
    let first_no_user = answered_no_user(&messages[0]["data"]);

    let attached = compat_call(&mut session, attach("tui:1", &TUI_CAPABILITIES)).await;
    assert_eq!(attached["success"], true, "{attached}");
    let accepted = session.publish("second", None).await;
    session.until_streamed("question_open").await;
    let question = question_id(&stream_events(&session));
    let late = runtime_answers(
        &mut reader,
        &id,
        vec![attach("late:1", &["attach_snapshot"])],
    )
    .await;
    let pending = late[0]["data"]["snapshot"]["state"]["pendingQuestion"].clone();
    let answered = compat_call(&mut session, answer(&question)).await;
    session.done(&accepted["task"]).await;
    let stale = compat_call(&mut session, answer(&question)).await;
    let runtime = Seen {
        first_no_user,
        events: masked(
            &json!(interaction_events(&stream_events(&session))),
            &question,
        )
        .as_array()
        .unwrap()
        .clone(),
        pending: masked(&pending, &question),
        answered,
        stale: masked(&stale, &question),
    };
    let question_open = json!({ "type": "question_open", "question": {
        "id": "<question>", "text": "Proceed?", "kind": "single",
        "choices": ["yes", "no"], "allowOther": true } });
    let expected = Seen {
        first_no_user: true,
        events: vec![
            json!({ "type": "todo_update", "todos": { "items": [
                { "id": "1", "text": "ship", "status": "pending" } ] } }),
            json!({ "type": "kernel_notification",
                    "notification": { "message": "ready", "level": "info" } }),
            question_open.clone(),
            json!({ "type": "question_closed", "questionId": "<question>",
                    "status": "answered" }),
        ],
        pending: question_open["question"].clone(),
        answered: json!({ "type": "response", "command": "answer_question", "success": true }),
        stale: json!({ "type": "response", "command": "answer_question", "success": false,
                       "error": "question \"<question>\" is not open" }),
    };
    assert_eq!(old, expected);
    assert_eq!(runtime, expected);
}
