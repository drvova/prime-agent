//! Headless e2e for the prompt bar's up/down input-history recall (TS
//! `packages/tui/src/components/editor.ts`'s `navigateHistory` behind
//! `tui.editor.cursorUp`/`cursorDown`): Up walks the recalled prompts
//! backward, Down walks them forward, and one Down past the newest returns
//! to the draft. The compact dock coexists with the recall: the draft's
//! own Down enters the dock in every session shape, and the Up that
//! follows returns to the prompt before the next Up recalls.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    attach_response, create_response, options, run_headless, serve_connection, write_json,
    write_session_event, MockSupervisor, Request,
};

/// The prompt whose turn the mock keeps running until the client aborts it.
const HOLD_PROMPT: &str = "hold the turn";

impl MockSupervisor {
    /// Serve one connection: attach an empty session, list one heartbeat (selectable while no
    /// subagents exist — the session shape that must not steal the prompt's Down).
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(stream, &[], |writer, Request { id, kind, command }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &attach_response(id, "recall session"));
                }
                "heartbeats_list" => {
                    // One in-scope heartbeat row: the dock renders its heartbeats group, and that
                    // group is selectable.
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "heartbeats_list",
                            "success": true,
                            "data": {
                                "heartbeats": [
                                    {
                                        "job": {
                                            "id": "hb-1",
                                            "status": "active",
                                            "source": "heartbeat",
                                            "activeSessionId": "s1",
                                            "sessionId": "sess-1",
                                            "label": "dock recall beat",
                                            "schedule": {
                                                "kind": "interval",
                                                "expression": "every 30m",
                                            },
                                        },
                                    },
                                ],
                            },
                        }),
                    );
                }
                "prompt" => {
                    // An admitted prompt: the ack, then the turn's bookends (`turn_start` +
                    // `turn_end`, no message rows — the transcript keeps nothing of the prompt).
                    // The client marks the turn active on the ack and only `turn_end` clears it;
                    // the harness's idle gate requires the turn settled. The hold prompt's turn
                    // keeps running until an abort.
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                    write_session_event(writer, &json!({ "type": "turn_start" }));
                    if command.get("message").and_then(Value::as_str) != Some(HOLD_PROMPT) {
                        write_session_event(writer, &json!({ "type": "turn_end" }));
                        write_session_event(writer, &json!({ "type": "agent_end" }));
                    }
                }
                "abort" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "abort",
                            "success": true,
                        }),
                    );
                    write_session_event(writer, &json!({ "type": "turn_end" }));
                    write_session_event(writer, &json!({ "type": "agent_end" }));
                }
                _ => return false,
            }
            true
        });
    }
}

/// Run a headless plan; the captured frames show the editor surface (the prompt box) and the dock.
fn run_plan(steps: Vec<HeadlessStep>) -> Vec<String> {
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve()
        },
    );
    outcome.frames
}

/// Every step's condition wait must have been satisfied: a barrier that timed out renders its note
/// naming the broken step.
fn assert_no_barrier_timeouts(frames: &[String]) {
    let all = frames.join("\n");
    assert!(
        !all.contains("timed out waiting"),
        "a plan barrier timed out (the step's condition never rendered):\n{all}"
    );
}

/// Submit one prompt through the typed path (`HeadlessStep::Submit` bypasses the editor and would
/// leave the history empty), then hold until the prompt box renders empty again.
fn submit(prompt: &str) -> Vec<HeadlessStep> {
    vec![
        HeadlessStep::Type(prompt.to_string()),
        HeadlessStep::Key(enter()),
        HeadlessStep::WaitGone {
            needle: prompt.to_string(),
            timeout_ms: 3000,
        },
    ]
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn up() -> KeyEvent {
    KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn down() -> KeyEvent {
    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
}

/// Alt+A: the dock's own focus shortcut (`app.subagents.focus`).
fn alt_a() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)
}

fn escape() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn wait_render(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 3000,
    }
}

fn wait_gone(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitGone {
        needle: needle.to_string(),
        timeout_ms: 3000,
    }
}

#[test]
fn up_walks_backward_down_forward_and_the_draft_returns() {
    let mut steps = Vec::new();
    steps.extend(submit("first prompt"));
    steps.extend(submit("second prompt"));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_render("second prompt"));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_gone("second prompt"));
    steps.push(wait_render("first prompt"));
    steps.push(HeadlessStep::Key(down()));
    steps.push(wait_gone("first prompt"));
    steps.push(wait_render("second prompt"));
    steps.push(HeadlessStep::Key(down()));
    steps.push(wait_gone("second prompt"));
    let frames = run_plan(steps);
    assert_no_barrier_timeouts(&frames);
    let last = frames.last().expect("the plan rendered frames");
    assert!(
        !last.contains("first prompt") && !last.contains("second prompt"),
        "the final Down returned to the empty draft, not a recalled prompt: {last:?}"
    );
}

/// Ctrl+C on a draft clears it without arming the exit hint, and the first Up on the empty prompt
/// recalls that exact draft ahead of the submitted prompts; the next Up walks on into the history.
#[test]
fn ctrl_c_clears_the_draft_and_up_recalls_it_before_the_history() {
    let mut steps = Vec::new();
    steps.extend(submit("older prompt"));
    steps.push(HeadlessStep::Type("my unsent draft".to_string()));
    steps.push(wait_render("my unsent draft"));
    steps.push(HeadlessStep::Key(ctrl_c()));
    steps.push(wait_gone("my unsent draft"));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_render("my unsent draft"));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_gone("my unsent draft"));
    steps.push(wait_render("older prompt"));
    let frames = run_plan(steps);
    assert_no_barrier_timeouts(&frames);
    assert!(
        frames.iter().all(|frame| !frame.contains("again to exit")),
        "clearing a draft never arms the exit hint: {frames:?}"
    );
}

/// While a turn runs, Ctrl+C on a draft only interrupts: the draft stays in the composer and the
/// exit hint arms, as before drafts were recallable.
#[test]
fn ctrl_c_during_a_running_turn_interrupts_and_keeps_the_draft() {
    let steps = vec![
        HeadlessStep::Type(HOLD_PROMPT.to_string()),
        HeadlessStep::Key(enter()),
        wait_gone(HOLD_PROMPT),
        HeadlessStep::Type("my unsent draft".to_string()),
        wait_render("my unsent draft"),
        HeadlessStep::Key(ctrl_c()),
        wait_render("again to exit"),
    ];
    let frames = run_plan(steps);
    assert_no_barrier_timeouts(&frames);
    let hinted = frames
        .iter()
        .find(|frame| frame.contains("again to exit"))
        .expect("the interrupt arms the exit hint");
    assert!(
        hinted.contains("my unsent draft"),
        "the interrupt keeps the draft:\n{hinted}"
    );
}

/// The draft's Down enters the dock in every session shape (the
/// operator's 2026-10-01 consistency ruling — here one heartbeat and no
/// subagents), and the recall stays one press away: the Up after that
/// Down returns the focus to the prompt, and the next Up recalls.
#[test]
fn the_drafts_down_enters_the_dock_and_up_returns_before_the_recall() {
    let mut steps = Vec::new();
    // The dock's heartbeats group is mounted before the walk starts: the
    // frame carries its `◷ 1 heartbeat` row.
    steps.push(wait_render("heartbeat"));
    steps.extend(submit("first prompt"));
    steps.extend(submit("second prompt"));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_render("second prompt"));
    // The overshoot: Down back to the draft, then the draft's own Down —
    // the press that enters the dock.
    steps.push(HeadlessStep::Key(down()));
    steps.push(wait_gone("second prompt"));
    steps.push(HeadlessStep::Key(down()));
    // The first Up only leaves the dock, so the second Up recalls the
    // NEWEST prompt (two recalls would have walked on to the older one).
    steps.push(HeadlessStep::Key(up()));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_render("second prompt"));
    steps.push(HeadlessStep::WaitMs(100));
    let frames = run_plan(steps);
    assert_no_barrier_timeouts(&frames);
    let last = frames.last().expect("the plan rendered frames");
    assert!(
        last.contains("second prompt") && !last.contains("first prompt"),
        "the first Up left the dock and the second recalled the newest prompt: {last:?}"
    );
}

/// Coexistence: Down walks the expanded dock to the heartbeats row, and Escape closes the panel
/// onto the dock's own item (the 2026-09-26 panel-exit ruling): the history recall works right
/// after the round trip.
#[test]
fn alt_a_arrows_and_enter_still_open_the_dock_group_view_and_recall_survives_it() {
    let mut steps = Vec::new();
    steps.push(wait_render("heartbeat"));
    steps.extend(submit("first prompt"));
    steps.push(HeadlessStep::Key(alt_a()));
    // The dock's focus starts on the subagents row; one Down steps to heartbeats.
    steps.push(HeadlessStep::Key(down()));
    steps.push(HeadlessStep::Key(enter()));
    steps.push(wait_render("Heartbeats"));
    steps.push(HeadlessStep::Key(escape()));
    steps.push(wait_gone("Heartbeats"));
    // The panel's exit hands the keyboard focus to the dock's own Heartbeates item (the panel-exit
    // ruling): the cancel Escape returns the editor, and only then the Up recalls the prompt.
    steps.push(HeadlessStep::Key(escape()));
    steps.push(HeadlessStep::Key(up()));
    steps.push(wait_render("first prompt"));
    let frames = run_plan(steps);
    assert_no_barrier_timeouts(&frames);
    let last = frames.last().expect("the plan rendered frames");
    assert!(
        last.contains("first prompt"),
        "the recall works right after the dock round trip: {last:?}"
    );
}
