//! An attached interactive window lives through the daemon's death.
//!
//! The daemon is launched the way a window launches it (under its restart
//! owner), a turn is started, and the session-hosting process is killed (SIGKILL).
//! The window does nothing but wait: the restart owner brings the daemon
//! back, the window reattaches by itself, shows what happened (the daemon's
//! recorded death and what it did to this session's turn), and the session
//! takes the next prompt. No daemon launcher is installed in this process,
//! so nothing on the client side can have started the daemon again.

#![cfg(unix)]
#![expect(
    clippy::too_many_lines,
    reason = "the one test spells out the window's full option set, as its siblings do"
)]

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pa_tui::interactive::HeadlessStep;
use serde_json::Value;

/// Stops the daemon (and with it its restart owner) when the test ends.
struct LaunchedDaemon {
    socket: PathBuf,
}

impl Drop for LaunchedDaemon {
    fn drop(&mut self) {
        use std::io::{BufRead, BufReader, Write};
        // Best effort by design: this runs inside `Drop`.
        let Ok(stream) = std::os::unix::net::UnixStream::connect(&self.socket) else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(stream);
        let mut hello = String::new();
        let _ = reader.read_line(&mut hello);
        let pids: Vec<i32> = serde_json::from_str::<Value>(hello.trim())
            .ok()
            .map_or_default(|hello| {
                ["keeperPid", "supervisorPid"]
                    .iter()
                    .filter_map(|key| hello[*key].as_u64())
                    .filter_map(|pid| i32::try_from(pid).ok())
                    .collect()
            });
        let _ = writer.write_all(
            b"{\"type\":\"command\",\"id\":\"stop\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"command\":{\"type\":\"shutdown\"}}\n",
        );
        let mut response = String::new();
        let _ = reader.read_line(&mut response);
        let deadline = Instant::now() + Duration::from_secs(10);
        let alive =
            |pid: &i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(*pid), None).is_ok();
        while pids.iter().any(alive) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        for pid in pids.iter().filter(|pid| alive(pid)) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn list_command() -> pa_types::daemon::DaemonCommand {
    pa_types::daemon::DaemonCommand::List {
        id: None,
        all: None,
        cwd: None,
        session_dir: None,
        include_client_owned: None,
        rest: serde_json::Map::default(),
    }
}

/// Wait until a session's turn is running on the daemon at `socket`, then
/// SIGKILL that daemon process. Returns the killed pid.
async fn kill_the_daemon_mid_turn(socket: &Path) -> u64 {
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect_with_retry(socket)
        .await
        .expect("connect to the daemon");
    let pid = client.hello()["supervisorPid"]
        .as_u64()
        .expect("the daemon's pid");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let listed = client
            .request_ok(list_command())
            .await
            .expect("list sessions");
        let running = listed["sessions"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["isSessionActive"] == true));
        if running {
            break;
        }
        assert!(Instant::now() < deadline, "no turn ever started: {listed}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a pid")),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("SIGKILL the daemon");
    pid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_reattaches_by_itself_after_the_daemon_is_killed_and_reports_what_happened() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let socket = dir.path().join("kept.sock");
    std::env::set_var("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir);
    std::env::set_var("DO_NOT_TRACK", "1");
    support::scrub_ambient_provider_credentials();
    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [{ "text": "slow answer", "delayMs": 4000 }] })
            .to_string(),
    )
    .expect("write script");

    let _guard = LaunchedDaemon {
        socket: socket.clone(),
    };
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    pa_cli::ensure_daemon_running_with(&exe, &socket, dir.path())
        .await
        .expect("launch the daemon under its restart owner");

    let options = pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir),
        script_path: Some(script_path),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        show_images: true,
        fullscreen_mouse: true,
        initial_message: Some("run the turn".to_string()),
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
    };
    let wait = |needle: &str| HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 90_000,
    };
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            // The window notices the loss, then the restarted daemon, on its own.
            wait("the daemon connection closed"),
            wait("Daemon restarted"),
            wait("was killed by signal 9 (SIGKILL)"),
            wait("Session interrupted by a daemon restart"),
            // The revived session finishes the interrupted turn and takes another.
            HeadlessStep::WaitIdle { timeout_ms: 90_000 },
            HeadlessStep::Submit("again".to_string()),
            HeadlessStep::WaitIdle { timeout_ms: 90_000 },
        ],
        width: 200,
        height: 50,
    };
    let killer = tokio::spawn({
        let socket = socket.clone();
        async move { kill_the_daemon_mid_turn(&socket).await }
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        Box::pin(pa_tui::interactive::run_interactive(
            options,
            pa_tui::interactive::UiMode::Headless(plan),
        )),
    )
    .await
    .expect("the window's run is bounded")
    .expect("the window's run");
    let killed_pid = killer.await.expect("the kill");

    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect_with_retry(&socket)
        .await
        .expect("connect to the restarted daemon");
    let hello = client.hello().clone();
    let rendered = outcome.frames.join("\n");
    let daemon_log =
        std::fs::read_to_string(pa_daemon::paths::daemon_log_path(&socket, &agent_dir))
            .unwrap_or_default();
    assert_eq!(
        (
            hello["supervisorPid"].as_u64() == Some(killed_pid),
            hello["daemonDeath"]["pid"].as_u64(),
            hello["daemonDeath"]["signal"].as_u64(),
            hello["daemonDeath"]["outcome"].as_str(),
            // The interrupted turn ran to its end in the revived session...
            rendered.contains("slow answer"),
            // ...and the session then took the next prompt (the script is spent: it echoes).
            outcome.last_assistant_text.as_deref(),
        ),
        (
            false,
            Some(killed_pid),
            Some(9),
            Some("restarted"),
            true,
            Some("echo: again"),
        ),
        "hello: {hello}\ndaemon log:\n{daemon_log}\nframes:\n{rendered}"
    );
}
