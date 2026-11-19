//! Headless e2e for the tray's context indicator through a compaction. The regression (the user's
//! "the context line disappears in long-running threads as context fills"): a compaction leaves
//! the session's usage unknown until the next reply (`contextUsage.tokens: null`), and the
//! settled-turn stats refresh folded that into "no usage", so the gauge and its `tokens/window %`
//! label vanished from the tray for as long as the long turn ran. The tray must keep the last
//! known value, marked stale, until a reply measures it again.
#![cfg(unix)]

mod support;

use std::time::Duration;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use serde_json::json;

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, MockSupervisor,
};

/// How long the attached turn runs before it settles.
const TURN_MS: u64 = 300;

#[test]
fn the_context_label_survives_the_unknown_usage_after_a_compaction() {
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::WaitRender {
                needle: "90k/128k".to_string(),
                timeout_ms: 10_000,
            },
            // The settled turn's stats refresh answers with the post-compaction usage.
            HeadlessStep::WaitRender {
                needle: "~90k/128k".to_string(),
                timeout_ms: 5_000,
            },
        ],
        width: 100,
        height: 30,
    };
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
                            // A long session near its window, mid-turn.
                            let mut response = attach_response(request.id, "long session");
                            let state = &mut response["data"]["snapshot"]["state"];
                            state["isStreaming"] = json!(true);
                            state["contextUsage"] =
                                json!({ "tokens": 90_000, "contextWindow": 128_000 });
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
                        // Right after a compaction the daemon cannot measure the context.
                        "get_session_stats" => write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": request.id,
                                "command": "get_session_stats",
                                "success": true,
                                "data": {
                                    "contextUsage": {
                                        "tokens": null,
                                        "contextWindow": 128_000,
                                        "percent": null,
                                    },
                                },
                            }),
                        ),
                        _ => return false,
                    }
                    true
                });
            }
        },
    );
    let last = outcome.frames.last().expect("a settled frame");
    assert!(
        last.lines()
            .any(|line| line.contains("~90k/128k 70%") && line.contains('\u{2501}')),
        "the tray keeps the gauge and the last known usage, marked stale: {last}"
    );
}
