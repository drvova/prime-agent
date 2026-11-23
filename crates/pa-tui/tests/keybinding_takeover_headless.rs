//! Headless e2e for a user binding that takes another action's default key: the run's start shows
//! one quiet row naming it, and `/hotkeys` leads with the line saying what the key did before.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use pa_tui::keybindings::{KeybindingsConfig, KeybindingsManager};

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
            "attach" => write_json(writer, &attach_response(id, "takeover session")),
            _ => return false,
        }
        true
    });
}

/// The rows of `frame` holding any of `needles`, trimmed of the frame's padding.
fn rows_with(frame: &str, needles: &[&str]) -> Vec<String> {
    frame
        .lines()
        .filter(|row| needles.iter().any(|needle| row.contains(needle)))
        .map(|row| row.trim().to_string())
        .collect()
}

/// `app.session.resume` bound to F2 takes `app.model.cycleRecent`'s default: the start names it
/// in one row, and `/hotkeys` names the key, the user's action, and the action left unbound.
#[test]
fn a_taken_default_key_is_named_at_start_and_in_hotkeys() {
    let mut bindings = KeybindingsConfig::new();
    bindings.insert("app.session.resume".to_string(), vec!["f2".to_string()]);
    let notice = "Your keybindings.json takes a default key";
    let outcome = run_headless(
        HeadlessPlan {
            steps: vec![
                HeadlessStep::WaitRender {
                    needle: notice.to_string(),
                    timeout_ms: 10_000,
                },
                HeadlessStep::Submit("/hotkeys".to_string()),
                HeadlessStep::WaitRender {
                    needle: "Default keys your keybindings.json took".to_string(),
                    timeout_ms: 10_000,
                },
            ],
            width: 80,
            height: 30,
        },
        |socket| InteractiveOptions {
            keybindings: KeybindingsManager::with_user_bindings(bindings),
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || serve(&supervisor)
        },
    );
    let started = outcome
        .frames
        .iter()
        .find(|frame| frame.contains(notice))
        .expect("the notice painted");
    let guide = outcome.frames.last().expect("a frame painted");
    assert_eq!(
        (
            rows_with(started, &[notice, "Switch to the next"]),
            rows_with(guide, &["Default keys your", "- F2 \u{2192}"]),
        ),
        (
            vec![
                "Your keybindings.json takes a default key: F2 \u{2192} Resume a session (yours);"
                    .to_string(),
                "Switch to the next recently used model has no key".to_string(),
            ],
            vec![
                "Default keys your keybindings.json took".to_string(),
                "- F2 \u{2192} Resume a session (yours); Switch to the next recently used model has no"
                    .to_string(),
            ]
        )
    );
}
