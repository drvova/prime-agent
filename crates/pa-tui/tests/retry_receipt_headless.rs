//! Headless e2e for the provider retry episode on the live line: each
//! `auto_retry_start` shows the `retry N of M in Ns · <provider> <status>`
//! countdown, and the recovered episode's `provider_retry_outcome` row
//! lands as the `• Waited Ns through N retries` receipt with the wait this
//! client measured (nc's retry bar and receipt).
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

impl MockSupervisor {
    /// Serve one client connection: a mid-turn attach, then two retries and the recovery.
    fn serve(self) {
        let Some(stream) = accept_within_idle_window(&self.listener) else {
            return;
        };
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => write_json(writer, &create_response(id)),
                "attach" => {
                    write_json(writer, &attach_data(id));
                    let mut events = writer.try_clone().expect("clone event socket");
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        for attempt in 1..=2 {
                            write_session_event(
                                &mut events,
                                &json!({
                                    "type": "auto_retry_start",
                                    "attempt": attempt,
                                    "maxAttempts": 3,
                                    "delayMs": RETRY_DELAY_MS,
                                    "errorMessage": RATE_LIMITED,
                                }),
                            );
                            std::thread::sleep(std::time::Duration::from_millis(RETRY_GAP_MS));
                        }
                        write_session_event(
                            &mut events,
                            &json!({ "type": "auto_retry_end", "success": true, "attempt": 2 }),
                        );
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "message_start",
                                "message": {
                                    "role": "custom",
                                    "customType": "provider_retry_outcome",
                                    "content": format!("Recovered after 2 retries: {RATE_LIMITED}"),
                                    "display": true,
                                    "details": {
                                        "success": true,
                                        "attempts": 2,
                                        "finalError": RATE_LIMITED,
                                    },
                                },
                            }),
                        );
                        for event_type in ["turn_end", "agent_end"] {
                            write_session_event(&mut events, &json!({ "type": event_type }));
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

#[test]
fn a_recovered_retry_episode_counts_down_then_leaves_the_waited_receipt() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitMs(4200)],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || supervisor.serve()
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
    let receipt = last
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("\u{2022} Waited "))
        .unwrap_or_else(|| panic!("the receipt row lands:\n{last}"));
    let (waited, rest) = receipt.split_once('s').expect("the wait in seconds");
    assert_eq!(rest, format!(" through 2 retries \u{00b7} {RATE_LIMITED}"));
    let waited: f64 = waited.parse().expect("a decimal wait");
    let gaps = std::time::Duration::from_millis(2 * RETRY_GAP_MS).as_secs_f64();
    assert!(
        waited >= gaps - 0.2,
        "the receipt measures both waits ({waited}s)"
    );
}
