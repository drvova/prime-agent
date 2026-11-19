//! Headless e2e for the run summary row's prompt-cache segment: a run whose requests read from
//! the prompt cache ends with ` Worked for … · N% cached`, the hit rate summed over the run's
//! requests (`cache_read / (input + cache_read + cache_write)`); a run without cache reads keeps
//! the plain row, and a pane too narrow for the whole row drops the segment first.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixStream;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    attach_response, create_response, detach_response, options, run_headless, serve_connection,
    session_stats_response, write_json, MockSupervisor, Request,
};

impl MockSupervisor {
    /// Serve one connection: a prompt of `cached` streams a two-request run that reads from the
    /// cache; any other prompt streams one request with no cache data.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(stream, &[], |writer, Request { id, kind, command }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &attach_response(id, "cache session"));
                }
                "get_session_stats" => {
                    write_json(writer, &session_stats_response(id));
                }
                "prompt" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                    let requests = if command["message"] == "cached" {
                        vec![usage(100, 50, 900, 0), usage(50, 50, 1800, 50)]
                    } else {
                        vec![usage(400, 100, 0, 0)]
                    };
                    stream_run(writer, &requests);
                }
                "detach" => {
                    write_json(writer, &detach_response(id));
                }
                _ => return false,
            }
            true
        });
    }
}

/// One request's wire usage (`totalTokens` is the four-field sum).
fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "totalTokens": input + output + cache_read + cache_write,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    })
}

/// One scripted run: one completed assistant message per request usage.
fn stream_run(writer: &mut UnixStream, requests: &[Value]) {
    let event = |payload: Value| json!({ "type": "session_event", "activeSessionId": "s1", "event": payload });
    write_json(writer, &event(json!({ "type": "turn_start" })));
    for (index, usage) in requests.iter().enumerate() {
        let text = format!("reply {index}");
        write_json(
            writer,
            &event(json!({
                "type": "message_start",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "" }] },
                "assistantMessageEvent": { "type": "start" },
            })),
        );
        write_json(
            writer,
            &event(json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }],
                    "usage": usage,
                    "stopReason": "stop",
                    "timestamp": 1,
                },
            })),
        );
    }
    write_json(writer, &event(json!({ "type": "turn_end" })));
    write_json(writer, &event(json!({ "type": "agent_end" })));
}

/// Run the prompts at `width`, hold until a frame shows the last run's summary (its `settled`
/// text), and return every summary row that frame shows, trailing padding trimmed.
fn summary_rows(width: u16, prompts: &[&str], settled: &str) -> Vec<String> {
    let mut steps = Vec::new();
    for prompt in prompts {
        steps.push(HeadlessStep::Submit((*prompt).to_string()));
        steps.push(HeadlessStep::WaitIdle { timeout_ms: 10_000 });
    }
    steps.push(HeadlessStep::WaitRender {
        needle: settled.to_string(),
        timeout_ms: 10_000,
    });
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve()
        },
    );
    let last = outcome
        .frames
        .iter()
        .rev()
        .find(|frame| frame.contains(settled))
        .expect("the last run's summary rendered");
    last.lines()
        .filter(|line| line.contains("Worked for"))
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// A run that read from the cache carries the run-wide hit rate (2700 of 2900 prompt tokens,
/// not either request's own 90% or 95%); a run with no cache data keeps the plain row.
#[test]
fn the_run_summary_shows_the_runs_cache_hit_rate() {
    assert_eq!(
        summary_rows(100, &["cached", "fresh"], "500 tokens"),
        vec![
            " Worked for 0s \u{b7} 0 tool calls \u{b7} 3.0k tokens \u{b7} 93% cached".to_string(),
            " Worked for 0s \u{b7} 0 tool calls \u{b7} 500 tokens".to_string(),
        ]
    );
}

/// A pane too narrow for the whole row drops the cache segment first and keeps the rest on one
/// line.
#[test]
fn a_narrow_pane_drops_the_cache_segment_first() {
    assert_eq!(
        summary_rows(50, &["cached"], "3.0k tokens"),
        vec![" Worked for 0s \u{b7} 0 tool calls \u{b7} 3.0k tokens".to_string()]
    );
}
