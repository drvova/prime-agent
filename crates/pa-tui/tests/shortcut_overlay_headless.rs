//! Headless e2e for the `?` shortcut overview: on an empty prompt `?` opens it over the editor
//! dock, a printable key closes it and types into the prompt (so a message can start with `?`),
//! and a `?` typed into a draft is text.
#![cfg(unix)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::Value;

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, MockSupervisor, Request,
};

fn serve(supervisor: &MockSupervisor) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                let response: Value = attach_response(id, "overview session");
                write_json(writer, &response);
            }
            _ => return false,
        }
        true
    });
}

fn key(c: char) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT))
}

/// Run `steps` after the first frame and return every frame captured.
fn frames_after(steps: Vec<HeadlessStep>) -> Vec<String> {
    let mut plan = vec![HeadlessStep::WaitRender {
        needle: "overview session".to_string(),
        timeout_ms: 10_000,
    }];
    plan.extend(steps);
    run_headless(
        HeadlessPlan {
            steps: plan,
            width: 80,
            height: 40,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || serve(&supervisor)
        },
    )
    .frames
}

/// The prompt row of a frame: the composer's `>` row, trailing padding trimmed.
fn prompt_row(frame: &str) -> String {
    frame
        .lines()
        .rfind(|row| row.trim_start().starts_with('>'))
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// `?` on the empty prompt opens the overview; the next `?` closes it and types itself, so the
/// prompt reads `?`.
#[test]
fn question_mark_opens_the_overview_and_a_second_one_types_it() {
    let frames = frames_after(vec![
        key('?'),
        HeadlessStep::WaitRender {
            needle: "Shortcuts".to_string(),
            timeout_ms: 10_000,
        },
        key('?'),
        HeadlessStep::WaitGone {
            needle: "Shortcuts".to_string(),
            timeout_ms: 10_000,
        },
    ]);
    let opened = frames
        .iter()
        .find(|frame| frame.contains("  Shortcuts"))
        .expect("the overview opened");
    assert!(
        opened.contains("/hotkeys lists every key"),
        "the overview points at the full list: {opened}"
    );
    assert_eq!(
        prompt_row(frames.last().expect("a frame after the close")),
        " >  ?"
    );
}

/// A `?` typed into a draft is text: the overview never opens over it.
#[test]
fn a_question_mark_in_a_draft_is_text() {
    let frames = frames_after(vec![
        HeadlessStep::Type("why".to_string()),
        key('?'),
        HeadlessStep::WaitRender {
            needle: "why?".to_string(),
            timeout_ms: 10_000,
        },
    ]);
    assert!(
        frames.iter().all(|frame| !frame.contains("  Shortcuts")),
        "the overview stayed closed"
    );
    assert_eq!(prompt_row(frames.last().expect("a frame")), " >  why?");
}
