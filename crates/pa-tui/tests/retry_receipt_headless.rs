//! Headless e2e for the provider retry episode on the live line: each
//! `auto_retry_start` shows the `retry N of M in Ns · <provider> <status>`
//! countdown, and the recovered episode's `provider_retry_outcome` row
//! lands as the `• Waited Ns through N retries` receipt: the episode's
//! summed backoff, never an attempt's streaming time (nc's retry bar and
//! receipt).
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, write_session_event, MockSupervisor, Request,
};

/// The provider's words for both failed attempts.
const RATE_LIMITED: &str = "429 Rate limit exceeded (mock)";

/// Each retry's backoff, and how long the mock waits before the next frame.
const RETRY_DELAY_MS: u64 = 1200;
const RETRY_GAP_MS: u64 = 1300;

/// How long the recovered attempt streams before the episode's `auto_retry_end`: time the
/// receipt must not count as waiting.
const RECOVERED_STREAM_MS: u64 = 900;

/// The scripted retry stream the mock writes after the attach (one per test).
#[derive(Clone, Copy)]
enum Scenario {
    /// Two counted-down retries, a recovered attempt that streams, then the outcome row.
    Recovered,
    /// The daemon's real order: a retry the user aborts (no `auto_retry_end`), then a new prompt
    /// whose attempts are each their own run (`agent_start`, the failed message, `agent_end`,
    /// then `auto_retry_start`) until the third recovers: the receipt survives the attempts' runs
    /// and counts only the new episode.
    AbortedThenWireOrder,
}

/// The episode's `auto_retry_start` frame.
fn retry_start(attempt: u32, delay_ms: u64) -> Value {
    json!({
        "type": "auto_retry_start",
        "attempt": attempt,
        "maxAttempts": 3,
        "delayMs": delay_ms,
        "errorMessage": RATE_LIMITED,
    })
}

/// One failed attempt's assistant message (the provider error the retry follows).
fn failed_attempt() -> Value {
    json!({
        "type": "message_end",
        "message": {
            "role": "assistant",
            "content": [],
            "stopReason": "error",
            "errorMessage": RATE_LIMITED,
            "timestamp": 1,
        },
    })
}

/// A recovered episode's `auto_retry_end` and its `provider_retry_outcome` row.
fn recovered(events: &mut std::os::unix::net::UnixStream, attempts: u32) {
    write_session_event(
        events,
        &json!({ "type": "auto_retry_end", "success": true, "attempt": attempts }),
    );
    let retries = if attempts == 1 { "retry" } else { "retries" };
    write_session_event(
        events,
        &json!({
            "type": "message_start",
            "message": {
                "role": "custom",
                "customType": "provider_retry_outcome",
                "content": format!("Recovered after {attempts} {retries}: {RATE_LIMITED}"),
                "display": true,
                "details": {
                    "success": true,
                    "attempts": attempts,
                    "finalError": RATE_LIMITED,
                },
            },
        }),
    );
}

impl MockSupervisor {
    /// Serve one client connection: a mid-turn attach, then `scenario`'s retry stream.
    fn serve(self, scenario: Scenario) {
        let Some(stream) = accept_within_idle_window(&self.listener) else {
            return;
        };
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => write_json(writer, &create_response(id)),
                "attach" => {
                    write_json(writer, &attach_data(id));
                    let mut events = writer.try_clone().expect("clone event socket");
                    std::thread::spawn(move || match scenario {
                        Scenario::Recovered => {
                            std::thread::sleep(std::time::Duration::from_millis(300));
                            for attempt in 1..=2 {
                                write_session_event(
                                    &mut events,
                                    &retry_start(attempt, RETRY_DELAY_MS),
                                );
                                std::thread::sleep(std::time::Duration::from_millis(RETRY_GAP_MS));
                            }
                            // The recovered attempt streams before the loop reports the end.
                            write_session_event(
                                &mut events,
                                &json!({
                                    "type": "message_update",
                                    "message": {
                                        "role": "assistant",
                                        "content": [{ "type": "text", "text": "deploying" }],
                                    },
                                }),
                            );
                            std::thread::sleep(std::time::Duration::from_millis(
                                RECOVERED_STREAM_MS,
                            ));
                            recovered(&mut events, 2);
                            for event_type in ["turn_end", "agent_end"] {
                                write_session_event(&mut events, &json!({ "type": event_type }));
                            }
                        }
                        Scenario::AbortedThenWireOrder => {
                            // The aborted attempt never reports its episode's end.
                            write_session_event(&mut events, &retry_start(1, 30_000));
                            for event_type in ["turn_end", "agent_end"] {
                                write_session_event(&mut events, &json!({ "type": event_type }));
                            }
                            // Each failed attempt ends its own run before its retry starts.
                            for attempt in 1..=2 {
                                write_session_event(&mut events, &json!({ "type": "agent_start" }));
                                write_session_event(&mut events, &failed_attempt());
                                for event_type in ["turn_end", "agent_end"] {
                                    write_session_event(
                                        &mut events,
                                        &json!({ "type": event_type }),
                                    );
                                }
                                write_session_event(
                                    &mut events,
                                    &retry_start(attempt, RETRY_DELAY_MS),
                                );
                            }
                            // The recovered attempt's run reports the episode's end.
                            write_session_event(&mut events, &json!({ "type": "agent_start" }));
                            recovered(&mut events, 2);
                            for event_type in ["turn_end", "agent_end"] {
                                write_session_event(&mut events, &json!({ "type": event_type }));
                            }
                        }
                    });
                }
                _ => return false,
            }
            true
        });
    }
}

/// A streaming mid-turn snapshot on an `openrouter` model, so the countdown names it.
fn attach_data(id: &str) -> Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis();
    let mut response = attach_response(id, "retry session");
    response["data"]["snapshot"]["state"]["isStreaming"] = json!(true);
    response["data"]["snapshot"]["state"]["model"] =
        json!({ "id": "openai/gpt-5.5", "provider": "openrouter" });
    response["data"]["snapshot"]["messages"] = json!([
        {
            "role": "user",
            "content": [{ "type": "text", "text": "deploy it" }],
            "timestamp": now_ms,
        },
    ]);
    response
}

/// The receipt row's text after `• Waited ` in the newest frame that shows it.
fn receipt(frames: &[String]) -> String {
    frames
        .iter()
        .rev()
        .find_map(|frame| {
            frame
                .lines()
                .map(str::trim)
                .find_map(|line| line.strip_prefix("\u{2022} Waited "))
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("the receipt row lands:\n{}", frames.join("\n")))
}

#[test]
fn a_recovered_retry_episode_counts_down_then_leaves_the_waited_receipt() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitRender {
            needle: "\u{2022} Waited ".to_string(),
            timeout_ms: 10_000,
        }],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || supervisor.serve(Scenario::Recovered)
    });

    let countdown = "retry 2 of 3 in 1s \u{00b7} openrouter 429";
    assert!(
        outcome.frames.iter().any(|frame| frame
            .lines()
            .any(|line| line.trim_end().ends_with(countdown))),
        "the second attempt counts down on the live line"
    );
    let last = outcome.frames.last().expect("a frame painted");
    assert!(!last.contains("Recovered after"), "{last}");
    // The wait is the two backoffs (2 x 1.2s): neither the gap after each countdown nor the
    // recovered attempt's streaming counts.
    assert_eq!(
        receipt(&outcome.frames),
        format!("2.4s through 2 retries \u{00b7} {RATE_LIMITED}")
    );
}

#[test]
fn the_receipt_survives_the_attempts_runs_and_drops_an_aborted_episode() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitRender {
            needle: "\u{2022} Waited ".to_string(),
            timeout_ms: 10_000,
        }],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || supervisor.serve(Scenario::AbortedThenWireOrder)
    });

    // The two 1.2s backoffs, not the aborted episode's 30s and not the daemon's fallback text.
    assert_eq!(
        receipt(&outcome.frames),
        format!("2.4s through 2 retries \u{00b7} {RATE_LIMITED}")
    );
}
