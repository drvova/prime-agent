//! Headless e2e for the CLI reopen's first paint (latency study item R9, a divergence from
//! TS): `--resume`/`--attach` paints the chrome with a loading row where the transcript lands before
//! the attach answers, and keys typed during the open still apply after the attach lands.
#![cfg(unix)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions, SessionSelection};
use serde_json::json;

use support::{
    accept_within_idle_window, attach_response, options, run_headless, serve_connection,
    write_json, MockSupervisor,
};

/// How long the mock daemon sits on the attach: a large session's restore.
const ATTACH_DELAY: Duration = Duration::from_millis(600);

#[test]
fn a_cli_reopen_paints_the_chrome_before_the_attach_answers() {
    let attach_answered_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let answered = Arc::clone(&attach_answered_at);
    let (stamp_tx, stamp_rx) = std::sync::mpsc::channel();
    let plan = HeadlessPlan {
        steps: vec![
            // Typed while the attach is still pending: queued, applied after the open.
            HeadlessStep::WaitMs(100),
            HeadlessStep::Type("hi".to_string()),
            HeadlessStep::WaitRender {
                needle: "settled answer".to_string(),
                timeout_ms: 5_000,
            },
            HeadlessStep::Timestamp(stamp_tx),
        ],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(
        plan,
        |socket| InteractiveOptions {
            fullscreen_mouse: false,
            session: SessionSelection::Attach("s1".to_string()),
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || {
                let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
                    return;
                };
                serve_connection(stream, &[], |writer, request| {
                    if request.kind != "attach" {
                        return false;
                    }
                    std::thread::sleep(ATTACH_DELAY);
                    let mut response = attach_response(request.id, "resumed session");
                    response["data"]["snapshot"]["messages"] = json!([
                        { "role": "user", "content": "the question", "timestamp": 0 },
                        {
                            "role": "assistant",
                            "content": [{ "type": "text", "text": "settled answer" }],
                            "timestamp": 0,
                        },
                    ]);
                    answered
                        .lock()
                        .expect("attach clock")
                        .get_or_insert(Instant::now());
                    write_json(writer, &response);
                    true
                });
            }
        },
    );
    let _ = stamp_rx.recv().expect("the content frame was stamped");
    assert!(
        attach_answered_at.lock().expect("attach clock").is_some(),
        "the attach was answered"
    );
    let first = outcome.frames.first().expect("a first frame");
    assert!(
        first.contains("Loading session") && !first.contains("settled answer"),
        "the first frame is the loading chrome, painted before the attach answered:\n{first}"
    );
    assert!(
        outcome
            .frames
            .iter()
            .all(|frame| !frame.contains("prime agent")),
        "a reopen never renders the brand splash:\n{}",
        outcome.frames.join("\n---frame---\n")
    );
    let last = outcome.frames.last().expect("a last frame");
    assert!(
        last.contains("settled answer") && !last.contains("Loading session"),
        "the attach replaced the loading row with the transcript:\n{last}"
    );
    assert!(
        last.lines().any(|line| line.contains("hi")),
        "the keys typed during the open applied after the attach:\n{last}"
    );
}
