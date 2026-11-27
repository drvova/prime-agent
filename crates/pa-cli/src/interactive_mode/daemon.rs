//! The daemon-ensure concern: the socket probe, the stale-daemon shutdown, the
//! detached daemon spawn, and the startup poll window.

use anyhow::Context as _;

use super::{anyhow, Command, Duration, Instant, Path, Result, Stdio};

const DAEMON_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DAEMON_SHUTDOWN_WAIT_MS: u64 = 5_000;
/// Pause between daemon-startup probes (TS `ensureDaemonRunning` polls at
/// 25ms). A cold supervisor binds its socket ~39ms after the spawn, so the
/// poll interval quantizes every cold launch: the bind lands 0-interval
/// before the next probe, a pure wait on a floor of microseconds. The 1ms
/// interval caps that overshoot at <=1ms for the cost of ~40 failed
/// connects per cold launch (each ~us; bounded by the same 30s startup
/// budget, so a hung boot adds at most ~1k connects/s of syscall work,
/// not a spin). Overshoot tables: probe-grid record 20261001-034500 on the
/// bench repo. Timing-only: the probe itself, the 30s startup budget, and
/// the timeout error are unchanged.
const DAEMON_PROBE_INTERVAL_MS: u64 = 1;

enum DaemonProbe {
    Absent,
    Current,
    /// A supervisor answered with a different protocol/schema. The client rides
    /// boxed: its size is platform-dependent, so the box keeps the other arms small.
    Stale(Box<pa_tui::daemon_client::DaemonClient>),
}

/// Probe the socket once: connect, read the hello, and classify it.
async fn probe_daemon(socket_path: &Path) -> DaemonProbe {
    let Ok((client, _events)) = pa_tui::daemon_client::DaemonClient::connect(socket_path).await
    else {
        return DaemonProbe::Absent;
    };
    let hello = client.hello();
    let current = hello.get("protocol").and_then(|p| p.get("version"))
        == Some(&serde_json::json!(
            pa_types::daemon::DAEMON_PROTOCOL_VERSION
        ))
        && hello.get("schemaId") == Some(&serde_json::json!(pa_types::daemon::DAEMON_SCHEMA_ID));
    if current {
        client.close();
        DaemonProbe::Current
    } else {
        DaemonProbe::Stale(Box::new(client))
    }
}

/// Ensure a current daemon is listening on `socket_path`, spawning this
/// executable in `--mode daemon` when it is not (a stale idle daemon is
/// shut down, a busy one refuses replacement).
///
/// # Errors
/// Returns an error when the executable path cannot be resolved, a stale
/// daemon refuses replacement, the supervisor cannot be spawned, or no
/// current daemon starts before the startup timeout.
pub async fn ensure_daemon_running(socket_path: &Path, spawn_cwd: &Path) -> Result<()> {
    match probe_daemon(socket_path).await {
        DaemonProbe::Current => return Ok(()),
        DaemonProbe::Stale(client) => shutdown_stale_daemon(*client, socket_path).await?,
        DaemonProbe::Absent => {}
    }
    let exe = std::env::current_exe().context("resolve the prime-agent executable")?;
    ensure_daemon_running_with(&exe, socket_path, spawn_cwd).await
}

/// [`ensure_daemon_running`] with an explicit supervisor executable (the
/// product path uses this process's own binary).
///
/// # Errors
/// Returns an error when the supervisor cannot be spawned or no current
/// daemon starts before the startup timeout.
pub async fn ensure_daemon_running_with(
    exe: &Path,
    socket_path: &Path,
    spawn_cwd: &Path,
) -> Result<()> {
    match probe_daemon(socket_path).await {
        DaemonProbe::Current => return Ok(()),
        DaemonProbe::Stale(client) => {
            // A stale daemon appeared between the caller's check and here:
            // fall through to the spawn path after refusing busy ones.
            client.close();
        }
        DaemonProbe::Absent => {}
    }
    spawn_daemon_detached(socket_path, spawn_cwd, exe)?;
    let deadline = Instant::now() + Duration::from_millis(DAEMON_STARTUP_TIMEOUT_MS);
    loop {
        match probe_daemon(socket_path).await {
            DaemonProbe::Current => return Ok(()),
            DaemonProbe::Stale(client) => {
                // A concurrent launcher won the socket with a build whose protocol matches at
                // connect time but failed the schema check: re-probe before deciding.
                client.close();
            }
            DaemonProbe::Absent => {}
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Timed out waiting for the Prime Agent daemon to start on {}. Run: prime-agent shutdown --force, then retry the original command.",
                socket_path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(DAEMON_PROBE_INTERVAL_MS)).await;
    }
}

/// Ensure the runtime daemon is listening beside `daemon_socket` (its
/// `daemon2.sock`), spawning this executable in `--mode daemon` on
/// `daemon_socket` when it is not: the spawned process inherits this one's
/// environment, so it is the runtime daemon the environment or the
/// settings selected. Answers the runtime daemon's socket.
///
/// # Errors
/// Returns an error when the executable path cannot be resolved, the daemon
/// cannot be spawned, or its socket does not answer before the startup
/// timeout.
pub async fn ensure_runtime_daemon_running(
    daemon_socket: &Path,
    spawn_cwd: &Path,
) -> Result<std::path::PathBuf> {
    let socket = pa_runtime::daemon::socket_path(daemon_socket);
    let probe = Duration::from_millis(250);
    if pa_daemon::socket::can_connect(&socket, probe).await {
        return Ok(socket);
    }
    let exe = std::env::current_exe().context("resolve the prime-agent executable")?;
    spawn_daemon_detached(daemon_socket, spawn_cwd, &exe)?;
    let deadline = Instant::now() + Duration::from_millis(DAEMON_STARTUP_TIMEOUT_MS);
    loop {
        if pa_daemon::socket::can_connect(&socket, probe).await {
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

/// Shut a stale daemon down when no session is busy; a busy one refuses
/// replacement.
async fn shutdown_stale_daemon(
    client: pa_tui::daemon_client::DaemonClient,
    socket_path: &Path,
) -> Result<()> {
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
    let busy = sessions.map_or(true, |data| {
        data.get("sessions")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|rows| {
                rows.iter()
                    .any(|row| row.get("isSessionActive") == Some(&serde_json::json!(true)))
            })
    });
    client.close();
    if busy {
        return Err(anyhow!(
            "An incompatible Prime Agent daemon is running on {}.\n\nRun:\n  prime-agent shutdown --force\n\nThen retry the original command (the running daemon has active work).",
            socket_path.display()
        ));
    }
    if let Ok((client, _)) = pa_tui::daemon_client::DaemonClient::connect(socket_path).await {
        let _ = client
            .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
                id: None,
                force: None,
                rest: serde_json::Map::default(),
            })
            .await;
        client.close();
    }
    wait_for_socket_gone(socket_path).await;
    Ok(())
}

/// Wait until nothing accepts connections on the socket (bounded).
async fn wait_for_socket_gone(socket_path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_millis(DAEMON_SHUTDOWN_WAIT_MS);
    while Instant::now() < deadline {
        if !pa_daemon::socket::can_connect(socket_path, Duration::from_millis(250)).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
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

/// Spawn a detached daemon on `socket_path` (the child outlives this CLI): the daemon the
/// environment or the settings select, which the caller waits for on that daemon's socket.
fn spawn_daemon_detached(socket_path: &Path, spawn_cwd: &Path, exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket_path)
        .current_dir(spawn_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The update successor's override is never passed on to a daemon this launch selects.
    command.env_remove(crate::daemon_mode::SUPERVISOR_ENV);
    // The spawned process is the daemon's restart owner: it starts the daemon with these same
    // arguments and starts it again when it dies abnormally. Unix only: the owner forwards
    // signals and reads the daemon's terminating signal, neither of which exists on Windows,
    // where the daemon is started unkept as before.
    #[cfg(unix)]
    command.env(pa_daemon::keeper::KEEPER_ENV, "1");
    // The binary entry strips any inherited worker handshake env, so the
    // spawned supervisor never starts in worker mode. A daemon must also not
    // share the launching TUI's terminal session: a session-wide terminal
    // cleanup could hang it up after the TUI exits; on Unix, setsid also
    // creates its own process group.
    #[cfg(unix)]
    pa_core::platform::process::set_new_session(&mut command);
    #[cfg(not(unix))]
    pa_core::platform::process::set_new_process_group(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn the Prime Agent daemon on {}", socket_path.display()))?;
    Ok(())
}
