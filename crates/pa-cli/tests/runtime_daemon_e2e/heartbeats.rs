//! A session's heartbeat on the runtime daemon (phase 8d-4c), sent through
//! pa-tui's `wire2_compat` and answered as the old daemon (the `pa-daemon`
//! supervisor) answers it, on the same faux turns:
//!
//! - `heartbeat_get` with none, `heartbeat_set`, its read, a set refused
//!   for its delivery mode, a replacement that keeps the delivery mode,
//!   `heartbeat_update` pause, `heartbeat_manage` resume, an unknown job's
//!   refusal, `heartbeat_update` clear, the read after it and an update with
//!   no heartbeat left;
//! - a steered heartbeat's fire that lands while a turn holds in a kernel
//!   cell waits on the session's queue, and its pause withdraws it: the
//!   queue empties and the turn ends without it.
//!
//! Each daemon runs on a bed of its own; the ids and times the beds differ
//! in compare by their form.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::Path;

use serde_json::{json, Value};

use super::kernel_activity::runtime_answer;
use super::old_reference::OldReference;
use super::saved_list::old_list;
use super::side_questions::OldClient;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::Bed;

/// Both daemons run offline.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// `value` with the bed's own ids, paths and times by their form.
fn masked(mut value: Value) -> Value {
    fn mask(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, member) in object.iter_mut() {
                    let own = matches!(
                        key.as_str(),
                        "id" | "activeSessionId" | "sessionId" | "sessionFile" | "cwd"
                    ) || key.ends_with("At");
                    if own && !member.is_null() {
                        *member = json!("<own>");
                    } else {
                        mask(member);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(mask),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    mask(&mut value);
    value
}

/// The scenario's commands, without their session; `JOB` stands for the
/// heartbeat's job id.
fn steps() -> Vec<Value> {
    vec![
        json!({ "type": "heartbeat_get" }),
        json!({ "type": "heartbeat_set", "schedule": "1h", "prompt": "check in",
                "deliveryMode": "follow_up" }),
        json!({ "type": "heartbeat_get" }),
        json!({ "type": "heartbeat_set", "schedule": "1h", "prompt": "check in",
                "deliveryMode": "later" }),
        json!({ "type": "heartbeat_set", "schedule": "2h", "prompt": "check again" }),
        json!({ "type": "heartbeat_update", "action": "pause" }),
        json!({ "type": "heartbeat_manage", "jobId": "JOB", "action": "resume" }),
        json!({ "type": "heartbeat_manage", "jobId": "nope", "action": "stop" }),
        json!({ "type": "heartbeat_update", "action": "clear" }),
        json!({ "type": "heartbeat_get" }),
        json!({ "type": "heartbeat_update", "action": "pause" }),
    ]
}

/// `command` with the heartbeat's job id for `JOB`.
fn with_job(command: &Value, job: &str) -> Value {
    serde_json::from_str(&command.to_string().replace("JOB", job)).unwrap()
}

/// The job id a reply's heartbeat carries.
fn job_of(response: &Value) -> Option<String> {
    response["data"]["heartbeat"]["id"]
        .as_str()
        .map(str::to_string)
}

/// The turn the withdrawal holds in: its cell waits until `release` exists,
/// on a background shell command it waits for past the kernel's 10 s yield
/// (a foreground one would end the turn at the yield).
fn responses(release: &Path) -> Value {
    let poll = format!("until [ -f '{}' ]; do sleep 0.05; done", release.display());
    let hold = format!("(bash-wait (hash-ref (bash {poll:?} (hash 'background #t)) 'job) 600)");
    json!([cell("Holding.", &hold), "held"])
}

/// The steered heartbeat whose fire the held turn queues.
fn steered() -> Value {
    json!({ "type": "heartbeat_set", "schedule": "10s", "prompt": "tick",
            "deliveryMode": "steer" })
}

/// Whether an old `session_action_update`'s or the runtime stream's queue
/// view lists a row.
fn queued(actions: &Value) -> Option<bool> {
    Some(actions["queuedCount"].as_u64()? > 0)
}

/// What one daemon answered, and the transcript's heartbeat rows after the
/// withdrawal.
#[derive(serde::Serialize, serde::Deserialize)]
struct Seen {
    answers: Vec<Value>,
    heartbeat_rows: usize,
}

fn heartbeat_rows(rows: &[Value]) -> usize {
    rows.iter()
        .filter(|row| row["customType"] == "heartbeat_prompt")
        .count()
}

fn old_flow(bed: &Bed) -> Seen {
    let release = bed.dir.path().join("release");
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000,
                "responses": responses(&release), "repeatLastResponse": true })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "config": {
            "cwd": bed.dir.path(), "sessionDir": bed.agent_dir().join("sessions"),
            "script": script,
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let scoped = |mut command: Value| {
        command["activeSessionId"] = json!(active);
        command
    };
    let mut client = OldClient::connect(&socket);
    let mut answers = Vec::new();
    let mut job = String::new();
    for (index, command) in steps().iter().enumerate() {
        let response = client.send(&format!("h{index}"), &scoped(with_job(command, &job)));
        if let Some(id) = job_of(&response) {
            job = id;
        }
        answers.push(masked(read_as_the_tui_does(&response)));
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
    let set = client.send("tick", &scoped(steered()));
    assert_eq!(set["success"], true, "{set}");
    // The old worker sends no queue view while its turn runs: its state's
    // `sessionActions` is read until the fire waits there, then until the
    // pause took it out.
    let mut reads = 0;
    let mut queued_now = |client: &mut OldClient| {
        reads += 1;
        let state = client.send(
            &format!("state{reads}"),
            &scoped(json!({ "type": "get_state" })),
        );
        queued(&state["data"]["sessionActions"])
    };
    while queued_now(&mut client) != Some(true) {}
    let paused = client.send(
        "pause",
        &scoped(json!({ "type": "heartbeat_update", "action": "pause" })),
    );
    answers.push(masked(read_as_the_tui_does(&paused)));
    while queued_now(&mut client) != Some(false) {}
    std::fs::write(&release, "go").unwrap();
    let idle = client.send("idle", &scoped(json!({ "type": "wait_for_idle" })));
    assert_eq!(idle["success"], true, "{idle}");
    let messages = client.send("messages", &scoped(json!({ "type": "get_messages" })));
    Seen {
        answers,
        heartbeat_rows: heartbeat_rows(
            messages["data"]["messages"]
                .as_array()
                .unwrap_or(&Vec::new()),
        ),
    }
}

async fn runtime_flow(bed: &Bed) -> Seen {
    let release = bed.dir.path().join("release");
    let _daemon = bed.runtime_daemon_with(
        &json!({ "responses": responses(&release), "repeatLastResponse": true }),
        OFFLINE,
    );
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let mut answers = Vec::new();
    let mut job = String::new();
    for command in steps() {
        let response = runtime_answer(&mut session, &with_job(&command, &job)).await;
        if let Some(id) = job_of(&response) {
            job = id;
        }
        answers.push(masked(read_as_the_tui_does(&response)));
    }
    session.publish("hold", None).await;
    session.until_streamed("tool_execution_start").await;
    let set = runtime_answer(&mut session, &steered()).await;
    assert_eq!(set["success"], true, "{set}");
    let last_queued = |session: &Attached| {
        session
            .payloads("stream")
            .iter()
            .rev()
            .filter(|event| event["type"] == "session_action_update")
            .find_map(|event| queued(&event["actions"]))
    };
    session
        .until(|session| last_queued(session) == Some(true))
        .await;
    let paused = runtime_answer(
        &mut session,
        &json!({ "type": "heartbeat_update", "action": "pause" }),
    )
    .await;
    answers.push(masked(read_as_the_tui_does(&paused)));
    session
        .until(|session| last_queued(session) == Some(false))
        .await;
    std::fs::write(&release, "go").unwrap();
    session.call("session.wait_idle", json!({})).await;
    let page = session.query("transcript.page").await;
    let rows: Vec<Value> = page["entries"].as_array().cloned().unwrap_or_default();
    Seen {
        answers,
        heartbeat_rows: heartbeat_rows(&rows),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_heartbeat_answers_and_withdraws_as_the_old_daemons_does() {
    let old_bed = Bed::new();
    let mut reference = OldReference::open("heartbeats", &old_bed);
    let old: Seen = reference.take("flow", || old_flow(&old_bed));
    let runtime = runtime_flow(&Bed::new()).await;

    let answered = |command: &str, heartbeat: Value| {
        json!({ "type": "response", "command": command, "success": true,
                "data": { "heartbeat": heartbeat } })
    };
    let refused = |command: &str, error: &str| json!({ "type": "response", "command": command, "success": false, "error": error });
    let status = |answer: &Value| answer["data"]["heartbeat"]["status"].clone();
    // The old daemon's answers: what the scenario asks of it.
    assert_eq!(
        (
            &old.answers[0],
            &old.answers[3],
            &old.answers[4]["data"]["heartbeat"]["deliveryMode"],
            old.answers.iter().map(status).collect::<Vec<_>>(),
            &old.answers[7],
            &old.answers[9],
            &old.answers[10],
            old.heartbeat_rows,
        ),
        (
            &answered("heartbeat_get", Value::Null),
            &refused(
                "heartbeat_set",
                "Heartbeat delivery mode must be \"steer\" or \"follow_up\""
            ),
            &json!("follow_up"),
            [
                Value::Null,
                json!("active"),
                json!("active"),
                Value::Null,
                json!("active"),
                json!("paused"),
                json!("active"),
                Value::Null,
                json!("cancelled"),
                Value::Null,
                Value::Null,
                json!("paused"),
            ]
            .to_vec(),
            &refused("heartbeat_manage", "No active heartbeat found: nope"),
            &answered("heartbeat_get", Value::Null),
            &answered("heartbeat_update", Value::Null),
            0,
        ),
        "{:#?}",
        old.answers
    );
    assert_eq!(
        (runtime.answers, runtime.heartbeat_rows),
        (old.answers, old.heartbeat_rows)
    );
}
