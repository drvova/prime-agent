//! Headless e2e for the live line over parallel tool calls: when one of two
//! executing calls ends, the loader stays on the call still running (its
//! title, the tool activity) instead of falling back to `Waiting` while a
//! tool is still at work.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, write_session_event, MockSupervisor, Request,
};

/// The user row the mock writes after the frames under test: its render proves they landed.
const DRAINED: &str = "parallel-frames-drained";

/// How long after the drain row the mock ends the turn.
const SETTLE_AFTER_MS: u64 = 2000;

impl MockSupervisor {
    /// Serve one client connection: a mid-turn attach, two calls starting in parallel, the
    /// second one ending, then the drain row; the turn ends later with the first call open.
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
                        for (call_id, tool_name) in [("call_bash", "bash"), ("call_edit", "edit")] {
                            write_session_event(
                                &mut events,
                                &json!({
                                    "type": "tool_execution_start",
                                    "toolCallId": call_id,
                                    "toolName": tool_name,
                                    "args": {},
                                }),
                            );
                        }
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "tool_execution_end",
                                "toolCallId": "call_edit",
                                "toolName": "edit",
                                "result": {
                                    "content": [{ "type": "text", "text": "edited" }],
                                    "isError": false,
                                },
                            }),
                        );
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "message_start",
                                "message": {
                                    "role": "user",
                                    "content": [{ "type": "text", "text": DRAINED }],
                                },
                            }),
                        );
                        // The turn settles well after the drain row painted (the plan's
                        // condition wait), so the run ends idle.
                        std::thread::sleep(std::time::Duration::from_millis(SETTLE_AFTER_MS));
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

/// A streaming mid-turn snapshot, so the tool frames land on a running turn.
fn attach_data(id: &str) -> Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis();
    let mut response = attach_response(id, "parallel tools session");
    response["data"]["snapshot"]["state"]["isStreaming"] = json!(true);
    response["data"]["snapshot"]["messages"] = json!([
        {
            "role": "user",
            "content": [{ "type": "text", "text": "edit and build" }],
            "timestamp": now_ms,
        },
    ]);
    response
}

#[test]
fn the_live_line_stays_on_the_tool_still_running_when_a_parallel_one_ends() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitRender {
            needle: DRAINED.to_string(),
            timeout_ms: 10_000,
        }],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || supervisor.serve()
    });

    let drained = outcome
        .frames
        .iter()
        .find(|frame| frame.contains(DRAINED))
        .expect("the drain row painted");
    assert!(
        drained.lines().any(|line| line.contains("Running bash")),
        "the still-running call keeps the live line:\n{drained}"
    );
    assert!(
        !drained.lines().any(|line| line.contains("Waiting")),
        "no waiting fallback while a call runs:\n{drained}"
    );
}
