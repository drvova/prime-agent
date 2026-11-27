//! `--mode daemon`: the supervisor process. The interactive client spawns this
//! mode (detached) when no daemon is listening. With the runtime daemon
//! selected (`PRIME_AGENT_RUNTIME_DAEMON=1`, or `"daemon": "runtime"` in the
//! global settings) it is the runtime daemon instead, on its own socket,
//! unless its launcher waits on the supervisor's socket and says so
//! ([`SUPERVISOR_ENV`]).

use anyhow::Result;

use crate::config;
use crate::mode::RunOptions;

/// The environment switch that selects the runtime daemon.
pub(crate) const RUNTIME_DAEMON_ENV: &str = "PRIME_AGENT_RUNTIME_DAEMON";

/// The internal override a launcher that waits on the supervisor's socket
/// (`daemon.sock`) sets on the `--mode daemon` process it spawns: that process
/// is the supervisor whatever the runtime-daemon opt-in says. The interactive
/// TUI and the update coordinator's successor speak the old wire only until the
/// TUI moves to wire2, so the opt-in must not turn their daemon into the
/// runtime daemon on `daemon2.sock`. A restart owner passes it on to the daemon
/// it keeps, which inherits its environment.
pub(crate) const SUPERVISOR_ENV: &str = "PRIME_AGENT_DAEMON_SUPERVISOR";

/// Whether the process's own environment selects the runtime daemon; read
/// once, at startup.
pub(crate) fn env_selects_runtime_daemon() -> bool {
    std::env::var(RUNTIME_DAEMON_ENV).is_ok_and(|value| value == "1")
}

/// Whether the agent dir's global settings select the runtime daemon, as
/// they stand now: read at startup to pick the daemon, and by the runtime
/// daemon before each scheduled job, so a job never fires in a runtime
/// daemon the settings stopped selecting.
pub(crate) fn settings_select_runtime_daemon(agent_dir: &std::path::Path) -> bool {
    pa_core::settings::SettingsManager::create(agent_dir, agent_dir)
        .global_settings()
        .daemon
        .as_deref()
        == Some("runtime")
}

pub fn run_daemon_mode(options: &RunOptions) -> Result<i32> {
    let socket_path = config::resolve_daemon_socket_path(options.daemon_socket.as_deref());
    let agent_dir = config::get_agent_dir();
    // The daemon keeps its transcripts in this agent dir's database.
    pa_core::session::store::use_agent_dir(&agent_dir);
    // Process startup: which daemon this process is. A launcher that waits on the
    // supervisor's socket outranks the opt-in.
    let runtime_selected = std::env::var_os(SUPERVISOR_ENV).is_none()
        && (env_selects_runtime_daemon() || settings_select_runtime_daemon(&agent_dir));
    // Asked to be the restart owner: keep the daemon this command line starts instead of being
    // it, with the records of the daemon it keeps. Nothing of the session engine is built on
    // this path.
    #[cfg(unix)]
    if std::env::var_os(pa_daemon::keeper::KEEPER_ENV).is_some() {
        let (socket_path, daemon) = if runtime_selected {
            (
                pa_runtime::daemon::socket_path(&socket_path),
                pa_daemon::keeper::KeptDaemon::Runtime,
            )
        } else {
            (socket_path, pa_daemon::keeper::KeptDaemon::Supervisor)
        };
        return pa_daemon::keeper::run_keeper(&pa_daemon::keeper::KeeperOptions {
            exe: std::env::current_exe()?,
            args: std::env::args_os().skip(1).collect(),
            socket_path,
            agent_dir,
            daemon,
        });
    }
    if runtime_selected {
        return crate::print_runtime::daemon_session::run_runtime_daemon(
            options,
            &socket_path,
            agent_dir,
        );
    }
    let options = pa_daemon::supervisor::SupervisorOptions {
        socket_path,
        agent_dir,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(pa_daemon::supervisor::run_supervisor(options))?;
    Ok(0)
}
