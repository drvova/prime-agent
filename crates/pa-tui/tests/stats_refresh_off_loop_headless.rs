//! Headless e2e for the turn-end stats refresh: the refresh of the tray's context usage runs off
//! the run loop (AGENTS.md "Performance and the critical path"). The regression: the loop awaited
//! `get_session_stats` inline after the turn settled, so a daemon slow to answer it (a busy worker,
//! a wedged socket) held every key and every paint behind the 10 s request timeout.
#![cfg(unix)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use serde_json::json;

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, session_stats_response, write_json, MockSupervisor,
};

/// How long the attached turn runs before `turn_end`/`agent_end` arrive.
const TURN_MS: u64 = 300;
/// When the plan presses its key: well after the turn settled and the stats request went out.
const KEY_AT_MS: u64 = 900;
/// How long the daemon sits on `get_session_stats`: far past the key's paint budget.
const STATS_REPLY_DELAY: Duration = Duration::from_secs(2);
/// The key's paint budget.
const PAINT_BUDGET: Duration = Duration::from_millis(200);

#[test]
fn a_key_after_the_turn_paints_while_the_stats_request_hangs() {
    let stats_asked_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let asked = Arc::clone(&stats_asked_at);
    let (stamp_tx, stamp_rx) = std::sync::mpsc::channel();
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::WaitMs(KEY_AT_MS),
            HeadlessStep::Type("q".to_string()),
            HeadlessStep::Timestamp(stamp_tx),
        ],
        width: 100,
        height: 30,
    };
    // The plan's clock starts after this instant, so `started + KEY_AT_MS` is no later than the
    // moment the key is sent: the measured latency is an upper bound on the real one.
    let started = Instant::now();
    let outcome = run_headless(
        plan,
        |socket| InteractiveOptions {
            fullscreen_mouse: false,
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || {
                let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
                    return;
                };
                serve_connection(stream, &[], |writer, request| {
                    match request.kind {
                        "create" => write_json(writer, &create_response(request.id)),
                        "attach" => {
                            let mut response = attach_response(request.id, "stats session");
                            response["data"]["snapshot"]["state"]["isStreaming"] = json!(true);
                            write_json(writer, &response);
                            let mut events = writer.try_clone().expect("clone event socket");
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_millis(TURN_MS));
                                for event_type in ["turn_end", "agent_end"] {
                                    write_json(
                                        &mut events,
                                        &json!({
                                            "type": "session_event",
                                            "activeSessionId": "s1",
                                            "event": { "type": event_type },
                                        }),
                                    );
                                }
                            });
                        }
                        // The daemon answers the stats request only long after the key: a
                        // loop that awaits it inline paints the key late.
                        "get_session_stats" => {
                            asked
                                .lock()
                                .expect("stats clock")
                                .get_or_insert(Instant::now());
                            let mut late = writer.try_clone().expect("clone reply socket");
                            let id = request.id.to_string();
                            std::thread::spawn(move || {
                                std::thread::sleep(STATS_REPLY_DELAY);
                                write_json(&mut late, &session_stats_response(&id));
                            });
                        }
                        _ => return false,
                    }
                    true
                });
            }
        },
    );
    let (painted_at, _renders) = stamp_rx.recv().expect("the key's frame was stamped");
    let asked_at = stats_asked_at
        .lock()
        .expect("stats clock")
        .expect("the settled turn asked for the session stats");
    let key_sent_by = started + Duration::from_millis(KEY_AT_MS);
    assert!(
        asked_at < key_sent_by,
        "the stats request is in flight before the key is pressed"
    );
    let latency = painted_at.saturating_duration_since(key_sent_by);
    assert!(
        latency < PAINT_BUDGET,
        "the key painted {latency:?} after it was pressed (budget {PAINT_BUDGET:?})"
    );
    assert!(
        outcome
            .frames
            .last()
            .is_some_and(|frame| frame.lines().any(|line| line.contains('q'))),
        "the typed key reached the frame"
    );
}
