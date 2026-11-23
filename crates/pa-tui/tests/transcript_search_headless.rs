//! Headless e2e for transcript search: `app.transcript.search` opens the bar through the
//! session's key dispatch, typing and the step keys move between matches, and a user's
//! rebinding moves the handlers and the bar's hint together.
#![cfg(unix)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use pa_tui::keybindings::{format_key_text, KeybindingsConfig, KeybindingsManager};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, MockSupervisor, Request,
};

/// Two turns, each reply taller than the transcript window.
fn attach_data(id: &str) -> Value {
    let reply = |word: &str| {
        let steps: Vec<String> = (1..=12).map(|step| format!("{word} step {step}")).collect();
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": steps.join("\n\n") }],
            "timestamp": 1,
        })
    };
    let prompt = |text: &str| json!({ "role": "user", "content": text });
    let mut response = attach_response(id, "search session");
    response["data"]["snapshot"]["messages"] = json!([
        prompt("first question"),
        reply("alpha"),
        prompt("second question"),
        reply("beta"),
    ]);
    response
}

fn serve(supervisor: &MockSupervisor) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => write_json(writer, &attach_data(id)),
            _ => return false,
        }
        true
    });
}

fn press(code: KeyCode, modifiers: KeyModifiers) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(code, modifiers))
}

fn typed(text: &str) -> Vec<HeadlessStep> {
    text.chars()
        .map(|c| press(KeyCode::Char(c), KeyModifiers::NONE))
        .collect()
}

fn wait_for(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 10_000,
    }
}

/// Run `steps` after the transcript painted and return the last frame's rows, trailing padding
/// trimmed.
fn last_frame(keybindings: KeybindingsManager, steps: Vec<HeadlessStep>) -> Vec<String> {
    let mut plan = vec![wait_for("beta step 12")];
    plan.extend(steps);
    let outcome = run_headless(
        HeadlessPlan {
            steps: plan,
            width: 80,
            height: 16,
        },
        |socket| InteractiveOptions {
            keybindings,
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || serve(&supervisor)
        },
    );
    let frame = outcome.frames.last().expect("a frame painted");
    frame
        .lines()
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn rows(rows: &[&str]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}

/// The top bar's row.
const TOP: &str = "                                 search session";

/// The default key opens the bar over the whole dock, the nearest match above the bottom is
/// current, and the previous-match key steps to the one above it, already in view.
#[test]
fn the_default_keys_open_the_bar_and_step_between_matches() {
    let mut steps = vec![press(KeyCode::Char('/'), KeyModifiers::ALT)];
    steps.extend(typed("beta step 1"));
    steps.extend([
        wait_for("4 of 4"),
        press(KeyCode::Up, KeyModifiers::NONE),
        wait_for("3 of 4"),
    ]);
    let bar = format!(
        " / beta step 1                             3 of 4 \u{b7} {} next \u{b7} {} prev \u{b7} Esc close",
        format_key_text("down"),
        format_key_text("up")
    );
    assert_eq!(
        last_frame(KeybindingsManager::new(), steps),
        rows(&[
            TOP,
            "",
            " beta step 6",
            "",
            " beta step 7",
            "",
            " beta step 8",
            "",
            " beta step 9",
            "",
            " beta step 10",
            "",
            " beta step 11",
            "",
            " beta step 12",
            &bar,
        ])
    );
}

/// A rebinding moves the handlers and the bar's hint together: the default key no longer opens
/// the bar (the next key types into the composer), the new one does, and the rebound next key
/// steps, wrapping to the first match, and names itself in the bar.
#[test]
fn a_rebound_search_key_moves_the_handler_and_its_hint() {
    let rebound = || {
        let mut bindings = KeybindingsConfig::new();
        bindings.insert(
            "app.transcript.search".to_string(),
            vec!["ctrl+g".to_string()],
        );
        bindings.insert(
            "app.transcript.searchNext".to_string(),
            vec!["ctrl+e".to_string()],
        );
        KeybindingsManager::with_user_bindings(bindings)
    };
    let default_key = last_frame(
        rebound(),
        vec![
            press(KeyCode::Char('/'), KeyModifiers::ALT),
            press(KeyCode::Char('z'), KeyModifiers::NONE),
            wait_for(">  z"),
        ],
    );
    let mut steps = vec![press(KeyCode::Char('g'), KeyModifiers::CONTROL)];
    steps.extend(typed("beta step 1"));
    steps.extend([
        wait_for("4 of 4"),
        press(KeyCode::Char('e'), KeyModifiers::CONTROL),
        wait_for("1 of 4"),
    ]);
    let bar = format!(
        " / beta step 1                        1 of 4 \u{b7} {} next \u{b7} {} prev \u{b7} Esc close",
        format_key_text("ctrl+e"),
        format_key_text("up")
    );
    let pill = format!(
        "                            \u{2193} {} to follow",
        format_key_text("ctrl+shift+down")
    );
    assert_eq!(
        (
            default_key.iter().any(|row| row.starts_with(" / ")),
            last_frame(rebound(), steps)
        ),
        (
            false,
            rows(&[
                TOP,
                "",
                "  second question",
                "",
                "",
                " beta step 1",
                "",
                " beta step 2",
                "",
                " beta step 3",
                "",
                " beta step 4",
                "",
                " beta step 5",
                &pill,
                &bar,
            ])
        )
    );
}
