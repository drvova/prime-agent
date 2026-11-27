//! The agents view's session controls on the runtime daemon (phase 8d-4b),
//! sent through pa-tui's `wire2_compat` and answered as the old daemon (the
//! `pa-daemon` supervisor) answers them, on the same faux turns:
//!
//! - Agent-message ingest (`protocol_breadth_b6_b9`'s
//!   `wave_b7_agent_messages_wire_shapes`): the selector-less status with no
//!   session, the session's status, its pause, a message refused while
//!   paused, its resume and the selector-less status; then, while a turn
//!   holds in a kernel cell, a message queued behind it and the clear that
//!   takes it out (its prompt under `steering`), and once the session is idle
//!   again, a clear with nothing to take.
//! - The RLM depth bound and the subagent delete (`wave_b6`): the status, a
//!   chat override, its status, a global one (written to the settings file),
//!   a refused `maxDepth`; a delete of a held child (`running`), of the same
//!   child once settled, of it again, and of an unknown one.
//!
//! Each daemon runs on a bed of its own.

use std::path::Path;

use pa_types::daemon::DaemonCommand;
use pa_types::wire2::{ClientFrame, Outcome};
use serde_json::{json, Value};

use super::kernel_activity::runtime_answer;
use super::saved_list::old_list;
use super::side_questions::OldClient;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::Bed;

/// Both daemons run offline.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// A form that holds its cell until `marker` exists: short checks in a loop,
/// so no shell command outlives the kernel's 10 s yield (one that did would
/// end the turn with the command still running).
pub(super) fn hold_until(marker: &Path) -> String {
    let check = format!("test -f '{}'", marker.display());
    format!(
        "(define (hold) (if (= 0 (hash-ref (bash {check:?}) 'exit-code)) 'released \
         (begin (bash \"sleep 0.1\") (hold)))) (hold)"
    )
}

/// `prompt` with its sender (the client, whose name the daemons give it
/// differently) by its form.
fn sender_masked(prompt: &Value) -> Value {
    let text = prompt.as_str().unwrap_or_default();
    match text.split_once("]\n\n") {
        Some((_, body)) => json!(format!("[agent-message from <sender>]\n\n{body}")),
        None => prompt.clone(),
    }
}

/// `response` as the TUI reads it, the cleared prompts' senders masked.
fn normalized(response: &Value) -> Value {
    let mut response = read_as_the_tui_does(response);
    for lane in ["steering", "followUp"] {
        if let Some(Value::Array(prompts)) = response.pointer_mut(&format!("/data/{lane}")) {
            for prompt in prompts.iter_mut() {
                *prompt = sender_masked(prompt);
            }
        }
    }
    response
}

/// An old daemon's faux script file at `path`.
fn write_script(path: &Path, responses: &Value) {
    std::fs::write(
        path,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000, "responses": responses,
                "repeatLastResponse": true })
        .to_string(),
    )
    .unwrap();
}

/// Creates a session on the old daemon at `socket`; its id.
fn old_session(bed: &Bed, socket: &Path, config: Value) -> String {
    let mut config = config;
    config["cwd"] = json!(bed.dir.path());
    config["sessionDir"] = json!(bed.agent_dir().join("sessions"));
    let created = old_list(
        socket,
        "create",
        &json!({ "type": "create", "config": config }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    created["data"]["id"].as_str().unwrap().to_string()
}

/// The ingest scenario's turn: it holds in a kernel cell until `release`.
fn ingest_responses(release: &Path) -> Value {
    json!([cell("Holding.", &hold_until(release)), "held"])
}

/// The ingest scenario's commands before the held turn, without their
/// session (`None`: the command names none).
fn ingest_steps() -> Vec<(Value, bool)> {
    vec![
        (json!({ "type": "agent_messages_status" }), true),
        (json!({ "type": "agent_messages_pause" }), true),
        (
            json!({ "type": "send_message", "message": "while paused" }),
            true,
        ),
        (json!({ "type": "agent_messages_resume" }), true),
        (json!({ "type": "agent_messages_status" }), false),
    ]
}

/// The old daemon's ingest answers.
fn old_ingest(bed: &Bed) -> Vec<Value> {
    let release = bed.dir.path().join("release");
    let script = bed.dir.path().join("old-script.json");
    write_script(&script, &ingest_responses(&release));
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let mut answers = vec![normalized(
        old_list(
            &socket,
            "unscoped",
            &json!({ "type": "agent_messages_status" }),
        )
        .last()
        .unwrap(),
    )];
    let active = old_session(bed, &socket, json!({ "script": script }));
    let mut client = OldClient::connect(&socket);
    let scoped = |mut command: Value| {
        if command["type"] == "send_message" {
            command["targetActiveSessionId"] = json!(active);
        } else {
            command["activeSessionId"] = json!(active);
        }
        command
    };
    for (index, (command, with_session)) in ingest_steps().into_iter().enumerate() {
        let command = if with_session {
            scoped(command)
        } else {
            command
        };
        answers.push(normalized(&client.send(&format!("s{index}"), &command)));
    }
    let attached = client.send(
        "attach",
        &scoped(json!({ "type": "attach", "clientId": "tui:1" })),
    );
    assert_eq!(attached["success"], true, "{attached}");
    let prompted = client.send(
        "hold",
        &scoped(json!({ "type": "prompt", "message": "hold" })),
    );
    assert_eq!(prompted["success"], true, "{prompted}");
    client.until(|seen| {
        seen.iter().any(|frame| {
            frame["type"] == "session_event" && frame["event"]["type"] == "tool_execution_start"
        })
    });
    let sent = client.send(
        "queued",
        &scoped(json!({ "type": "send_message", "message": "behind the turn" })),
    );
    assert_eq!(
        (&sent["success"], &sent["data"]["deliveryStatus"]),
        (&json!(true), &json!("queued")),
        "{sent}"
    );
    answers.push(normalized(
        &client.send("clear", &scoped(json!({ "type": "agent_messages_clear" }))),
    ));
    std::fs::write(&release, "go").unwrap();
    let idle = client.send("idle", &scoped(json!({ "type": "wait_for_idle" })));
    assert_eq!(idle["success"], true, "{idle}");
    answers.push(normalized(&client.send(
        "cleared",
        &scoped(json!({ "type": "agent_messages_clear" })),
    )));
    answers
}

/// What the runtime daemon answers `session.send` with `message` (the
/// command's request), as the TUI's client would read it.
async fn runtime_send(session: &mut Attached, message: &str) -> (Value, Outcome) {
    let id = session.request_id();
    let outcome = session
        .request(&ClientFrame::Call {
            id,
            method: "session.send".to_string(),
            args: json!({ "session": session.id, "message": message }),
        })
        .await;
    let response = pa_tui::wire2_response_frame("send_message", outcome.clone()).unwrap();
    (response, outcome)
}

/// The runtime daemon's ingest answers, through the compat layer.
async fn runtime_ingest(bed: &Bed) -> Vec<Value> {
    let release = bed.dir.path().join("release");
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": ingest_responses(&release), "repeatLastResponse": true }),
        OFFLINE,
    );
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let mut answers = Vec::new();
    for (command, with_session) in ingest_steps() {
        let answer = if command["type"] == "send_message" {
            runtime_send(&mut session, "while paused").await.0
        } else if with_session {
            runtime_answer(&mut session, &command).await
        } else {
            unscoped_answer(&mut session, &command).await
        };
        answers.push(read_as_the_tui_does(&answer));
    }
    session.publish("hold", None).await;
    session.until_streamed("tool_execution_start").await;
    let (_, sent) = runtime_send(&mut session, "behind the turn").await;
    let Outcome::Ok { result: sent } = sent else {
        panic!("the queued send failed: {sent:?}");
    };
    assert_eq!(sent["deliveryStatus"], "queued", "{sent}");
    let clear = json!({ "type": "agent_messages_clear" });
    answers.push(normalized(&runtime_answer(&mut session, &clear).await));
    std::fs::write(&release, "go").unwrap();
    session.call("session.wait_idle", json!({})).await;
    answers.push(normalized(&runtime_answer(&mut session, &clear).await));
    answers
}

/// What the compat layer hands the TUI for `command` as it is, naming no
/// session, on the runtime daemon.
async fn unscoped_answer(session: &mut Attached, command: &Value) -> Value {
    let kind = command["type"].as_str().unwrap().to_string();
    let command: DaemonCommand = serde_json::from_value(command.clone()).unwrap();
    let request = session.request_id();
    let mut frames = pa_tui::wire2_request_frames(request, command).expect("a mapped command");
    let (Some(frame), None) = (frames.pop(), frames.pop()) else {
        panic!("{kind} is one request");
    };
    let outcome = session.request(&frame).await;
    pa_tui::wire2_response_frame(&kind, outcome).unwrap()
}

/// The selector-less status of a runtime daemon running no session (the
/// ingest scenario's daemon runs one from its start).
async fn runtime_unscoped_status(bed: &Bed) -> Value {
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": ["unused"] }), OFFLINE);
    let (mut client, _) = super::Client::connect(&bed.runtime_socket()).await;
    let command: DaemonCommand =
        serde_json::from_value(json!({ "type": "agent_messages_status" })).unwrap();
    let mut frames = pa_tui::wire2_request_frames("unscoped".to_string(), command).unwrap();
    let Some(ClientFrame::Query { name, args, .. }) = frames.pop() else {
        panic!("the status is a query");
    };
    let outcome = client.query(&name, args).await;
    read_as_the_tui_does(&pa_tui::wire2_response_frame("agent_messages_status", outcome).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_message_ingest_answers_as_the_old_daemons_does() {
    let old = old_ingest(&Bed::new());
    let mut runtime = vec![runtime_unscoped_status(&Bed::new()).await];
    runtime.extend(runtime_ingest(&Bed::new()).await);

    let answered = |command: &str, data: Value| json!({ "type": "response", "command": command, "success": true, "data": data });
    let status = |paused: bool| {
        json!({ "paused": paused, "maxMessageChars": 16384, "maxPendingPerSession": 20,
                "rateLimitCapacity": 3, "rateLimitRefillMs": 1000 })
    };
    let steering = |prompts: Value| json!({ "steering": prompts, "followUp": [] });
    // The old daemon's answers are `wave_b7`'s, and the queued message's
    // prompt is what the clear takes.
    assert_eq!(
        old,
        [
            answered(
                "agent_messages_status",
                json!({ "paused": false, "limits": {} })
            ),
            answered("agent_messages_status", status(false)),
            answered("agent_messages_pause", status(true)),
            json!({ "type": "response", "command": "send_message", "success": false,
                    "error": "Agent messaging is paused" }),
            answered("agent_messages_resume", status(false)),
            answered("agent_messages_status", status(false)),
            answered(
                "agent_messages_clear",
                steering(json!(["[agent-message from <sender>]\n\nbehind the turn"]))
            ),
            answered("agent_messages_clear", steering(json!([]))),
        ],
        "{old:#?}"
    );
    assert_eq!(runtime, old);
}

/// The depth scenario's parent: one turn spawns `kid`, which holds in its
/// cell until `release`; the parent's notice turn follows the kid's settle.
fn depth_responses() -> Value {
    json!([
        cell(
            "Spawning.",
            r#"(rlm-spawn "hold until released" #:name "kid")"#
        ),
        "spawned",
        "notice seen",
    ])
}

fn kid_responses(release: &Path) -> Value {
    json!([cell("Holding.", &hold_until(release)), "kid done"])
}

/// The depth scenario's commands, without their session; `CHILD` stands for
/// the kid's handle.
fn depth_steps() -> Vec<Value> {
    vec![
        json!({ "type": "get_rlm_max_depth_status" }),
        json!({ "type": "set_rlm_max_depth", "maxDepth": 3 }),
        json!({ "type": "get_rlm_max_depth_status" }),
        json!({ "type": "set_rlm_max_depth", "maxDepth": 4, "global": true }),
        json!({ "type": "get_rlm_max_depth_status" }),
        json!({ "type": "delete_rlm_subagent", "childId": "CHILD" }),
    ]
}

/// The kid's handle in a children listing's `rows`.
fn kid(rows: &Value) -> String {
    rows[0]["id"]
        .as_str()
        .expect("the kid is listed")
        .to_string()
}

/// The answers to [`depth_steps`] then the deletes after the kid settled,
/// and the settings file's `rlmMaxDepth`.
struct Depth {
    answers: Vec<Value>,
    global: Value,
}

fn after_settle() -> Vec<Value> {
    vec![
        json!({ "type": "delete_rlm_subagent", "childId": "CHILD" }),
        json!({ "type": "delete_rlm_subagent", "childId": "CHILD" }),
        json!({ "type": "delete_rlm_subagent", "childId": "ghost" }),
    ]
}

fn with_child(command: &Value, child: &str) -> Value {
    serde_json::from_str(&command.to_string().replace("CHILD", child)).unwrap()
}

fn global_depth(bed: &Bed) -> Value {
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(bed.agent_dir().join("settings.json")).unwrap(),
    )
    .unwrap();
    settings["rlmMaxDepth"].clone()
}

fn old_depth(bed: &Bed) -> Depth {
    let release = bed.dir.path().join("release");
    let script = bed.dir.path().join("old-script.json");
    let child_script = bed.dir.path().join("old-child.json");
    write_script(&script, &depth_responses());
    write_script(&child_script, &kid_responses(&release));
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let active = old_session(
        bed,
        &socket,
        json!({ "script": script, "childScript": child_script }),
    );
    let mut client = OldClient::connect(&socket);
    let scoped = |mut command: Value| {
        command["activeSessionId"] = json!(active);
        command
    };
    let ran = client.send(
        "turn",
        &scoped(json!({ "type": "prompt_and_wait", "message": "spawn" })),
    );
    assert_eq!(ran["success"], true, "{ran}");
    let child = kid(
        &client.send("children", &scoped(json!({ "type": "get_rlm_children" })))["data"]
            ["children"],
    );
    let mut answers = Vec::new();
    for (index, command) in depth_steps().iter().enumerate() {
        let command = scoped(with_child(command, &child));
        answers.push(normalized(&client.send(&format!("d{index}"), &command)));
    }
    std::fs::write(&release, "go").unwrap();
    // The barrier holds until the kid settled and its notice turn ran.
    let idle = client.send(
        "idle",
        &scoped(json!({ "type": "wait_for_idle", "waitForRlmQuiescence": true })),
    );
    assert_eq!(idle["success"], true, "{idle}");
    for (index, command) in after_settle().iter().enumerate() {
        let command = scoped(with_child(command, &child));
        answers.push(normalized(&client.send(&format!("a{index}"), &command)));
    }
    Depth {
        answers,
        global: global_depth(bed),
    }
}

async fn runtime_depth(bed: &Bed) -> Depth {
    let release = bed.dir.path().join("release");
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": depth_responses(), "repeatLastResponse": true,
                 "children": { "kid": { "responses": kid_responses(&release) } } }),
        OFFLINE,
    );
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let accepted = session.publish("spawn", None).await;
    session.done(&accepted["task"]).await;
    let child = kid(&session.query("session.children").await["children"]);
    let mut answers = Vec::new();
    for command in depth_steps() {
        let answer = runtime_answer(&mut session, &with_child(&command, &child)).await;
        answers.push(normalized(&answer));
    }
    std::fs::write(&release, "go").unwrap();
    // The wait answers once the kid settled and its notice turn ran.
    session.call("session.wait_idle", json!({})).await;
    for command in after_settle() {
        let answer = runtime_answer(&mut session, &with_child(&command, &child)).await;
        answers.push(normalized(&answer));
    }
    Depth {
        answers,
        global: global_depth(bed),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_depth_bound_and_the_subagent_delete_answer_as_the_old_daemons_do() {
    let old = old_depth(&Bed::new());
    let runtime = runtime_depth(&Bed::new()).await;

    let answered = |command: &str, data: Value| json!({ "type": "response", "command": command, "success": true, "data": data });
    let status = |max_depth: u64, source: &str| {
        answered(
            "get_rlm_max_depth_status",
            json!({ "maxDepth": max_depth, "source": source }),
        )
    };
    let set = |max_depth: u64, saved: bool| {
        answered(
            "set_rlm_max_depth",
            json!({ "maxDepth": max_depth, "source": "chat", "globalSaved": saved }),
        )
    };
    let deleted = |data: Value| answered("delete_rlm_subagent", data);
    // The old daemon's answers: `wave_b6`'s, and the delete's outcomes.
    assert_eq!(
        (&old.answers[1..], &old.global),
        (
            &[
                set(3, false),
                status(3, "chat"),
                set(4, true),
                status(4, "chat"),
                deleted(json!({ "deleted": false, "reason": "running" })),
                deleted(json!({ "deleted": true })),
                deleted(json!({ "deleted": true })),
                deleted(json!({ "deleted": false })),
            ][..],
            &json!(4)
        ),
        "{:#?}",
        old.answers
    );
    assert_eq!(
        old.answers[0]["data"]["source"], "default",
        "{:#?}",
        old.answers
    );
    assert_eq!((runtime.answers, runtime.global), (old.answers, old.global));
}
