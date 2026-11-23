//! Headless e2e for the prompt-jump keys: `tui.viewport.previousPrompt` and
//! `tui.viewport.nextPrompt` reach the transcript through the session's key dispatch, on their
//! default keys and on a user's rebinding, and the paused window pins the prompt it reads under.
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
    // No timestamp: the prompt row carries no clock, whose text follows the host's time zone.
    let prompt = |text: &str| json!({ "role": "user", "content": text });
    let mut response = attach_response(id, "jump session");
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

/// Press `keys` in order and return the transcript window of the last frame (the rows between
/// the top bar and the dock), trailing padding trimmed.
fn window_after(keybindings: KeybindingsManager, keys: &[(KeyCode, KeyModifiers)]) -> Vec<String> {
    let mut steps = vec![HeadlessStep::WaitRender {
        needle: "beta step 12".to_string(),
        timeout_ms: 10_000,
    }];
    steps.extend(
        keys.iter()
            .map(|(code, modifiers)| HeadlessStep::Key(KeyEvent::new(*code, *modifiers))),
    );
    let outcome = run_headless(
        HeadlessPlan {
            steps,
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
    outcome
        .frames
        .last()
        .expect("a frame painted")
        .lines()
        .skip(1)
        .take(5)
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The default keys walk the prompts: twice up lands on the first prompt, once down on the
/// second.
#[test]
fn the_default_keys_jump_between_prompts() {
    let up = (KeyCode::PageUp, KeyModifiers::ALT);
    let down = (KeyCode::PageDown, KeyModifiers::ALT);
    let windows = [
        window_after(KeybindingsManager::new(), &[up, up]),
        window_after(KeybindingsManager::new(), &[up, up, down]),
    ];
    assert_eq!(
        windows,
        [
            vec![
                String::new(),
                "  first question".to_string(),
                String::new(),
                String::new(),
                " alpha step 1".to_string(),
            ],
            vec![
                String::new(),
                String::new(),
                "  second question".to_string(),
                String::new(),
                String::new(),
            ],
        ]
    );
}

/// A rebinding moves the handler and the pinned row's hint together: the default key no longer
/// jumps, and the row names the new key.
#[test]
fn a_rebound_jump_key_moves_the_handler_and_its_hint() {
    let rebound = || {
        let mut bindings = KeybindingsConfig::new();
        bindings.insert(
            "tui.viewport.previousPrompt".to_string(),
            vec!["ctrl+alt+p".to_string()],
        );
        KeybindingsManager::with_user_bindings(bindings)
    };
    let page_up = (KeyCode::PageUp, KeyModifiers::NONE);
    let hints = format!(
        "{} prev \u{b7} {} next",
        format_key_text("ctrl+alt+p"),
        format_key_text("alt+pageDown")
    );
    let pinned = format!(
        "\u{276f} second question{}{hints}",
        " ".repeat(80 - 17 - pa_tui::width::str_width(&hints))
    );
    let windows = [
        // The old default does nothing; one page up shows the row and its hint.
        window_after(rebound(), &[(KeyCode::PageUp, KeyModifiers::ALT), page_up]),
        window_after(
            rebound(),
            &[
                page_up,
                (
                    KeyCode::Char('p'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ),
            ],
        ),
    ];
    assert_eq!(
        windows,
        [
            vec![
                pinned,
                String::new(),
                " beta step 7".to_string(),
                String::new(),
                " beta step 8".to_string(),
            ],
            vec![
                String::new(),
                String::new(),
                "  second question".to_string(),
                String::new(),
                String::new(),
            ],
        ]
    );
}
