#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "the headless TUI runs stay on the stack; each test awaits one at a time"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "each TUI scenario in this suite reads top to bottom"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "the pids read back from the wire and passed to `kill` are kernel-bounded"
    )
)]

//! End-to-end verifier for the interactive TUI on the supervisor: the
//! scenarios of `interactive_daemon_e2e` (which runs on the runtime daemon,
//! the default) that do not move yet, unchanged, so they keep running while
//! the supervisor ships. Each waits on a runtime gap or a harness seam:
//! the sign-in flow's model apply (the scripted engine refused the switch;
//! the runtime makes it, so the turn needs an offline provider), the
//! Anthropic warning's attach, a create whose model has no credentials, the
//! tree pane's entries, and the close reasons only the supervisor's
//! child-close cascade sends (`rlmCloseReason`) and its scripted engine's
//! `prompt cancelled`. Spawn the real supervisor (`prime-agent --mode
//! daemon`), drive the TUI headlessly against a scripted daemon session, and
//! assert on rendered frames plus daemon-side state. The `script` seam is the
//! faux provider contract; the product never sets it.
#![cfg(unix)]

mod support;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

/// Stop the daemon on `socket` by protocol so it can shut its workers down;
/// kill the child when the protocol path fails.
impl Drop for Supervisor {
    fn drop(&mut self) {
        graceful_shutdown(&self.socket);
        // Freeze the whole tree before any kill: a worker's grandchildren (the
        // commands its session started) outlive a worker-only kill and
        // keep writing into the test's temp dir after its removal. A stopped
        // process cannot fork, so the walk settles once every member is stopped.
        let root = self.child.id();
        unsafe {
            libc::kill(root as i32, libc::SIGSTOP);
        }
        let mut tree = Vec::new();
        loop {
            let mut walked = descendants_of(root);
            walked.sort_unstable();
            if walked == tree {
                break;
            }
            for &pid in &walked {
                unsafe {
                    libc::kill(pid as i32, libc::SIGSTOP);
                }
            }
            tree = walked;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in tree {
            kill_worker(pid);
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Every process below `root`, following parent links.
fn descendants_of(root: u32) -> Vec<u32> {
    let mut tree = Vec::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for child in child_pids_of(pid) {
            tree.push(child);
            frontier.push(child);
        }
    }
    tree
}

/// Kill a leaked worker process (SIGKILL) and wait briefly for it to disappear.
fn kill_worker(pid: u32) {
    // The worker pid is not our child, so poll /proc liveness. Best effort by
    // design: an assert in `Drop` would abort the process and orphan the parallel
    // tests; the contract lives in `assert_daemon_stops_clean`.
    for round in 0..2 {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(pid) {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !process_alive(pid) {
            return;
        }
        eprintln!("worker {pid} survived teardown kill round {round}; re-killing");
    }
    eprintln!("worker {pid} still alive after two teardown kills");
}

#[cfg(target_os = "linux")]
fn child_pids_of(ppid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let Ok(entry_pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{entry_pid}/stat")) else {
            continue;
        };
        // `comm` can contain spaces and parens, so parse after the last ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next(); // process state
        let Ok(parent) = fields.next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        if parent == ppid {
            pids.push(entry_pid);
        }
    }
    pids
}

/// Liveness that ignores zombies: a detached child nobody reaps keeps its
/// `/proc` entry, so path existence alone would call an exited process alive.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `comm` can contain spaces and parens, so parse after the last ')'.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next().unwrap_or_default();
    !state.starts_with('Z') && !state.starts_with('X')
}

/// The children of `ppid` without `/proc`: one `ps` listing of every pid's parent.
#[cfg(not(target_os = "linux"))]
fn child_pids_of(parent_pid: u32) -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent: u32 = fields.next()?.parse().ok()?;
            (parent == parent_pid).then_some(pid)
        })
        .collect()
}

/// Liveness without `/proc`: the platform probe, which also reads zombies as dead.
#[cfg(not(target_os = "linux"))]
fn process_alive(pid: u32) -> bool {
    pa_types::platform::process::is_process_alive(pid).unwrap_or(false)
}

/// Sync JSONL shutdown request (Drop runs inside the async test runtime, so
/// no nested runtime). Returns the supervisor pid from the hello.
fn graceful_shutdown(socket: &Path) -> Option<u32> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let Ok(stream) = UnixStream::connect(socket) else {
        return None;
    };
    let Ok(write_half) = stream.try_clone() else {
        return None;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let mut hello = String::new();
    let _ = reader.read_line(&mut hello); // daemon_hello
    let supervisor_pid = serde_json::from_str::<serde_json::Value>(hello.trim())
        .ok()
        .and_then(|hello| hello["supervisorPid"].as_u64())
        .map(|pid| pid as u32);

    let command = serde_json::json!({
        "type": "command",
        "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let Ok(mut line) = serde_json::to_string(&command) else {
        return None;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() {
        return None;
    }
    let _ = writer.flush();
    // The response is the sync point: the supervisor stops every worker before exiting.
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
    supervisor_pid
}

/// The headless run's wall: a turn that never settles holds the idle gate
/// closed and parks the run forever; the wall makes that a failing test.
const HEADLESS_RUN_BOUND: Duration = Duration::from_secs(300);

async fn run_headless_bounded(
    options: pa_tui::interactive::InteractiveOptions,
    plan: pa_tui::interactive::HeadlessPlan,
) -> anyhow::Result<pa_tui::interactive::InteractiveOutcome> {
    support::scrub_ambient_provider_credentials();
    let started = Instant::now();
    match tokio::time::timeout(
        HEADLESS_RUN_BOUND,
        pa_tui::interactive::run_interactive(options, pa_tui::interactive::UiMode::Headless(plan)),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(tokio::time::error::Elapsed { .. }) => panic!(
            "the headless run exceeded the {HEADLESS_RUN_BOUND:?} wall after {:?}: the wedge class - a turn never settled and the idle gate never opened",
            started.elapsed()
        ),
    }
}

/// Kill supervisors leaked by earlier runs (a dead binary runs no `Drop`, so
/// its daemons are reparented to init): a daemon matches when its socket dir
/// is a `tempfile`-created `.tmpXXXXXX` dir and its parent is dead.
/// Linux only: it walks `/proc`, and pairs with the Linux-only parent-death signal.
#[cfg(target_os = "linux")]
fn sweep_orphan_test_daemons() {
    static SWEEPED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if SWEEPED.set(()).is_err() {
        return; // a later test's spawn: the first spawn already swept
    }
    let mut swept = 0;
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        // /proc cmdline is NUL-separated.
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        let flag = |needle: &str| args.iter().any(|a| a == needle);
        let arg_after = |needle: &str| {
            args.iter()
                .position(|a| a == needle)
                .and_then(|idx| args.get(idx + 1))
                .cloned()
        };
        if !flag("--mode") || arg_after("--mode").as_deref() != Some("daemon") {
            continue;
        }
        let Some(socket) = arg_after("--daemon-socket") else {
            continue;
        };
        // A live run's daemon keeps its owning test binary as the parent, and
        // the user's product daemons fail the socket-dir test, so neither is swept.
        let orphan_socket_dir = Path::new(&socket)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".tmp"));
        if !orphan_socket_dir {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let Ok(parsed_ppid) = rest
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .parse::<u32>()
        else {
            continue;
        };
        if parsed_ppid != 1 && process_alive(parsed_ppid) {
            continue; // a live run's daemon: its test binary is still up
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        swept += 1;
    }
    if swept > 0 {
        eprintln!("swept {swept} orphan test daemon(s) (socket dir gone) before spawning");
    }
}

fn spawn_supervisor(dir: &Path) -> Supervisor {
    support::scrub_ambient_provider_credentials();
    #[cfg(target_os = "linux")]
    sweep_orphan_test_daemons();
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        // The supervisor (selected by name: the runtime daemon is the default), which this
        // suite drives on the old wire.
        .env("PRIME_AGENT_RUNTIME_DAEMON", "0")
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`: keep them in the
        // test's temp dir, which a SIGKILLed worker never cleans up after.
        .env("TMPDIR", dir)
        // The daemon's startup catalog refresh must never reach the network
        // from a test: PI_OFFLINE keeps it on the bundled/models.json snapshot.
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The launcher strips inherited worker role env vars before spawning the
    // supervisor; a CLI running inside a daemon worker must not leak them.
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    // Ambient provider credentials must not leak into the daemon's catalog:
    // every spawned supervisor here serves models.json fixtures only.
    for provider in pa_ai::models_generated::get_providers() {
        if let Some(vars) = pa_ai::env_api_keys::get_api_key_env_vars(provider) {
            for var in vars {
                command.env_remove(var);
            }
        }
    }
    command.env_remove("PRIME_TEAM_ID");
    // A supervisor killed by a failing test must not leak its session workers:
    // the supervisor-lost exit runs on this short window, not the 5-minute default.
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    // A full workspace run can starve a session worker's boot far past the
    // 30s default connect budget; this keeps session creates deterministic.
    command.env("PA_DAEMON_WORKER_CONNECT_TIMEOUT_MS", "90000");
    // Die with this test binary: the kernel SIGKILLs a supervisor that gets
    // reparented to init; the guard's protocol teardown stays the exit path.
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn prime-agent --mode daemon");
    support::await_listening(&socket, &mut child);
    Supervisor { child, socket }
}

/// Create a live session through the daemon protocol, writing the scripted engine config first.
async fn create_session_via_daemon(
    socket: &Path,
    script_path: &Path,
    script: &serde_json::Value,
    cwd: &Path,
    session_dir: &Path,
) -> String {
    std::fs::write(script_path, script.to_string()).expect("write script");
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect supervisor");
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: None,
            name: None,
            config: Some(serde_json::json!({
                "cwd": cwd.display().to_string(),
                "sessionDir": session_dir.display().to_string(),
                "script": script_path.display().to_string(),
            })),
            telemetry_disabled: None,
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("create session");
    client.close();
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .expect("session id")
        .to_string()
}

/// The product's settings-backed onboarding persistence (`SettingsOnboardingSink` glue).
struct FreshHomeOnboardingSink {
    cwd: PathBuf,
    agent_dir: PathBuf,
}

impl pa_tui::interactive::OnboardingSink for FreshHomeOnboardingSink {
    fn onboarding_shown(&self) -> bool {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .get_onboarding_shown()
    }

    fn agent_traces_choice_written(&self) -> bool {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .agent_traces_choice_written()
    }

    fn set_agent_traces_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .set_agent_traces_enabled(enabled)
    }

    fn mark_onboarding_complete(&self) -> anyhow::Result<()> {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .set_onboarding_shown(true)
    }

    fn onboarding_incomplete(
        &self,
        _outcome: &'static str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }
}

/// The scripted provider-auth surface the full-flow verifier drives: the
/// Prime Inference row, one api-key provider row, one `mcp:` service row.
struct FullFlowProviderAuth {
    agent_dir: PathBuf,
}

impl FullFlowProviderAuth {
    fn stored(&self, provider: &str) -> bool {
        pa_core::auth::AuthStorage::create(&self.agent_dir)
            .get_all()
            .credential(provider)
            .is_some()
    }
}

impl pa_tui::provider_auth::ProviderAuthCommands for FullFlowProviderAuth {
    fn login_options(&self) -> pa_tui::provider_auth::ProviderRowsFuture {
        let rows = vec![
            pa_tui::provider_auth::ProviderRow {
                id: pa_tui::provider_auth::PRIME_INFERENCE_PROVIDER_ID.to_string(),
                name: "Prime Inference".to_string(),
                auth_type: pa_tui::provider_auth::AuthType::ApiKey,
                status: Some(pa_tui::provider_auth::AuthStatusIndicator {
                    style: pa_tui::provider_auth::AuthStatusStyle::Success,
                    label: "configured".to_string(),
                }),
                flow: pa_tui::provider_auth::AuthFlow::TerminalFlow,
                configured: self.stored(pa_tui::provider_auth::PRIME_INFERENCE_PROVIDER_ID),
                available: true,
                credential: pa_tui::provider_auth::RowCredential::Default,
            },
            pa_tui::provider_auth::ProviderRow {
                id: "faux-key".to_string(),
                name: "Faux Key".to_string(),
                auth_type: pa_tui::provider_auth::AuthType::ApiKey,
                status: None,
                flow: pa_tui::provider_auth::AuthFlow::ApiKeyPrompt,
                configured: self.stored("faux-key"),
                available: true,
                credential: pa_tui::provider_auth::RowCredential::Default,
            },
            pa_tui::provider_auth::ProviderRow {
                id: "mcp:faux".to_string(),
                name: "Faux MCP".to_string(),
                auth_type: pa_tui::provider_auth::AuthType::Oauth,
                status: None,
                flow: pa_tui::provider_auth::AuthFlow::TerminalFlow,
                configured: false,
                available: true,
                credential: pa_tui::provider_auth::RowCredential::Default,
            },
        ];
        Box::pin(async move { rows })
    }

    fn logout_options(&self) -> pa_tui::provider_auth::ProviderRowsFuture {
        Box::pin(async move { Vec::new() })
    }

    fn login(
        &self,
        provider: &pa_tui::provider_auth::ProviderRow,
        api_key: Option<&str>,
        _target: &pa_tui::provider_auth::LoginTarget,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        let agent_dir = self.agent_dir.clone();
        let provider_id = provider.id.clone();
        let provider_name = provider.name.clone();
        let key = api_key.map(str::to_string);
        Box::pin(async move {
            let mut auth = pa_core::auth::AuthStorage::create(&agent_dir);
            auth.set(
                &provider_id,
                pa_core::auth::AuthCredential::ApiKey {
                    key: key.unwrap_or_default(),
                    prime_team: None,
                },
            );
            if auth.drain_errors().pop().is_some() {
                return pa_tui::provider_auth::ProviderAuthOutcome::Error(format!(
                    "Failed to save API key for {provider_name}"
                ));
            }
            pa_tui::provider_auth::ProviderAuthOutcome::Status(format!(
                "Saved API key for {provider_name}"
            ))
        })
    }

    fn login_on_panel(
        &self,
        provider: &pa_tui::provider_auth::ProviderRow,
        panel: pa_tui::auth_panel::AuthPanelHandle,
        _target: &pa_tui::provider_auth::LoginTarget,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        let agent_dir = self.agent_dir.clone();
        let provider_id = provider.id.clone();
        let provider_name = provider.name.clone();
        Box::pin(async move {
            // Only the Prime row runs here; anything else cancels.
            if provider_id != pa_tui::provider_auth::PRIME_INFERENCE_PROVIDER_ID {
                return pa_tui::provider_auth::ProviderAuthOutcome::Cancelled;
            }
            panel.progress("Checking Prime Inference access...");
            let Some(api_key) = panel
                .paste_prompt(
                    "Paste a Prime API key below:",
                    pa_tui::auth_panel::PastePromptTone::Muted,
                    pa_tui::auth_panel::PasteStyle::Visible,
                )
                .await
            else {
                return pa_tui::provider_auth::ProviderAuthOutcome::Cancelled;
            };
            let mut auth = pa_core::auth::AuthStorage::create(&agent_dir);
            auth.set(
                &provider_id,
                pa_core::auth::AuthCredential::ApiKey {
                    key: api_key,
                    prime_team: None,
                },
            );
            if auth.drain_errors().pop().is_some() {
                return pa_tui::provider_auth::ProviderAuthOutcome::Error(format!(
                    "Failed to login to {provider_name}"
                ));
            }
            pa_tui::provider_auth::ProviderAuthOutcome::Status(format!(
                "Saved API key for {provider_name}. Credentials saved to {}.",
                agent_dir.join("auth.json").display()
            ))
        })
    }

    fn logout(
        &self,
        _provider: &pa_tui::provider_auth::ProviderRow,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        Box::pin(async move { pa_tui::provider_auth::ProviderAuthOutcome::Cancelled })
    }

    fn anthropic_subscription_warning(&self) -> pa_tui::provider_auth::ProviderWarningFuture {
        // The faux full-flow drives no Anthropic subscription auth.
        Box::pin(async move { None })
    }
}

/// The full first-run flow with no usable model (TS `runOnboardingFlow`'s
/// not-ready branch): every answer, both credentials, and the flag persist.
#[tokio::test]
async fn fresh_home_runs_the_full_sign_in_flow_to_completion() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());

    let script = serde_json::json!({ "responses": [
        { "text": "hello full flow", "delayMs": 20 },
    ] });
    std::fs::write(
        dir.path().join("script.json"),
        serde_json::to_string(&script).expect("script json"),
    )
    .expect("script.json");

    let mut options = base_options(&supervisor, dir.path(), &session_dir);
    // The readiness probe mirrors the flow's contract: not ready until the
    // sign-in stores its credential (the model-ready gate at flow end).
    let probe_agent_dir = agent_dir.clone();
    let model_ready = std::sync::Arc::new(move || {
        pa_core::auth::AuthStorage::create(&probe_agent_dir)
            .get_all()
            .credential(pa_tui::provider_auth::PRIME_INFERENCE_PROVIDER_ID)
            .is_some()
    });
    options.onboarding = Some(pa_tui::interactive::OnboardingTask {
        sink: std::sync::Arc::new(FreshHomeOnboardingSink {
            cwd: dir.path().to_path_buf(),
            agent_dir: agent_dir.clone(),
        }),
        model_ready,
        current_model: None,
        provider_auth: Some(pa_tui::provider_auth::ProviderAuthCommandsHandle(
            std::sync::Arc::new(FullFlowProviderAuth {
                agent_dir: agent_dir.clone(),
            }),
        )),
    });
    let enter = || {
        pa_tui::interactive::HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
    };
    let down = || {
        pa_tui::interactive::HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ))
    };
    // The plan waits on observable readiness, not fixed sleeps: each barrier
    // holds the queued batch until a rendered frame contains the condition.
    let wait_render = |needle: &str| pa_tui::interactive::HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 5_000,
    };
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            enter(),
            wait_render("Paste a Prime API key below:"),
            pa_tui::interactive::HeadlessStep::Type("faux-prime-key".to_string()),
            enter(),
            wait_render("Prime Inference is your default. Connect other providers, or continue."),
            down(),
            enter(),
            wait_render("Enter API key"),
            pa_tui::interactive::HeadlessStep::Type("faux-key".to_string()),
            enter(),
            wait_render("\u{2713}"),
            enter(),
            wait_render("Share agent traces"),
            enter(),
            pa_tui::interactive::HeadlessStep::Submit("hi".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
        ],
        width: 100,
        height: 30,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run");

    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("Log in with Prime Intellect"),
        "the welcome screen's action rendered:\n{rendered}"
    );
    assert!(
        outcome.frames.iter().any(|frame| {
            frame.contains("Log in with Prime Intellect")
                && frame.contains("Paste a Prime API key below:")
        }),
        "the login dialog's heading replaced the brand line:\n{rendered}"
    );
    assert!(
        rendered.contains("Paste a Prime API key below:"),
        "the paste prompt mounted inside the pane:\n{rendered}"
    );
    assert!(
        rendered.contains("Prime Inference is your default. Connect other providers, or continue."),
        "the providers picker rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Share agent traces"),
        "the trace question ended the flow:\n{rendered}"
    );
    assert!(
        rendered.contains("hello full flow"),
        "the completed flow released the pane and the first turn ran:\n{rendered}"
    );
    // The scripted engine refuses live model switches by design, so the
    // refusal row is the proof the apply REQUEST reached it.
    assert!(
        rendered.contains("This session does not support model switching"),
        "the apply round-tripped and the scripted engine's refusal surfaced:\n{rendered}"
    );
    assert!(
        rendered.contains("Saved API key for Faux Key"),
        "the provider login's status row applied:\n{rendered}"
    );

    let settings = pa_core::settings::SettingsManager::create(dir.path(), &agent_dir);
    assert!(
        settings.get_onboarding_shown(),
        "the completed flow marked onboarding shown"
    );
    assert!(
        settings.get_agent_traces_enabled(),
        "the Share answer persisted"
    );
    let auth = pa_core::auth::AuthStorage::create(&agent_dir);
    assert!(
        auth.get_all()
            .credential(pa_tui::provider_auth::PRIME_INFERENCE_PROVIDER_ID)
            .is_some(),
        "the Prime sign-in stored its credential"
    );
    assert!(
        auth.get_all().credential("faux-key").is_some(),
        "the provider login stored its key"
    );
    drop(supervisor);
}

#[tokio::test]
async fn tui_session_tree_navigates_forks_and_clones() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());
    let script = serde_json::json!({
        "engine": "faux",
        "responses": [
            { "text": "first answer", "delayMs": 10 },
            { "text": "second answer", "delayMs": 10 },
            { "text": "post-fork answer", "delayMs": 10 },
        ],
    });
    std::fs::write(dir.path().join("script.json"), script.to_string()).expect("write script");
    let options = pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        script_path: Some(dir.path().join("script.json")),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        initial_message: None,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        // The default tree filter keeps every message row visible, and the
        // branch-summary prompt is skipped (TS `branchSummary.skipPrompt`).
        tree_filter_mode: "default".to_string(),
        branch_summary_skip_prompt: true,
        show_images: true,
        fullscreen_mouse: true,
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
    let key = |code: KeyCode| {
        pa_tui::interactive::HeadlessStep::Key(crossterm::event::KeyEvent::new(
            code,
            KeyModifiers::NONE,
        ))
    };
    let enter = key(KeyCode::Enter);
    let up = key(KeyCode::Up);
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::Submit("first question".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            pa_tui::interactive::HeadlessStep::Submit("second question".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            // `/tree` opens the selector; Enter on the leaf is the TS no-op.
            pa_tui::interactive::HeadlessStep::Submit("/tree".to_string()),
            pa_tui::interactive::HeadlessStep::SettleIdle,
            enter.clone(),
            pa_tui::interactive::HeadlessStep::SettleIdle,
            // `/fork` opens the selector; Enter forks before the selected (latest) user message.
            pa_tui::interactive::HeadlessStep::Submit("/fork".to_string()),
            pa_tui::interactive::HeadlessStep::SettleIdle,
            enter.clone(),
            pa_tui::interactive::HeadlessStep::SettleIdle,
            // The fork re-entered the user message; submit runs it on the forked session.
            enter.clone(),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            // `/tree` again, then two rows up (the first answer) cuts the branch back.
            pa_tui::interactive::HeadlessStep::Submit("/tree".to_string()),
            pa_tui::interactive::HeadlessStep::SettleIdle,
            up.clone(),
            up.clone(),
            enter.clone(),
            pa_tui::interactive::HeadlessStep::SettleIdle,
        ],
        width: 100,
        height: 34,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run");
    if let Ok(dump) = std::env::var("PA_TUI_DUMP_FRAMES") {
        for (index, frame) in outcome.frames.iter().enumerate() {
            let _ = std::fs::write(
                std::path::Path::new(&dump).join(format!("tree-frame-{index:03}.txt")),
                frame,
            );
        }
    }
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("Session tree"),
        "the tree pane rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Search entries"),
        "the search line rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("user: first question") && rendered.contains("user: second question"),
        "the entry rows rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Already at this point"),
        "the leaf selection was a no-op:\n{rendered}"
    );
    assert!(
        rendered.lines().any(|row| row.trim() == "Fork"),
        "the fork selector rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Forked to new session"),
        "the fork note rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("post-fork answer"),
        "the forked session ran a turn:\n{rendered}"
    );
    // The transcript rebuilt on the moved branch (the abandoned turn drops).
    assert!(
        rendered.contains("Navigated to selected point"),
        "the navigation note rendered:\n{rendered}"
    );
    let settled = outcome
        .frames
        .iter()
        .rev()
        .find(|frame| frame.contains("Navigated to selected point"))
        .expect("the navigation frame");
    assert!(
        !settled.contains("post-fork answer"),
        "the abandoned branch dropped from the rebuilt transcript:\n{settled}"
    );
    assert!(
        settled.contains("first answer"),
        "the moved branch kept the target path:\n{settled}"
    );
    let session_files: Vec<_> = support::session_texts(&session_dir).into_iter().collect();
    assert!(
        session_files.len() >= 2,
        "the fork wrote a new session file: {} files",
        session_files.len()
    );
    drop(supervisor);
}

/// The live-dogfood failure pair (Kevin's repro, 2026-09-21): the flagged
/// model resolves from the full catalog and fails at run-start auth
/// validation with the TS login-guidance message — never "Model not found".
#[tokio::test]
async fn tui_flagged_model_turn_reports_the_ts_preflight_error_without_credentials() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());
    // No models.json and no credentials (ambient keys stripped), so the
    // auth-scoped catalog is empty but the bundled catalog carries the model.
    let glm: pa_types::ai::Model = serde_json::from_value(serde_json::json!({
        "id": "z-ai/glm-5.3", "name": "GLM 5.3", "api": "openai-completions",
        "provider": "prime-inference", "baseUrl": "https://inference.example/v1",
        "reasoning": true, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 8192
    }))
    .expect("catalog entry");
    let options = pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        script_path: None,
        model_selection: pa_tui::interactive::ModelSelection {
            provider: Some("prime-inference".to_string()),
            model: Some("z-ai/glm-5.3".to_string()),
            ..Default::default()
        },
        model_catalog: vec![glm],
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        show_images: true,
        client_settings: None,
        fullscreen_mouse: true,
        initial_message: None,
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
    };
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            pa_tui::interactive::HeadlessStep::Submit("/model".to_string()),
            pa_tui::interactive::HeadlessStep::Type("glm".to_string()),
            pa_tui::interactive::HeadlessStep::Type("\n".to_string()),
            pa_tui::interactive::HeadlessStep::Submit("hello".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 60_000 },
        ],
        width: 120,
        height: 36,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("must be configured externally") && rendered.contains("prime-inference"),
        "the pick against the empty auth-scoped catalog routes the sign-in flow (no provider-auth hook in this composition, so the TS external-config error):\n{rendered}"
    );
    assert!(
        !rendered.contains("Model not found: "),
        "the not-signed-in pick never surfaces the dead-end refusal (the typed sign-in class):\n{rendered}"
    );
    assert!(
        rendered.contains("No API key found for prime-inference"),
        "the turn resolves the flagged model from the full catalog and fails at the run-start auth validation with the TS message:\n{rendered}"
    );
    assert!(
        !rendered.contains("No models available"),
        "the auth-blind turn resolution must not report the resolver's empty-catalog error:\n{rendered}"
    );
    let last = outcome.frames.last().expect("a final frame");
    // The failed pick must hold the `model:effort` label with glm-5.3's effective level: the
    // tray row ends on it (whatever badge the tray puts in front).
    assert!(
        last.lines()
            .any(|row| row.trim_end().ends_with(" z-ai/glm-5.3:high")),
        "the footer label holds the resolved flagged model (the failed pick switched nothing):\n{last}"
    );
    drop(supervisor);
}

/// The base options every utility-command verifier shares.
fn base_options(
    supervisor: &Supervisor,
    dir: &Path,
    session_dir: &Path,
) -> pa_tui::interactive::InteractiveOptions {
    pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: supervisor.socket.clone(),
        cwd: dir.to_path_buf(),
        session_dir: Some(session_dir.to_path_buf()),
        script_path: Some(dir.join("script.json")),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        show_images: true,
        fullscreen_mouse: true,
        initial_message: None,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
        provider_auth: None,
        traces: None,
        update_commands: None,
    }
}

/// The Anthropic subscription ban-risk warning's detection text: the fake
/// auth resolves it for the e2e (the product text the login-completed arm
/// draws is the `ANTHROPIC_SUBSCRIPTION_AUTH_WARNING` constant; the two
/// stay distinguishable).
const E2E_SUBSCRIPTION_WARNING: &str = "E2E anthropic subscription ban-risk warning";

/// The e2e's fake auth surface: the credential-detection arm resolves a
/// subscription warning (the product's `getAnthropicSubscriptionAuthWarning`
/// seam — a stored OAuth credential answers the warning text).
struct E2ESubscriptionAuth;

impl pa_tui::provider_auth::ProviderAuthCommands for E2ESubscriptionAuth {
    fn login_options(&self) -> pa_tui::provider_auth::ProviderRowsFuture {
        Box::pin(async move { Vec::new() })
    }

    fn logout_options(&self) -> pa_tui::provider_auth::ProviderRowsFuture {
        Box::pin(async move { Vec::new() })
    }

    fn login(
        &self,
        _provider: &pa_tui::provider_auth::ProviderRow,
        _api_key: Option<&str>,
        _target: &pa_tui::provider_auth::LoginTarget,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        Box::pin(async move { pa_tui::provider_auth::ProviderAuthOutcome::Cancelled })
    }

    fn login_on_panel(
        &self,
        _provider: &pa_tui::provider_auth::ProviderRow,
        _panel: pa_tui::auth_panel::AuthPanelHandle,
        _target: &pa_tui::provider_auth::LoginTarget,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        Box::pin(async move { pa_tui::provider_auth::ProviderAuthOutcome::Cancelled })
    }

    fn logout(
        &self,
        _provider: &pa_tui::provider_auth::ProviderRow,
    ) -> pa_tui::provider_auth::ProviderAuthFuture {
        Box::pin(async move { pa_tui::provider_auth::ProviderAuthOutcome::Cancelled })
    }

    fn anthropic_subscription_warning(&self) -> pa_tui::provider_auth::ProviderWarningFuture {
        Box::pin(async move { Some(E2E_SUBSCRIPTION_WARNING) })
    }
}

/// The interactive options for a session on an Anthropic model with the
/// fake subscription credential surface (the detection arm's two inputs).
fn subscription_options(
    supervisor: &Supervisor,
    dir: &Path,
    session_dir: &Path,
    session: pa_tui::interactive::SessionSelection,
) -> pa_tui::interactive::InteractiveOptions {
    let mut options = base_options(supervisor, dir, session_dir);
    options.model_selection = pa_tui::interactive::ModelSelection {
        provider: Some("anthropic".to_string()),
        model: Some("claude-test".to_string()),
        api_key: None,
        credential: None,
        profile: None,
        thinking: None,
    };
    options.provider_auth = Some(pa_tui::provider_auth::ProviderAuthCommandsHandle(
        std::sync::Arc::new(E2ESubscriptionAuth),
    ));
    options.session = session;
    options
}

/// The Anthropic subscription warning fires once per session LIFECYCLE,
/// not on every open (operator directive 2026-09-29): a new session on an
/// Anthropic subscription credential draws the ban-risk warning once and
/// marks the session's persisted gate with the daemon — the marker row is
/// durable in the session file and `get_state` serves it — and a FRESH
/// TUI process attaching to that session draws NO warning: the reattach
/// reads the gate. This is the end-to-end composition of the client gate
/// and the daemon's marker (the supervisor routes the new
/// `mark_anthropic_warning_shown` frame to the worker).
#[tokio::test]
async fn tui_anthropic_warning_warns_once_then_a_fresh_process_reattaches_silently() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());

    // The scripted model fixture (the harness's opt-in `model` knob): the
    // session reports an Anthropic model, so the startup detection arm's
    // provider gate passes and the fake credential resolves the warning.
    let script = serde_json::json!({
        "responses": [],
        "model": { "id": "claude-test", "provider": "anthropic", "reasoning": false },
    });
    std::fs::write(
        dir.path().join("script.json"),
        serde_json::to_string(&script).expect("script json"),
    )
    .expect("script.json");

    // Run one: the fresh session warns once and marks the gate.
    let options = subscription_options(
        &supervisor,
        dir.path(),
        &session_dir,
        pa_tui::interactive::SessionSelection::New,
    );
    let plan = pa_tui::interactive::HeadlessPlan {
        // The exit gate holds the run until the fire-and-forget mark's
        // write resolves (the worker persists the marker row before its
        // ack), so the durable-row assertions below read completed state —
        // no timing window guards them.
        steps: vec![pa_tui::interactive::HeadlessStep::WaitRender {
            needle: E2E_SUBSCRIPTION_WARNING.to_string(),
            timeout_ms: 15_000,
        }],
        width: 100,
        height: 30,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run one");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains(E2E_SUBSCRIPTION_WARNING),
        "the new session drew the ban-risk warning:\n{rendered}"
    );
    let session = outcome.active_session_id.clone();

    // The gate is durable: the marker row landed in the session file, and
    // the daemon's `get_state` serves it open.
    let file = session_dir.join(format!("{}.jsonl", outcome.session_id));
    let persisted = support::session_texts(&session_dir)
        .into_iter()
        .find(|(locator, _)| locator.file_name() == file.file_name())
        .map_or_else(
            || panic!("the session file {} is not saved", file.display()),
            |(_, text)| text,
        );
    assert!(
        persisted.contains("anthropic_subscription_warning_shown"),
        "the marker row reached the session file:\n{persisted}"
    );
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&supervisor.socket)
        .await
        .expect("connect supervisor");
    let state = client
        .request_ok(DaemonCommand::GetState {
            id: None,
            active_session_id: session.clone(),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("get_state");
    client.close();
    assert_eq!(
        state.get("anthropicWarningShown"),
        Some(&serde_json::json!(true)),
        "the daemon serves the open gate: {state}"
    );

    // Run two: a FRESH TUI process attaches to the same session — the
    // gate holds, no warning renders anywhere in the run.
    let options = subscription_options(
        &supervisor,
        dir.path(),
        &session_dir,
        pa_tui::interactive::SessionSelection::Attach(session.clone()),
    );
    let plan = pa_tui::interactive::HeadlessPlan {
        // The detection arm is awaited at open, so the first dock frame
        // proves its decision baked in — the reattach's negative reads
        // completed state, not a timing window (a late warning cannot
        // miss the window: the open either warned or skipped before the
        // frame painted).
        steps: vec![pa_tui::interactive::HeadlessStep::WaitRender {
            needle: "subagents".to_string(),
            timeout_ms: 15_000,
        }],
        width: 100,
        height: 30,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run two");
    let rendered = outcome.frames.join("\n");
    assert!(
        !rendered.contains(E2E_SUBSCRIPTION_WARNING),
        "the reattaching process did not re-render the warning:\n{rendered}"
    );
    assert!(
        !rendered.contains("Anthropic subscription auth is active"),
        "neither arm re-warned on the reattach:\n{rendered}"
    );
    assert_eq!(outcome.active_session_id, session, "run two attached by id");
    drop(supervisor);
}

/// A prompt accepted before the worker is killed must surface the TS
/// connection-closed error row, not a quiet status note. The killer waits
/// for the worker's `isStreaming` state (an external admission condition),
/// so I/O stalls cannot move a fixed submit/kill clock across the edge.
#[tokio::test]
async fn tui_accepted_then_killed_turn_renders_closed_error() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let session_dir = dir.path().join("agent/sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());
    let script = serde_json::json!({ "responses": [
        { "text": "reply must not arrive", "delayMs": 60_000 },
    ] });
    let script_path = dir.path().join("script.json");
    let session_id = create_session_via_daemon(
        &supervisor.socket,
        &script_path,
        &script,
        dir.path(),
        &session_dir,
    )
    .await;
    let kill_socket = supervisor.socket.clone();
    let kill_session_id = session_id.clone();
    let kill_task = tokio::spawn(async move {
        let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&kill_socket)
            .await
            .expect("connect supervisor for kill");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = client
                .request_ok(DaemonCommand::GetConnectionState {
                    id: None,
                    active_session_id: kill_session_id.clone(),
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("get worker connection state");
            if state["isStreaming"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker never began the accepted turn: {state}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        client
            .request_ok(DaemonCommand::Kill {
                id: None,
                active_session_id: kill_session_id,
                rest: serde_json::Map::default(),
            })
            .await
            .expect("kill admitted session");
        client.close();
    });
    let options = pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir),
        script_path: Some(script_path),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::Attach(session_id),
        show_images: true,
        fullscreen_mouse: true,
        initial_message: None,
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
    let error_row = "⚠ Error: The daemon stopped this agent session. Its transcript remains saved and can be reopened from Agents View.";
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::Submit("held accepted prompt".to_string()),
            // The cancellation status is a later, replaceable note. Require
            // it first so the error assertion covers the end state after
            // the status that used to overwrite `session closed (killed)`.
            pa_tui::interactive::HeadlessStep::WaitRender {
                needle: "turn failed: prompt cancelled".to_string(),
                timeout_ms: 30_000,
            },
            pa_tui::interactive::HeadlessStep::WaitRender {
                needle: error_row.to_string(),
                timeout_ms: 30_000,
            },
        ],
        width: 180,
        height: 30,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run");
    kill_task.await.expect("the kill task");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains(error_row),
        "killed turn had {} frames without the TS error row; old_info={}; final={}",
        outcome.frames.len(),
        rendered.contains("session closed (killed)"),
        outcome.frames.last().expect("final frame")
    );
    assert!(
        !rendered.contains("reply must not arrive"),
        "held response leaked:\n{rendered}"
    );
    let last = outcome.frames.last().expect("final frame");
    assert!(
        last.contains("turn failed: prompt cancelled"),
        "the post-close cancellation must reach the frame:\n{last}"
    );
    assert!(
        last.contains(error_row),
        "close error row must persist after cancellation:\n{last}"
    );
    assert!(
        !last.contains("session closed (killed)"),
        "info downgrade remains:\n{last}"
    );
    drop(supervisor);
}

/// Both terminal close reasons remain visible after a later, replaceable turn status.
#[tokio::test]
async fn tui_close_reason_rows_survive_late_turn_status() {
    for (reason, explanation) in [
        ("shutdown", "The Prime Agent daemon shut down while this window was attached. The session transcript remains saved; restart Prime Agent and reopen it from Agents View."),
        ("replaced", "The daemon replaced this agent session with another session. Reopen the current session from Agents View."),
    ] {
        assert_close_reason_survives_late_turn_status(reason, explanation).await;
    }
}

async fn assert_close_reason_survives_late_turn_status(reason: &str, explanation: &str) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let session_dir = dir.path().join("agent/sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());
    let script = serde_json::json!({ "responses": [
        { "text": "reply must not arrive", "delayMs": 60_000 },
    ] });
    let script_path = dir.path().join("script.json");
    let session_id = create_session_via_daemon(
        &supervisor.socket,
        &script_path,
        &script,
        dir.path(),
        &session_dir,
    )
    .await;
    let kill_socket = supervisor.socket.clone();
    let reason_owned = reason.to_string();
    let kill_session_id = session_id.clone();
    let kill_task = tokio::spawn(async move {
        let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&kill_socket)
            .await
            .expect("connect supervisor for kill");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = client
                .request_ok(DaemonCommand::GetConnectionState {
                    id: None,
                    active_session_id: kill_session_id.clone(),
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("get worker connection state");
            if state["isStreaming"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker never began the accepted turn: {state}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        client
            .request_ok(DaemonCommand::Kill {
                id: None,
                active_session_id: kill_session_id,
                rest: serde_json::Map::from_iter([(
                    "rlmCloseReason".to_string(),
                    serde_json::Value::String(reason_owned),
                )]),
            })
            .await
            .expect("kill admitted session");
        client.close();
    });
    let options = pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir),
        script_path: Some(script_path),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::Attach(session_id),
        show_images: true,
        fullscreen_mouse: true,
        initial_message: None,
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
    let error_row = format!("⚠ Error: {explanation}");
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::Submit("held accepted prompt".to_string()),
            // The cancellation status is a later, replaceable note. Require
            // it first so the error assertion covers the end state after
            // the status that used to overwrite `session closed (killed)`.
            pa_tui::interactive::HeadlessStep::WaitRender {
                needle: "turn failed: prompt cancelled".to_string(),
                timeout_ms: 30_000,
            },
            pa_tui::interactive::HeadlessStep::WaitRender {
                needle: error_row.clone(),
                timeout_ms: 30_000,
            },
        ],
        width: 180,
        height: 30,
    };
    let outcome = run_headless_bounded(options, plan)
        .await
        .expect("interactive run");
    kill_task.await.expect("the kill task");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains(&error_row),
        "{reason} turn had {} frames without the TS error row; old_info={}; final={}",
        outcome.frames.len(),
        rendered.contains(&format!("session closed ({reason})")),
        outcome.frames.last().expect("final frame")
    );
    assert!(
        !rendered.contains("reply must not arrive"),
        "held response leaked:\n{rendered}"
    );
    assert!(
        !rendered.contains("timed out waiting for the headless render condition"),
        "the close/cancellation render barrier must complete:\n{rendered}"
    );
    // Verify coexistence after the cancellation status in one observed frame.
    // A later successful reconnect may resync the transcript again.
    let checkpoint = outcome
        .frames
        .iter()
        .find(|frame| frame.contains("turn failed: prompt cancelled") && frame.contains(&error_row))
        .unwrap_or_else(|| {
            panic!("the close error and cancellation must share a frame:\n{rendered}")
        });
    assert!(
        !checkpoint.contains(&format!("session closed ({reason})")),
        "info downgrade remains:\n{checkpoint}"
    );
    drop(supervisor);
}
