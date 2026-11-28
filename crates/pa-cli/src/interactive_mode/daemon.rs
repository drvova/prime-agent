//! The daemon-ensure concern: the socket probe, the retirement of an idle
//! supervisor an earlier release left running, the detached daemon spawn,
//! and the startup poll window.

use anyhow::Context as _;

use super::{anyhow, Command, Duration, Instant, Path, Result, Stdio};

const DAEMON_STARTUP_TIMEOUT_MS: u64 = 30_000;
/// Pause between daemon-startup probes (TS `ensureDaemonRunning` polls at
/// 25ms). A cold daemon binds its socket ~39ms after the spawn, so the
/// poll interval quantizes every cold launch: the bind lands 0-interval
/// before the next probe, a pure wait on a floor of microseconds. The 1ms
/// interval caps that overshoot at <=1ms for the cost of ~40 failed
/// connects per cold launch (each ~us; bounded by the same 30s startup
/// budget, so a hung boot adds at most ~1k connects/s of syscall work,
/// not a spin). Overshoot tables: probe-grid record 20261001-034500 on the
/// bench repo. Timing-only: the probe itself, the 30s startup budget, and
/// the timeout error are unchanged.
const DAEMON_PROBE_INTERVAL_MS: u64 = 1;

/// Ensure the runtime daemon is listening beside `daemon_socket` (its
/// `daemon2.sock`), spawning this executable in `--mode daemon` on
/// `daemon_socket` when it is not. Answers the runtime daemon's socket. An
/// idle supervisor an earlier release left on `daemon_socket` is stopped on a
/// task of its own ([`retire_idle_supervisor`]).
///
/// # Errors
/// Returns an error when the executable path cannot be resolved, the daemon
/// cannot be spawned, or its socket does not answer before the startup
/// timeout.
pub async fn ensure_runtime_daemon_running(
    daemon_socket: &Path,
    spawn_cwd: &Path,
) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("resolve the prime-agent executable")?;
    ensure_runtime_daemon_running_with(&exe, daemon_socket, spawn_cwd).await
}

/// [`ensure_runtime_daemon_running`] with an explicit daemon executable (the
/// product path uses this process's own binary).
///
/// # Errors
/// Returns an error when the daemon cannot be spawned, or its socket does not
/// answer before the startup timeout.
pub async fn ensure_runtime_daemon_running_with(
    exe: &Path,
    daemon_socket: &Path,
    spawn_cwd: &Path,
) -> Result<std::path::PathBuf> {
    let socket = pa_runtime::daemon::socket_path(daemon_socket);
    // Off the launch path: nothing here waits for it.
    tokio::spawn(retire_idle_supervisor(daemon_socket.to_path_buf()));
    let probe = Duration::from_millis(250);
    if pa_core::platform::daemon_socket::can_connect(&socket, probe).await {
        return Ok(socket);
    }
    // The daemon is spawned detached on `daemon_socket` (it outlives this CLI, and serves
    // `daemon2.sock` beside it).
    let mut command = Command::new(exe);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(daemon_socket)
        .current_dir(spawn_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The spawned process is the daemon's restart owner: it starts the daemon with these same
    // arguments and starts it again when it dies abnormally. Unix only: the owner forwards
    // signals and reads the daemon's terminating signal, neither of which exists on Windows,
    // where the daemon is started unkept as before.
    #[cfg(unix)]
    command.env(pa_runtime::daemon::keeper::KEEPER_ENV, "1");
    // A daemon must not share the launching TUI's terminal session: a session-wide terminal
    // cleanup could hang it up after the TUI exits; on Unix, setsid also creates its own process
    // group.
    #[cfg(unix)]
    pa_core::platform::process::set_new_session(&mut command);
    #[cfg(not(unix))]
    pa_core::platform::process::set_new_process_group(&mut command);
    command.spawn().with_context(|| {
        format!(
            "spawn the Prime Agent daemon on {}",
            daemon_socket.display()
        )
    })?;
    let deadline = Instant::now() + Duration::from_millis(DAEMON_STARTUP_TIMEOUT_MS);
    loop {
        if pa_core::platform::daemon_socket::can_connect(&socket, probe).await {
            return Ok(socket);
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Timed out waiting for the Prime Agent daemon to start on {} (the runtime daemon). Run: prime-agent shutdown --force, then retry the original command.",
                socket.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(DAEMON_PROBE_INTERVAL_MS)).await;
    }
}

/// Stop the supervisor on `daemon_socket` once it is idle, when a launch reaches the runtime
/// daemon beside it: a supervisor an earlier release started (from before the runtime daemon was
/// the default) holds its sessions' leases, which the runtime daemon needs to open them. Idle is
/// no session running a turn and none with a window attached; a busy supervisor is left alone
/// for a later launch to stop. Its sessions persist on disk, so the runtime daemon opens them
/// from their transcripts. Nothing answering is nothing to do, and so is the runtime daemon's own
/// legacy greeter on that socket (its hello says `legacyGreeter`).
async fn retire_idle_supervisor(daemon_socket: std::path::PathBuf) {
    let Ok((client, _events)) = pa_tui::daemon_client::DaemonClient::connect(&daemon_socket).await
    else {
        return;
    };
    if client.hello().get("legacyGreeter").is_some() {
        client.close();
        return;
    }
    let sessions = client
        .request_ok(pa_types::daemon::DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: serde_json::Map::default(),
        })
        .await;
    let idle = sessions.is_ok_and(|data| {
        data.get("sessions")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|rows| {
                rows.iter().all(|row| {
                    row.get("isSessionActive") != Some(&serde_json::json!(true))
                        && row
                            .get("attachedClients")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_default()
                            == 0
                })
            })
    });
    if idle {
        let _ = client
            .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
                id: None,
                force: None,
                rest: serde_json::Map::default(),
            })
            .await;
    }
    client.close();
}

/// The interactive UI's [`pa_tui::daemon_reconnect::DaemonLauncher`] on the runtime daemon: a
/// window that lost its daemon, and whose restart owner did not bring it back, gets it started
/// the way the launch did ([`ensure_runtime_daemon_running`], which leaves a daemon that answers
/// alone).
pub(super) struct RuntimeDaemonLauncher {
    /// The supervisor's socket the runtime daemon's (`daemon2.sock`) sits beside.
    pub(super) daemon_socket: std::path::PathBuf,
    pub(super) spawn_cwd: std::path::PathBuf,
}

impl pa_tui::daemon_reconnect::DaemonLauncher for RuntimeDaemonLauncher {
    fn ensure_running(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            ensure_runtime_daemon_running(&self.daemon_socket, &self.spawn_cwd)
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        })
    }
}
