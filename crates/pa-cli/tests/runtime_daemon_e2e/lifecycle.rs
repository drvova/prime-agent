//! The daemon's own end on the runtime daemon (phase 8d-5a), sent through
//! pa-tui's `wire2_compat` and compared with the old daemon (the `pa-daemon`
//! supervisor) on the same faux turns, ported from the old daemon's
//! `supervisor_e2e` shutdown tests and `supervisor_restart_e2e`
//! `graceful_shutdown_continues_the_aborted_turn_after_restart`:
//!
//! - `shutdown`, and the stop signal (SIGTERM): the client that asked reads
//!   its response, then every connected client the daemon's
//!   `daemon_closing` (on wire2 the live-only `system/closing`, mapped back
//!   by the compat layer); the process exits 0, its socket is gone and the
//!   session it held can be leased again;
//! - `restart` while a session's turn runs in a kernel cell: the turn is cut
//!   off, the daemon exits 0, and the next daemon revives the session and
//!   continues the turn with the old daemon's continuation prompt, once (the
//!   old daemon's expectations, replayed on the runtime daemon only).
//!
//! Each daemon runs on a bed of its own.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;
use pa_types::daemon::DaemonCommand;
use pa_types::event_log::TopicFilter;
use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::{turn_rows, Bed, Client, Daemon, FRAME_WAIT};

/// The runtime daemon runs offline, as the old daemon did for its golden.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// How a daemon is stopped.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// A client's `shutdown`.
    Shutdown,
    /// The stop signal its stopper sends.
    Signal,
}

/// What the clients of a daemon saw as it stopped, and what it left.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Stopped {
    /// The stopper's frames: its response, then the closing.
    stopper: Vec<Value>,
    /// Another connected client's frames: the closing.
    watcher: Vec<Value>,
    exit: Option<i32>,
    socket_left: bool,
    /// Whether the session the daemon held can be leased once it is gone.
    lease_free: bool,
}

/// The process's exit code, once it exits.
pub(super) fn exit_code(daemon: &mut Daemon) -> Option<i32> {
    let deadline = Instant::now() + FRAME_WAIT;
    loop {
        if let Some(status) = daemon.0.try_wait().expect("poll the daemon") {
            return status.code();
        }
        assert!(Instant::now() < deadline, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn terminate(daemon: &Daemon) {
    kill(
        Pid::from_raw(i32::try_from(daemon.0.id()).expect("a pid")),
        Signal::SIGTERM,
    )
    .expect("signal the daemon");
}

/// Whether `transcript` can be leased now (no daemon holds it).
fn lease_free(transcript: &Path, agent_dir: &Path) -> bool {
    pa_core::session::lease::acquire_runtime_session_lease(transcript, agent_dir).is_ok()
}

/// A compat client of the runtime daemon: it follows `system/closing` for
/// the connection's life, as the compat layer's transport does.
pub(super) async fn closing_follower(socket: &Path) -> Client {
    let (mut client, _) = Client::connect(socket).await;
    client
        .send(&ClientFrame::Subscribe {
            sub_id: "compat:closing".to_string(),
            filter: TopicFilter::new("system/closing").unwrap(),
            since: Since::Now,
        })
        .await;
    loop {
        if let ServerFrame::Backfilled { .. } = client.frame().await {
            return client;
        }
    }
}

/// What `client` hands the TUI through the compat layer until the runtime
/// closes its connection; `request` is its `shutdown`'s request id.
async fn runtime_frames_until_closed(client: &mut Client, request: Option<&str>) -> Vec<Value> {
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
            ServerFrame::Reply { id, outcome } if Some(id.as_str()) == request => {
                frames.push(pa_tui::wire2_response_frame("shutdown", outcome).unwrap());
            }
            ServerFrame::Event { event, .. } => {
                frames.extend(pa_tui::wire2_live_frames(&[event]).unwrap());
            }
            ServerFrame::Closing { .. } => return frames,
            other => panic!("an unexpected frame as the daemon stops: {other:?}"),
        }
    }
}

async fn runtime_stop(bed: &Bed, stop: Stop) -> Stopped {
    let mut daemon = bed.runtime_daemon_with(&json!({ "responses": [] }), OFFLINE);
    let session = Attached::open(bed, bed.dir.path()).await;
    let transcript = pa_core::session::manager::get_session_file_path(
        &bed.agent_dir().join("sessions"),
        &session.id,
    );
    let mut watcher = closing_follower(&bed.runtime_socket()).await;
    let mut stopper = closing_follower(&bed.runtime_socket()).await;
    let request = match stop {
        Stop::Shutdown => {
            let id = stopper.id();
            let command = DaemonCommand::Shutdown {
                id: None,
                force: None,
                rest: serde_json::Map::default(),
            };
            let mut frames = pa_tui::wire2_request_frames(id.clone(), command).unwrap();
            let (Some(frame), None) = (frames.pop(), frames.pop()) else {
                panic!("shutdown is one request");
            };
            stopper.send(&frame).await;
            Some(id)
        }
        Stop::Signal => {
            terminate(&daemon);
            None
        }
    };
    let stopper = runtime_frames_until_closed(&mut stopper, request.as_deref()).await;
    let watcher = runtime_frames_until_closed(&mut watcher, None).await;
    let exit = exit_code(&mut daemon);
    drop(session);
    Stopped {
        stopper: stopper.iter().map(read_as_the_tui_does).collect(),
        watcher: watcher.iter().map(read_as_the_tui_does).collect(),
        exit,
        socket_left: UnixStream::connect(bed.runtime_socket()).is_ok()
            || bed.runtime_socket().exists(),
        lease_free: lease_free(&transcript, &bed.agent_dir()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_answers_tells_every_client_and_exits_as_on_the_old_daemon() {
    let mut seen = Vec::new();
    let reference = OldReference::open("daemon_stop", &Bed::new());
    for stop in [Stop::Shutdown, Stop::Signal] {
        let old: Stopped = reference.take(&format!("{stop:?}"));
        let runtime = runtime_stop(&Bed::new(), stop).await;
        seen.push((stop, old, runtime));
    }
    let closing = json!({ "type": "daemon_closing", "reason": "shutdown" });
    let stopped = |stopper: Vec<Value>| Stopped {
        stopper,
        watcher: vec![closing.clone()],
        exit: Some(0),
        socket_left: false,
        lease_free: true,
    };
    let [(_, old_shutdown, runtime_shutdown), (_, old_signal, runtime_signal)] =
        <[_; 2]>::try_from(seen).unwrap();
    assert_eq!(
        (&old_shutdown, &old_signal),
        (
            &stopped(vec![
                json!({ "type": "response", "command": "shutdown", "success": true }),
                closing.clone(),
            ]),
            &stopped(vec![closing.clone()]),
        )
    );
    assert_eq!(
        (runtime_shutdown, runtime_signal),
        (old_shutdown, old_signal)
    );
}

/// The turn the restart cuts off: its cell touches `started`, then waits on
/// a program that outlasts the test.
pub(super) fn cut_off_turn() -> Value {
    json!([cell(
        "Running the long command.",
        "(bash \"touch started; /bin/sleep 600\")"
    )])
}

/// What the next daemon answers the continuation with.
const CONTINUED: &str = "continued after the restart";

/// The user and assistant texts of a page of message rows, in order.
fn texts(page: &Value) -> Vec<String> {
    turn_rows(page)
        .into_iter()
        .filter(|(_, role, _)| matches!(role.as_deref(), Some("user" | "assistant")))
        .filter_map(|(_, _, text)| text.filter(|text| !text.is_empty()))
        .collect()
}

/// Waits until `started` exists in the bed's dir: the cell runs.
pub(super) fn await_started(bed: &Bed) {
    let started = bed.dir.path().join("started");
    let deadline = Instant::now() + FRAME_WAIT;
    while !started.exists() {
        assert!(Instant::now() < deadline, "the cell never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The runtime daemon's restart mid-turn: its exit code, the texts once
/// the next daemon continued the turn, and the session's last crash there.
async fn runtime_restart(bed: &Bed) -> (Option<i32>, Vec<String>, Value) {
    let mut daemon = bed.runtime_daemon_with(&json!({ "responses": cut_off_turn() }), OFFLINE);
    let mut session = Attached::open(bed, bed.dir.path()).await;
    session.publish("go", None).await;
    await_started(bed);
    let command: DaemonCommand = serde_json::from_value(json!({ "type": "restart" })).unwrap();
    let request = session.request_id();
    let mut frames = pa_tui::wire2_request_frames(request, command).unwrap();
    let (Some(frame), None) = (frames.pop(), frames.pop()) else {
        panic!("restart is one request");
    };
    let restarted = pa_tui::wire2_response_frame("restart", session.request(&frame).await).unwrap();
    assert_eq!(restarted["success"], true, "{restarted}");
    let exit = exit_code(&mut daemon);
    let id = session.id.clone();
    drop((session, daemon));
    let _next = bed.runtime_daemon_with(&json!({ "responses": [CONTINUED] }), OFFLINE);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let deadline = Instant::now() + FRAME_WAIT;
    loop {
        let page = super::ok(
            client
                .query("transcript.page", json!({ "session": id, "limit": 500 }))
                .await,
        );
        let texts = texts(&page);
        if texts.last().map(String::as_str) == Some(CONTINUED) {
            let snapshot = super::ok(
                client
                    .query("session.snapshot", json!({ "session": id }))
                    .await,
            );
            return (exit, texts, snapshot["lastCrash"].clone());
        }
        assert!(
            Instant::now() < deadline,
            "the turn never continued: {texts:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The old daemon's `supervisor_restart_e2e`
/// `graceful_shutdown_continues_the_aborted_turn_after_restart`, replayed: the
/// cut-off turn's prompt and reply stay, the continuation prompt follows once,
/// and the next daemon's answer ends the transcript.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_mid_turn_is_continued_by_the_next_daemon_as_on_the_old_daemon() {
    let (exit, texts, last_crash) = runtime_restart(&Bed::new()).await;
    assert_eq!(
        (exit, texts, last_crash),
        (
            Some(0),
            vec![
                "go".to_string(),
                "Running the long command.".to_string(),
                UPDATE_RESTART_CONTINUATION_PROMPT.to_string(),
                CONTINUED.to_string(),
            ],
            // A deliberate stop is no crash: the revived session has none.
            Value::Null,
        )
    );
}
