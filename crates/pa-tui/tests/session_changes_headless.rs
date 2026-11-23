//! Headless e2e for the session changes panel: `/diff` and `app.changes.open` reach it through
//! the session's dispatch, a rebinding moves the open key's handler and its `/hotkeys` row, and
//! the attach key leaves the file's diff in the composer as the editor's paste marker.
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

const TITLE: &str = "Changes \u{b7} this turn";

/// One turn whose cell added twelve rows to `src/host.rs` (the session's directory is `/tmp`).
fn attach_data(id: &str) -> Value {
    let diff: String = std::iter::once(" 1 fn roster() {\n".to_string())
        .chain((2..=13).map(|line| format!("+{line}     step({line});\n")))
        .collect();
    let mut response = attach_response(id, "changes session");
    response["data"]["snapshot"]["messages"] = json!([
        { "role": "user", "content": "read the roster in host.rs" },
        {
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "c1",
                "name": "eval",
                "arguments": { "code": "(edit \"src/host.rs\" \"a\" \"b\")" },
            }],
            "timestamp": 1,
        },
        {
            "role": "toolResult",
            "toolCallId": "c1",
            "toolName": "eval",
            "content": [{ "type": "text", "text": "ok" }],
            "details": {
                "status": "ok",
                "diffs": [{ "path": "/tmp/src/host.rs", "diff": diff }],
            },
            "isError": false,
            "timestamp": 2,
        },
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

/// Run `steps` after the transcript painted and return the last frame.
fn last_frame(keybindings: KeybindingsManager, steps: Vec<HeadlessStep>) -> String {
    let mut plan = vec![HeadlessStep::WaitRender {
        needle: "+12".to_string(),
        timeout_ms: 10_000,
    }];
    plan.extend(steps);
    let outcome = run_headless(
        HeadlessPlan {
            steps: plan,
            width: 80,
            height: 24,
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
    outcome.frames.last().expect("a frame painted").clone()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(code, modifiers))
}

/// `/diff` opens the panel on the turn's file, and the attach key closes it with the diff in
/// the composer: seventeen lines (the file and counts, the fence, the cell's thirteen rows),
/// collapsed into the editor's paste marker.
#[test]
fn the_attach_key_leaves_the_files_diff_in_the_composer() {
    let frame = last_frame(
        KeybindingsManager::new(),
        vec![
            HeadlessStep::Submit("/diff".to_string()),
            HeadlessStep::WaitRender {
                needle: "src/host.rs  +12 -0  1 edit  no test after".to_string(),
                timeout_ms: 10_000,
            },
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            HeadlessStep::WaitRender {
                needle: "[paste #1 +17 lines]".to_string(),
                timeout_ms: 10_000,
            },
        ],
    );
    assert_eq!(
        (
            frame.contains(TITLE),
            frame.contains("[paste #1 +17 lines]")
        ),
        (false, true),
        "{frame}"
    );
}

/// The default key opens the panel; once rebound, the old key no longer does, the new one
/// does, and the `/hotkeys` guide names the new key.
#[test]
fn a_rebound_open_key_moves_its_handler_and_its_guide_row() {
    let rebound = || {
        let mut bindings = KeybindingsConfig::new();
        bindings.insert(
            "app.changes.open".to_string(),
            vec!["ctrl+alt+d".to_string()],
        );
        KeybindingsManager::with_user_bindings(bindings)
    };
    let alt_g = || key(KeyCode::Char('g'), KeyModifiers::ALT);
    let opened = HeadlessStep::WaitRender {
        needle: TITLE.to_string(),
        timeout_ms: 10_000,
    };
    let row = |key: &str| {
        format!(
            "| `{}` | Open the files changed this turn",
            format_key_text(key)
        )
    };
    let frames = [
        last_frame(KeybindingsManager::new(), vec![alt_g(), opened.clone()]),
        // The probe word reaches the composer only when the old key opened nothing over it.
        last_frame(
            rebound(),
            vec![
                alt_g(),
                HeadlessStep::Type("probe".to_string()),
                HeadlessStep::WaitRender {
                    needle: "probe".to_string(),
                    timeout_ms: 10_000,
                },
            ],
        ),
        last_frame(
            rebound(),
            vec![
                key(
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ),
                opened,
            ],
        ),
    ];
    let guide = pa_tui::hotkeys::hotkeys_guide(&rebound());
    assert_eq!(
        (
            frames.map(|frame| frame.contains(TITLE)),
            guide.contains(&row("ctrl+alt+d")),
            guide.contains(&row("alt+g")),
        ),
        ([true, false, true], true, false)
    );
}
