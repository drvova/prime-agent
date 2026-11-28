#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "the headless `run_interactive` futures stay on the stack; each test awaits one at a time"
    )
)]
#![cfg(unix)]

//! E2E verifier for the dock after a session stop: a chat attached to a live session whose kernel
//! runs a shell drops the shell from its dock when the daemon stops the session, instead of
//! keeping the dead kernel's last list.

mod support;

use support::runtime_daemon::{RuntimeDaemon, Wire2};

use std::path::{Path, PathBuf};
use std::time::Duration;

use pa_tui::interactive::{
    HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection, SessionSelection, UiMode,
};
use serde_json::json;

/// The faux script of a session whose kernel runs `cell` on its second turn (the first turn
/// persists the session), written under `dir`; returns its path.
fn running_script(dir: &Path, cell: &str) -> PathBuf {
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [
            { "text": "ready" },
            { "content": [{ "type": "toolCall", "name": "eval",
                            "arguments": { "code": cell } }] },
            { "text": "started" },
        ] })
        .to_string(),
    )
    .expect("write faux script");
    script
}

/// A live session that ran its script's two turns, its kernel's shell left running; returns its
/// id.
fn session_running(client: &mut Wire2, dir: &Path) -> String {
    let created = client
        .call(
            "session.create",
            &json!({
                "cwd": dir.to_string_lossy(),
                "sessionDir": dir.join("agent").join("sessions").to_string_lossy(),
            }),
        )
        .expect("the create");
    let active = created["session"]
        .as_str()
        .expect("the created session is live")
        .to_string();
    for message in ["warm up", "start the shell"] {
        client.run_turn(&active, message);
    }
    let listed = client
        .query("session.kernel_bash", &json!({ "session": active }))
        .expect("the kernel's shell commands");
    assert_eq!(
        listed["activities"][0]["status"], "running",
        "the kernel runs the shell: {listed}"
    );
    active
}

fn chat_options(socket: PathBuf, cwd: PathBuf, active: String) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd,
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::Attach(active),
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

fn dock_line(frame: &str) -> &str {
    frame
        .lines()
        .find(|line| line.contains("subagents") && line.contains("heartbeats"))
        .unwrap_or_default()
}

#[tokio::test]
async fn stopping_the_attached_session_clears_its_shells_from_the_dock() {
    // Under the canonical temp root: the session lease keys on canonical paths,
    // and macOS temp dirs sit behind the /var -> /private/var symlink.
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let script = running_script(
        dir.path(),
        "(define job (bash \"sleep 300\" (hash 'background #t)))",
    );
    let daemon = RuntimeDaemon::spawn_with(dir.path(), |command| {
        command.env("PRIME_AGENT_FAUX_SCRIPT", &script);
    });
    let mut client = daemon.client();
    let active = session_running(&mut client, dir.path());

    let (rendered_tx, rendered_rx) = std::sync::mpsc::channel();
    let kill_thread = {
        let active = active.clone();
        std::thread::spawn(move || {
            rendered_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the chat rendered the shell");
            client
                .call("session.stop", &json!({ "session": active }))
                .expect("the stop");
        })
    };
    let outcome = pa_tui::interactive::run_interactive(
        chat_options(daemon.socket.clone(), dir.path().to_path_buf(), active),
        UiMode::Headless(HeadlessPlan {
            steps: vec![
                HeadlessStep::WaitRender {
                    needle: "\u{25b8} 1 shell".to_string(),
                    timeout_ms: 20_000,
                },
                HeadlessStep::Timestamp(rendered_tx),
                HeadlessStep::WaitRender {
                    needle: "stopped this agent session".to_string(),
                    timeout_ms: 20_000,
                },
                HeadlessStep::WaitRender {
                    needle: "\u{25b8} 0 shells".to_string(),
                    timeout_ms: 2_500,
                },
            ],
            width: 160,
            height: 30,
        }),
    )
    .await
    .expect("the chat run");
    kill_thread.join().expect("the stop ran");

    let frames = &outcome.frames;
    let stop_frame = frames
        .iter()
        .position(|frame| frame.contains("stopped this agent session"))
        .unwrap_or_else(|| panic!("the stop reached the chat:\n{}", frames.join("\n")));
    assert!(
        frames[..stop_frame]
            .iter()
            .any(|frame| dock_line(frame).contains("\u{25b8} 1 shell")),
        "the dock counted the live shell before the stop:\n{}",
        frames.join("\n")
    );
    let last = frames.last().expect("frames");
    assert!(
        dock_line(last).contains("\u{25b8} 0 shells"),
        "the dock dropped the stopped session's shell; last dock: {:?}\n{last}",
        dock_line(last)
    );
}
