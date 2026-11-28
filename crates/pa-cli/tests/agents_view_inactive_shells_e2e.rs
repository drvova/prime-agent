#![cfg_attr(
    target_os = "linux",
    expect(
        clippy::cast_possible_wrap,
        reason = "`Child::id()` is a pid the kernel bounds below i32::MAX"
    )
)]
#![cfg(target_os = "linux")]

//! E2E verifier for an inactive session's shells line: the open agents view follows the
//! daemon's live persistent shells, so a shell that ends (or starts) while the view stays open
//! moves its session's `$` line within a poll cycle instead of keeping the catalog's open-time
//! snapshot until a reopen.

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewOptions, AgentsViewUiMode};
use serde_json::json;

/// One saved session: a header plus a named user/assistant exchange.
fn write_session(sessions_dir: &Path, id: &str, name: &str) {
    let mut content = String::new();
    let _ = writeln!(
        content,
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2024-01-01T00:00:00.000Z","cwd":"/tmp"}}"#
    );
    let _ = writeln!(
        content,
        r#"{{"type":"session_info","id":"{id}-info","timestamp":"2024-01-01T00:00:00.000Z","name":"{name}"}}"#
    );
    let _ = writeln!(
        content,
        r#"{{"type":"message","id":"{id}-u","timestamp":"2024-01-01T00:00:00.000Z","message":{{"role":"user","content":"start a server","timestamp":0}}}}"#
    );
    let _ = writeln!(
        content,
        r#"{{"type":"message","id":"{id}-a","timestamp":"2024-01-01T00:00:01.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"started"}}],"timestamp":1}}}}"#
    );
    std::fs::write(sessions_dir.join(format!("{id}.jsonl")), content).expect("write session");
}

/// A persistent shell: a `sleep` leading its own process group, registered in the agent dir's
/// persistent-shell home under its real start identity and assigned to `session_id`.
struct Shell(Child);

impl Shell {
    fn start(agent_dir: &Path, session_id: &str) -> Shell {
        let mut command = Command::new("sleep");
        command.arg("300");
        pa_core::platform::process::set_new_session(&mut command);
        let child = command.spawn().expect("spawn shell");
        let pid = child.id();
        let home = agent_dir.join("persistent-shells");
        std::fs::create_dir_all(&home).expect("persistent-shell home");
        let id = format!("{pid:032}");
        let record = json!({
            "id": id,
            "pid": pid,
            "startId": pa_core::session::lease::get_process_start_id(pid).expect("start id"),
            "command": "sleep 300",
            "startedAt": "2026-01-01T00:00:00+00:00",
            "sessionId": session_id,
        });
        std::fs::write(home.join(format!("{id}.json")), record.to_string()).expect("write record");
        Shell(child)
    }

    /// End the shell's process group (what a user's kill does) and reap it.
    fn stop(mut self) {
        killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGTERM).expect("signal the group");
        self.0.wait().expect("reap the shell");
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn view_options(socket: &Path, session_dir: &Path) -> AgentsViewOptions {
    AgentsViewOptions {
        socket_path: socket.to_path_buf(),
        cwd: PathBuf::from("/tmp"),
        session_dir: Some(session_dir.to_path_buf()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: None,
        scope: None,
        folder_scope: None,
        query: None,
        expanded_ancestors: Vec::new(),
        selected_row_identity: None,
        selected_key: None,
        status_message: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        show_hardware_cursor: false,
        incident_notice_state: None,
        create_config: json!({}),
    }
}

/// The sessions' indented `$` lines: the unindented `Persistent shells` rows also start with `$`.
fn shell_lines(frame: &str) -> Vec<&str> {
    frame
        .lines()
        .filter(|line| line.starts_with(' '))
        .map(str::trim)
        .filter(|line| line.starts_with("$ "))
        .collect()
}

/// Session A's shell ends while the view is open, then session B (already listed, no shells at
/// the catalog load) starts two: B's line can only land from a registry read made after A's
/// shell was reaped, so the frame that shows it must also show A without a line.
#[tokio::test]
async fn an_inactive_sessions_shells_line_follows_its_live_persistent_shells() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("session dir");
    write_session(&sessions_dir, "session-a", "server session a");
    write_session(&sessions_dir, "session-b", "server session b");
    let shell_a = Shell::start(&agent_dir, "session-a");
    let daemon = RuntimeDaemon::spawn(dir.path());

    let line_rendered = std::sync::Arc::new(tokio::sync::Notify::new());
    let view = pa_tui::agents_view::run_agents_view(
        view_options(&daemon.socket, &sessions_dir),
        AgentsViewUiMode::Headless(AgentsHeadlessPlan {
            steps: vec![
                AgentsStep::WaitRender {
                    needle: "$ 1 shell \u{b7} 1 persistent".to_string(),
                    timeout_ms: 20_000,
                },
                AgentsStep::Notify(line_rendered.clone()),
                // Two poll cycles: the change lands with the first cycle after it.
                AgentsStep::WaitRender {
                    needle: "$ 2 shells \u{b7} 2 persistent".to_string(),
                    timeout_ms: 2 * 2_000 + 500,
                },
            ],
            width: 120,
            height: 36,
        }),
        None,
    );
    let shells_b = async {
        line_rendered.notified().await;
        shell_a.stop();
        [
            Shell::start(&agent_dir, "session-b"),
            Shell::start(&agent_dir, "session-b"),
        ]
    };
    let (view, _shells_b) = tokio::join!(view, shells_b);
    let frames = view.expect("agents view run").outcome.frames;

    let before = frames
        .iter()
        .position(|frame| shell_lines(frame) == ["$ 1 shell \u{b7} 1 persistent"])
        .unwrap_or_else(|| panic!("A's line rendered:\n{}", frames.join("\n---\n")));
    let after = frames[before..]
        .iter()
        .find(|frame| frame.contains("$ 2 shells"))
        .unwrap_or_else(|| {
            panic!(
                "B's shells never reached the open view; last frame:\n{}",
                frames.last().map_or("", String::as_str)
            )
        });
    assert_eq!(
        shell_lines(after),
        ["$ 2 shells \u{b7} 2 persistent"],
        "A's ended shell left its line:\n{after}"
    );
}
