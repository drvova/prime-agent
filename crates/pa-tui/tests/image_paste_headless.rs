//! Headless e2e for the image-paste paths (the user's spec: image paste is Ctrl+V): an empty or
//! whitespace-only bracketed paste — what a terminal sends when it intercepts Ctrl+V over an
//! image-only clipboard — attaches the clipboard image, and so does the Ctrl+V key that reaches
//! the app. Neither submits. The `PRIME_AGENT_TEST_CLIPBOARD_IMAGE` seam stands in for the
//! clipboard, so the paste path runs exactly as it does over a real one.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};

use support::{
    attach_response, create_response, options, run_headless, serve_connection, write_json,
    MockSupervisor, Request,
};

/// A 1x1 opaque PNG.
const ONE_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x70, 0x6b, 0xda, 0x02,
    0x00, 0x02, 0x8e, 0x01, 0x7d, 0xc8, 0x73, 0x8d, 0x1c, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn an_empty_paste_and_ctrl_v_attach_the_clipboard_image() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let image = dir.path().join("clipboard.png");
    std::fs::write(&image, ONE_PIXEL_PNG).expect("write the clipboard image");
    std::env::set_var("PRIME_AGENT_TEST_CLIPBOARD_IMAGE", &image);
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = prompts.clone();
    let wait = |needle: &str| HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 10_000,
    };
    let outcome = run_headless(
        HeadlessPlan {
            steps: vec![
                HeadlessStep::Paste(String::new()),
                wait("[image #1]"),
                HeadlessStep::Paste(" \r\n".to_string()),
                wait("[image #2]"),
                HeadlessStep::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
                wait("[image #3]"),
                HeadlessStep::Paste(" tail".to_string()),
                wait("[image #3] tail"),
            ],
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || {
                let (stream, _) = supervisor.listener.accept().expect("accept");
                serve_connection(stream, &[], |writer, Request { id, kind, command }| {
                    match kind {
                        "create" => write_json(writer, &create_response(id)),
                        "attach" => write_json(writer, &attach_response(id, "paste session")),
                        "prompt" => {
                            seen.lock().expect("prompts lock").push(command.to_string());
                            return false;
                        }
                        _ => return false,
                    }
                    true
                });
            }
        },
    );
    let last = outcome.frames.last().cloned().unwrap_or_default();
    let composer = last
        .lines()
        .find(|line| line.contains("[image #"))
        .map(str::trim)
        .map(str::to_string);
    let all = outcome.frames.join("\n");
    assert_eq!(
        (
            composer,
            all.contains("timed out waiting"),
            prompts.lock().expect("prompts lock").clone(),
        ),
        (
            Some(">  [image #1][image #2][image #3] tail".to_string()),
            false,
            Vec::<String>::new(),
        ),
        "{last}"
    );
}
