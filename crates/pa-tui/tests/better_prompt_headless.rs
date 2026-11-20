//! Headless e2e for `/better <prompt>`: a mock supervisor plays the daemon's helper on the
//! side-run lane (`start_side_question` with `kind: "better"`, then `side_question_event`
//! frames). The rewrite lands in the composer above the draft typed meanwhile and nothing is
//! sent; Esc cancels a run and says nothing was sent; a helper that never answers leaves the
//! composer painting what the user types.
#![cfg(unix)]

mod support;

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, MockSupervisor, Request,
};

const REWRITE: &str = "Fix `parse_expr` in `src/parser.rs`.\nProve it with `cargo test -p parser`.";

/// How the mock helper answers a run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// One progress event, then the rewrite once the draft had time to land.
    Rewrites,
    /// Progress, then nothing until `abort_side_question` (answered with `cancelled`).
    WaitsForAbort,
    /// Acknowledges the run and never sends another frame.
    Hangs,
}

fn side_event(id: &str, status: &str, answer: &str) -> Value {
    json!({
        "type": "side_question_event",
        "activeSessionId": "s1",
        "event": { "id": id, "question": "fix the parser", "answer": answer, "status": status },
    })
}

/// Serve one connection; every command the client sends is recorded.
fn serve(supervisor: &MockSupervisor, helper: Helper, seen: &Arc<Mutex<Vec<Value>>>) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, command }| {
        seen.lock().unwrap().push(command.clone());
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => write_json(writer, &attach_response(id, "better")),
            "start_side_question" => {
                write_json(
                    writer,
                    &json!({ "type": "response", "id": id, "command": kind, "success": true }),
                );
                let run = command["sideQuestionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let mut events = writer.try_clone().expect("clone event socket");
                if helper == Helper::Hangs {
                    return true;
                }
                write_json(
                    &mut events,
                    &side_event(&run, "running", "fs.read src/parser.rs"),
                );
                if helper == Helper::Rewrites {
                    std::thread::spawn(move || {
                        // The draft is typed well inside this window (the plan waits on the
                        // rendered notice, never on this delay).
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        write_json(&mut events, &side_event(&run, "complete", REWRITE));
                    });
                }
            }
            "abort_side_question" => {
                write_json(
                    writer,
                    &json!({ "type": "response", "id": id, "command": kind, "success": true,
                             "data": { "aborted": true } }),
                );
                let run = command["sideQuestionId"].as_str().unwrap_or_default();
                write_json(writer, &side_event(run, "cancelled", ""));
            }
            _ => return false,
        }
        true
    });
}

fn run(helper: Helper, steps: Vec<HeadlessStep>) -> (Vec<String>, Vec<Value>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 30,
    };
    let recorded = Arc::clone(&seen);
    let outcome = run_headless(plan, options, move |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || serve(&supervisor, helper, &recorded)
    });
    let commands = seen.lock().unwrap().clone();
    (outcome.frames, commands)
}

fn kinds(commands: &[Value]) -> Vec<&str> {
    commands
        .iter()
        .filter_map(|command| command["type"].as_str())
        .collect()
}

fn wait(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 10_000,
    }
}

/// The composer's text rows in a frame: the lines between the last two horizontal rules.
fn composer_rows(frame: &str) -> Vec<String> {
    let lines: Vec<&str> = frame.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("\u{2500}\u{2500}\u{2500}"))
        .map(|(index, _)| index)
        .collect();
    let [.., top, bottom] = rules[..] else {
        panic!("no composer rules in the frame:\n{frame}");
    };
    lines[top + 1..bottom]
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[test]
fn the_rewrite_lands_above_the_draft_and_nothing_is_sent() {
    let (frames, commands) = run(
        Helper::Rewrites,
        vec![
            HeadlessStep::Submit("/better fix the parser".to_string()),
            wait("Improving your prompt: fs.read src/parser.rs"),
            HeadlessStep::Type("my draft".to_string()),
            wait("Improved prompt is in your input box"),
        ],
    );
    let last = frames.last().expect("a frame painted");
    assert_eq!(
        composer_rows(last),
        vec![
            " Fix `parse_expr` in `src/parser.rs`.".to_string(),
            " Prove it with `cargo test -p parser`.".to_string(),
            String::new(),
            " my draft".to_string(),
        ],
        "{last}"
    );
    assert!(
        last.contains("Improved prompt is in your input box; edit it, then press Enter to send."),
        "{last}"
    );
    let start = commands
        .iter()
        .find(|command| command["type"] == "start_side_question")
        .expect("the run started");
    assert_eq!(
        (start["kind"].clone(), start["question"].clone()),
        (json!("better"), json!("fix the parser"))
    );
    // Nothing reached the model: no prompt, steer, or follow-up went out.
    let sent: Vec<&str> = kinds(&commands)
        .into_iter()
        .filter(|kind| matches!(*kind, "prompt" | "prompt_and_wait" | "steer" | "follow_up"))
        .collect();
    assert_eq!(sent, Vec::<&str>::new());
}

#[test]
fn esc_cancels_the_run_and_says_nothing_was_sent() {
    let (frames, commands) = run(
        Helper::WaitsForAbort,
        vec![
            HeadlessStep::Submit("/better fix the parser".to_string()),
            wait("Improving your prompt: fs.read src/parser.rs"),
            HeadlessStep::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            wait("/better cancelled; nothing was sent."),
        ],
    );
    let last = frames.last().expect("a frame painted");
    assert!(!last.contains("Improved prompt"), "{last}");
    let run_id = commands
        .iter()
        .find(|command| command["type"] == "start_side_question")
        .map(|command| command["sideQuestionId"].clone())
        .expect("the run started");
    let aborts: Vec<(&str, Value)> = commands
        .iter()
        .filter(|command| {
            matches!(
                command["type"].as_str(),
                Some("abort_side_question" | "abort")
            )
        })
        .map(|command| {
            (
                command["type"].as_str().unwrap_or_default(),
                command["sideQuestionId"].clone(),
            )
        })
        .collect();
    // The first Esc belongs to the run: it aborts the run, never the agent.
    assert_eq!(aborts, vec![("abort_side_question", run_id)]);
    assert!(!kinds(&commands).contains(&"prompt"));
}

#[test]
fn a_hanging_helper_never_holds_the_composer() {
    let (frames, commands) = run(
        Helper::Hangs,
        vec![
            HeadlessStep::Submit("/better fix the parser".to_string()),
            HeadlessStep::Type("still typing".to_string()),
            wait("still typing"),
        ],
    );
    let last = frames.last().expect("a frame painted");
    assert!(
        last.contains("Improving your prompt: reading the code"),
        "{last}"
    );
    assert_eq!(
        composer_rows(last),
        vec![" still typing".to_string()],
        "{last}"
    );
    assert!(!kinds(&commands).contains(&"prompt"));
}
