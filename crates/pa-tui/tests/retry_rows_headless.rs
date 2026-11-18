//! Headless e2e for the retry-episode collapse: each failed attempt's run ends (`agent_end`)
//! before its `auto_retry_start`, so the attempt leaves an `Error:` row with a `Worked for`
//! summary under it. A recovered episode keeps neither: one summary row for the whole episode
//! stays. A given-up episode keeps only the terminal retry-failed line.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, write_session_event, MockSupervisor, Request,
};

const RATE_LIMITED: &str = "429 Rate limit exceeded (mock)";
const GAVE_UP: &str =
    "\u{26a0} Error: Retry failed after 2 attempts: 429 Rate limit exceeded (mock)";

/// How the scripted episode settles.
#[derive(Clone, Copy)]
enum Settle {
    Recovered,
    GaveUp,
}

/// One run on the wire: the agent run frames around one assistant message.
fn write_run(events: &mut std::os::unix::net::UnixStream, message: &Value) {
    for event_type in ["agent_start", "turn_start"] {
        write_session_event(events, &json!({ "type": event_type }));
    }
    write_session_event(
        events,
        &json!({ "type": "message_end", "message": message }),
    );
    for event_type in ["turn_end", "agent_end"] {
        write_session_event(events, &json!({ "type": event_type }));
    }
}

fn serve(supervisor: &MockSupervisor, settle: Settle) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                write_json(writer, &attach_response(id, "retry rows"));
                let mut events = writer.try_clone().expect("clone event socket");
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let failed = json!({
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": RATE_LIMITED,
                    });
                    for attempt in 1..=2 {
                        write_run(&mut events, &failed);
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "auto_retry_start",
                                "attempt": attempt,
                                "maxAttempts": 2,
                                "delayMs": 600,
                                "errorMessage": RATE_LIMITED,
                            }),
                        );
                        std::thread::sleep(std::time::Duration::from_millis(700));
                    }
                    let (end, content, success) = match settle {
                        Settle::Recovered => {
                            write_run(
                                &mut events,
                                &json!({
                                    "role": "assistant",
                                    "content": [{ "type": "text", "text": "recovered reply" }],
                                    "stopReason": "stop",
                                }),
                            );
                            (
                                json!({ "type": "auto_retry_end", "success": true, "attempt": 2 }),
                                format!("Recovered after 2 retries: {RATE_LIMITED}"),
                                true,
                            )
                        }
                        Settle::GaveUp => {
                            write_run(&mut events, &failed);
                            (
                                json!({
                                    "type": "auto_retry_end",
                                    "success": false,
                                    "attempt": 2,
                                    "finalError": RATE_LIMITED,
                                }),
                                GAVE_UP.to_string(),
                                false,
                            )
                        }
                    };
                    write_session_event(&mut events, &end);
                    write_session_event(
                        &mut events,
                        &json!({
                            "type": "message_start",
                            "message": {
                                "role": "custom",
                                "customType": "provider_retry_outcome",
                                "content": content,
                                "display": true,
                                "details": {
                                    "success": success,
                                    "attempts": 2,
                                    "finalError": RATE_LIMITED,
                                },
                            },
                        }),
                    );
                });
            }
            _ => return false,
        }
        true
    });
}

/// The settled transcript rows below the attach snapshot, trimmed.
fn settled_rows(settle: Settle) -> Vec<String> {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitMs(2600)],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || serve(&supervisor, settle)
    });
    let last = outcome.frames.last().expect("a frame painted");
    last.lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("Error:")
                || line.starts_with("Worked for")
                || line.starts_with("recovered reply")
                || line.starts_with('\u{2022}')
                || line.starts_with('\u{26a0}')
        })
        // The receipt's measured wait varies run to run; its shape does not.
        .map(|line| match line.split_once("s through ") {
            Some((_, rest)) if line.starts_with('\u{2022}') => {
                format!("\u{2022} Waited Ns through {rest}")
            }
            _ => line.to_string(),
        })
        .collect()
}

#[test]
fn a_recovered_retry_episode_leaves_no_attempt_rows() {
    let rows = settled_rows(Settle::Recovered);
    assert_eq!(
        rows,
        vec![
            "recovered reply".to_string(),
            // One summary for the whole episode: the retried run kept the first run's start.
            "Worked for 1s \u{b7} 0 tool calls \u{b7} 0 tokens".to_string(),
            format!("\u{2022} Waited Ns through 2 retries \u{b7} {RATE_LIMITED}"),
        ]
    );
}

#[test]
fn a_given_up_retry_episode_leaves_one_terminal_error() {
    let rows = settled_rows(Settle::GaveUp);
    assert_eq!(rows, vec![GAVE_UP.to_string()]);
}
