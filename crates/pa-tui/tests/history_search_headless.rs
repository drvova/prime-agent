//! Headless e2e for the ctrl+r prompt-history search: the shortcut opens the search over the
//! prompts sent this session, the query narrows it, and Enter puts the picked prompt in the
//! composer.
#![cfg(unix)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::json;

use support::{
    attach_response, create_response, options, run_headless, serve_connection, write_json,
    write_session_event, MockSupervisor, Request,
};

/// Serve one connection: an empty session whose prompts each run an empty turn.
fn serve(supervisor: &MockSupervisor) {
    let (stream, _) = supervisor.listener.accept().expect("accept");
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => write_json(writer, &attach_response(id, "history session")),
            "prompt" => {
                write_json(
                    writer,
                    &json!({ "type": "response", "id": id, "command": "prompt", "success": true }),
                );
                for event_type in ["turn_start", "turn_end", "agent_end"] {
                    write_session_event(writer, &json!({ "type": event_type }));
                }
            }
            _ => return false,
        }
        true
    });
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn ctrl_r_finds_a_past_prompt_and_enter_puts_it_in_the_composer() {
    let mut steps = Vec::new();
    for prompt in ["fix the parser tests", "explain the tokenizer"] {
        steps.extend([
            HeadlessStep::Type(prompt.to_string()),
            key(KeyCode::Enter, KeyModifiers::NONE),
            HeadlessStep::WaitGone {
                needle: prompt.to_string(),
                timeout_ms: 3000,
            },
        ]);
    }
    steps.extend([
        key(KeyCode::Char('r'), KeyModifiers::CONTROL),
        HeadlessStep::WaitRender {
            needle: "Search prompt history".to_string(),
            timeout_ms: 3000,
        },
        HeadlessStep::Type("parser".to_string()),
        HeadlessStep::WaitGone {
            needle: "explain the tokenizer".to_string(),
            timeout_ms: 3000,
        },
        key(KeyCode::Enter, KeyModifiers::NONE),
        HeadlessStep::WaitGone {
            needle: "Search prompt history".to_string(),
            timeout_ms: 3000,
        },
    ]);
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || serve(&supervisor)
        },
    );
    let all = outcome.frames.join("\n");
    assert!(!all.contains("timed out waiting"), "{all}");
    let last = outcome.frames.last().expect("a frame painted");
    assert!(
        last.lines().any(|line| line
            .split_whitespace()
            .eq("> fix the parser tests".split_whitespace())),
        "the picked prompt is the composer's draft:\n{last}"
    );
}
