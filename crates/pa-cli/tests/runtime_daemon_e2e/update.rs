//! The update restart on the runtime daemon (phase 8d-5b), sent through
//! pa-tui's `wire2_compat` and compared with the old daemon (the `pa-daemon`
//! supervisor) on the same faux turn, the coordinator's side of
//! `docs/legacy-upgrade-contract.md`'s handoff:
//!
//! - `prepare_update_restart` while a session's turn runs in a kernel cell:
//!   answered `prepared` (the drain, the roster written durably); a repeat
//!   answers the same state, another id is refused with its typed info, and a
//!   prompt meanwhile is refused with the old `Daemon is preparing an update
//!   restart` and `update_restarting`;
//! - the roster each daemon wrote: the session's row (its transcript, a
//!   running turn as `streaming`, to be resumed), no subagents or heartbeats;
//! - `commit_update_restart`: answered `stopping` with the stopped count,
//!   every other client told `daemon_closing` with the update's payload
//!   (`resume`, `estSeconds`, the session), and the daemon exits 0.
//!
//! Each daemon runs on a bed of its own.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::Path;

use pa_types::daemon::update_flow::{socket_update_dir, update_prepared_dir, update_roster_path};
use pa_types::daemon::{DaemonCommand, UpdateId};
use pa_types::wire2::ServerFrame;
use serde_json::{json, Value};

use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;

use super::lifecycle::{await_started, closing_follower, cut_off_turn, exit_code};
use super::old_reference::OldReference;
use super::switches::read_as_the_tui_does;
use super::turns::Attached;
use super::{Bed, Client, FRAME_WAIT};

/// The runtime daemon runs offline, as the old daemon did for its golden.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// The update both coordinators drive, and the version they say they update to.
const UPDATE: &str = "u-e2e-update";
const TO_VERSION: &str = "99.0.0";

/// The commands the coordinator sends, in order: the prepare, its repeat,
/// another update's prepare, a prompt while prepared, the commit.
fn coordinator_commands(session: &str) -> Vec<(&'static str, Value)> {
    vec![
        (
            "prepare_update_restart",
            json!({ "type": "prepare_update_restart", "updateId": UPDATE, "toVersion": TO_VERSION }),
        ),
        (
            "prepare_update_restart",
            json!({ "type": "prepare_update_restart", "updateId": UPDATE, "toVersion": TO_VERSION }),
        ),
        (
            "prepare_update_restart",
            json!({ "type": "prepare_update_restart", "updateId": "u-other" }),
        ),
        (
            "prompt",
            json!({ "type": "prompt", "activeSessionId": session, "message": "too late" }),
        ),
        (
            "commit_update_restart",
            json!({ "type": "commit_update_restart", "updateId": UPDATE }),
        ),
    ]
}

/// What one daemon answered and left.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Updated {
    /// The coordinator's responses, in order.
    responses: Vec<Value>,
    /// Another connected client's closing frames.
    watcher: Vec<Value>,
    exit: Option<i32>,
    /// The roster the prepare wrote, normalized ([`roster_view`]).
    roster: Value,
    /// The next daemon's `update_restore_status`, once complete.
    restore_status: Value,
    /// The restored session's texts once the next daemon queued its
    /// continuation (the runtime's answer to it checked and left out).
    texts: Vec<String>,
}

/// What the next daemon answers the continuation with.
const CONTINUED: &str = "continued after the update";

/// The user and assistant texts of a `get_messages` answer's `data`.
pub(super) fn message_texts(data: &Value) -> Vec<String> {
    data["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| matches!(message["role"].as_str(), Some("user" | "assistant")))
        .filter_map(|message| match &message["content"] {
            Value::String(text) => Some(text.clone()),
            Value::Array(blocks) => Some(
                blocks
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Asks `request` (an old command's response) until `done` holds for it.
async fn until_answered<F, Fut>(mut request: F, done: impl Fn(&Value) -> bool) -> Value
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    let deadline = std::time::Instant::now() + FRAME_WAIT;
    loop {
        let response = request().await;
        if done(&response) {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never answered as waited for: {response}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The restore status is complete.
fn restore_complete(response: &Value) -> bool {
    response["data"]["complete"] == true
}

/// The restored session's last text is `text`.
fn ends_with(text: &str) -> impl Fn(&Value) -> bool + '_ {
    move |response| message_texts(&response["data"]).last().map(String::as_str) == Some(text)
}

/// A response or closing with its times and the session's durable id
/// replaced (the one session, whose transcript's stem it is).
fn normalized(mut frame: Value, transcript: &Path) -> Value {
    if let Some(data) = frame.get_mut("data").and_then(Value::as_object_mut) {
        for key in ["acceptedAt", "expiresAt"] {
            if data.contains_key(key) {
                data.insert(key.to_string(), json!("<time>"));
            }
        }
    }
    let stem = transcript
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if let Some(sessions) = frame
        .get_mut("payload")
        .and_then(|payload| payload.get_mut("sessions"))
        .and_then(Value::as_array_mut)
    {
        for row in sessions {
            assert_eq!(row["sessionId"], json!(stem), "{row}");
            row["sessionId"] = json!("<session>");
        }
    }
    frame
}

/// The roster `socket`'s daemon wrote for [`UPDATE`], as both daemons write
/// it: the session rows (the session's ids, transcript and socket replaced,
/// its queue lanes), the subagent and heartbeat rows. The worker rows (the
/// old daemon's process per session; the runtime daemon has none), the
/// create command each row reopens its session with, and the times and
/// process identities are each daemon's own.
fn roster_view(bed: &Bed, socket: &Path, transcript: &Path) -> Value {
    let roster: Value =
        serde_json::from_str(&std::fs::read_to_string(roster_file(bed, socket)).unwrap()).unwrap();
    assert!(
        roster["supervisor"]["pid"].as_u64().unwrap() > 0,
        "{roster}"
    );
    assert_eq!(roster["socket_path"], json!(socket), "{roster}");
    let sessions: Vec<Value> = roster["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["session_file"], json!(transcript), "{row}");
            assert_eq!(row["cwd"], json!(bed.dir.path()), "{row}");
            let lanes = row["queue"]["actions"]
                .get("queue")
                .unwrap_or(&row["queue"]["actions"]);
            json!({
                "kind": row["kind"],
                "name": row["name"],
                "cwd": "<bed>",
                "rlm_depth": row["rlm_depth"],
                "parent_session_id": row["parent_session_id"],
                "next_turn": row["queue"]["next_turn"],
                "steering": lanes.get("steering").cloned().unwrap_or(json!([])),
                "followUps": lanes.get("followUps").cloned().unwrap_or(json!([])),
                "in_flight": row["in_flight"],
                "should_resume": row["should_resume"],
            })
        })
        .collect();
    json!({
        "format_version": roster["format_version"],
        "update_id": roster["update_id"],
        "to_version": roster["binary"]["to_version"],
        "sessions": sessions,
        "subagents": roster.get("subagents").cloned().unwrap_or(json!([])),
        "heartbeats": roster.get("heartbeats").cloned().unwrap_or(json!([])),
    })
}

/// The roster `socket`'s daemon wrote for [`UPDATE`].
fn roster_file(bed: &Bed, socket: &Path) -> std::path::PathBuf {
    update_roster_path(&update_prepared_dir(
        &socket_update_dir(
            &bed.agent_dir(),
            &pa_core::update::restart::socket_hash(socket),
        ),
        &UpdateId::from(UPDATE.to_string()),
    ))
}

/// The next daemon's environment: offline, with the roster the prepare wrote.
fn successor_env(roster: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("PI_OFFLINE", "1".to_string()),
        (
            pa_types::daemon::update_flow::UPDATE_ROSTER_ENV,
            roster.to_string_lossy().into_owned(),
        ),
    ]
}

/// `client`'s request for old `command` through the compat layer, and its
/// response as the TUI reads it.
pub(super) async fn compat_request(client: &mut Client, kind: &str, command: Value) -> Value {
    let command: DaemonCommand = serde_json::from_value(command).unwrap();
    let id = client.id();
    let mut frames = pa_tui::wire2_request_frames(id.clone(), command).unwrap();
    let (Some(frame), None) = (frames.pop(), frames.pop()) else {
        panic!("{kind} is one request");
    };
    client.send(&frame).await;
    loop {
        match client.frame().await {
            ServerFrame::Reply {
                id: replied,
                outcome,
            } if replied == id => {
                return read_as_the_tui_does(&pa_tui::wire2_response_frame(kind, outcome).unwrap());
            }
            ServerFrame::Closing { reason } => panic!("closed before {kind}'s reply: {reason}"),
            _ => {}
        }
    }
}

/// The old frames `client` (a closing follower) hands the TUI until the
/// runtime closes its connection.
async fn closing_frames(client: &mut Client) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let read = tokio::time::timeout(FRAME_WAIT, client.frames.read_frame())
            .await
            .expect("a frame within the wait")
            .expect("a readable frame");
        let Some(read) = read else {
            panic!("the connection ended without `closing`: {frames:?}");
        };
        match serde_json::from_value::<ServerFrame>(read.header).unwrap() {
            ServerFrame::Event { event, .. } => {
                frames.extend(pa_tui::wire2_live_frames(&[event]).unwrap());
            }
            ServerFrame::Closing { .. } => return frames,
            other => panic!("an unexpected frame as the daemon stops: {other:?}"),
        }
    }
}

async fn runtime_update(bed: &Bed) -> Updated {
    let mut daemon = bed.runtime_daemon_with(&json!({ "responses": cut_off_turn() }), OFFLINE);
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let transcript = pa_core::session::manager::get_session_file_path(
        &bed.agent_dir().join("sessions"),
        &session.id,
    );
    session.publish("go", None).await;
    await_started(bed);
    let id = session.id.clone();
    let mut watcher = closing_follower(&bed.runtime_socket()).await;
    let (mut coordinator, _) = Client::connect(&bed.runtime_socket()).await;
    let mut responses = Vec::new();
    for (kind, command) in coordinator_commands(&id) {
        let response = compat_request(&mut coordinator, kind, command).await;
        responses.push(normalized(response, &transcript));
    }
    let watcher = closing_frames(&mut watcher)
        .await
        .into_iter()
        .map(|frame| normalized(read_as_the_tui_does(&frame), &transcript))
        .collect();
    let exit = exit_code(&mut daemon);
    drop(session);
    let roster = roster_view(bed, &bed.runtime_socket(), &transcript);
    let env = successor_env(&roster_file(bed, &bed.runtime_socket()));
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let _next = bed.runtime_daemon_with(&json!({ "responses": [CONTINUED] }), &env);
    let socket = bed.runtime_socket();
    let ask = |command: Value| {
        let socket = socket.clone();
        async move {
            let kind = command["type"].as_str().unwrap().to_string();
            let (mut client, _) = Client::connect(&socket).await;
            compat_request(&mut client, &kind, command).await
        }
    };
    let status = until_answered(
        || ask(json!({ "type": "update_restore_status" })),
        restore_complete,
    )
    .await;
    let messages = until_answered(
        || ask(json!({ "type": "get_messages", "activeSessionId": id })),
        ends_with(CONTINUED),
    )
    .await;
    // The continuation's answer: the old daemon's faux worker cannot answer
    // after its relaunch (it restores the transcript's faux model before its
    // script registers it), so the old daemon's run is compared up to the
    // continuation, and the answer is the runtime's alone (the old daemon's
    // own continuation is `supervisor_restart_e2e`'s).
    let mut texts = message_texts(&messages["data"]);
    assert_eq!(texts.pop().as_deref(), Some(CONTINUED));
    Updated {
        responses,
        watcher,
        exit,
        roster,
        restore_status: status,
        texts,
    }
}

/// The old daemon's prepare, commit and the next daemon's restore, replayed on
/// the runtime daemon: the same responses, refusals, closing, roster, restore
/// status and continued turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_prepare_commit_and_restore_answer_and_continue_as_on_the_old_daemon() {
    let old: Updated = OldReference::open("update_restart", &Bed::new()).take("update");
    let runtime = runtime_update(&Bed::new()).await;
    let response = |command: &str, data: Value| json!({ "type": "response", "command": command, "success": true, "data": data });
    let prepared = json!({ "updateId": UPDATE, "state": "prepared", "acceptedAt": "<time>",
                           "expiresAt": "<time>" });
    let budget = pa_types::daemon::UpdateTimeoutBudget::default();
    assert_eq!(
        old,
        Updated {
            responses: vec![
                response("prepare_update_restart", prepared.clone()),
                response("prepare_update_restart", {
                    let mut repeat = prepared;
                    repeat.as_object_mut().unwrap().remove("expiresAt");
                    repeat
                }),
                json!({ "type": "response", "command": "prepare_update_restart",
                        "success": false, "error": "Daemon is already preparing an update restart",
                        "errorInfo": { "code": "update_prepare_refused", "activeUpdateId": UPDATE } }),
                json!({ "type": "response", "command": "prompt", "success": false,
                        "error": "Daemon is preparing an update restart",
                        "errorInfo": { "code": "update_restarting" } }),
                response(
                    "commit_update_restart",
                    json!({ "updateId": UPDATE, "state": "stopping", "stopped": 1 })
                ),
            ],
            watcher: vec![
                json!({ "type": "daemon_closing", "reason": "update", "payload": {
                "updateId": UPDATE, "resume": true,
                "estSeconds": (budget.boot_ms + budget.restore_overall_ms) / 1000,
                "sessions": [{ "sessionId": "<session>", "name": "" }],
            } })
            ],
            exit: Some(0),
            roster: json!({
                "format_version": 1,
                "update_id": UPDATE,
                "to_version": TO_VERSION,
                "sessions": [{
                    "kind": "top-level", "name": null, "cwd": "<bed>",
                    "rlm_depth": 0, "parent_session_id": null, "next_turn": [],
                    "steering": [], "followUps": [],
                    "in_flight": { "streaming": true, "compacting": false, "bash_running": false,
                                   "rlm_children": false, "retrying": false,
                                   "prompt_in_flight": false },
                    "should_resume": true,
                }],
                "subagents": [],
                "heartbeats": [],
            }),
            restore_status: response(
                "update_restore_status",
                json!({
                    "updateId": UPDATE, "inFlight": false, "complete": true,
                    "counts": { "total": 1, "restored": 1, "resumed": 1, "failed": 0 },
                    "failures": [],
                })
            ),
            texts: vec![
                "go".to_string(),
                "Running the long command.".to_string(),
                UPDATE_RESTART_CONTINUATION_PROMPT.to_string(),
            ],
        },
        "the old daemon's own expectations"
    );
    assert_eq!(runtime, old);
}
