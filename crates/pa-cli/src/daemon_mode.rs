//! `--mode daemon`: the daemon process. The interactive client spawns this
//! mode (detached) when no daemon is listening. It is the runtime daemon
//! (wire2, on `daemon2.sock` beside the supervisor's socket) unless the
//! supervisor is selected (`"daemon": "supervisor"` in the global settings,
//! or `PRIME_AGENT_RUNTIME_DAEMON=0`, both kept for one release) or a
//! launcher that waits on the supervisor's socket says so ([`SUPERVISOR_ENV`]).

use anyhow::Result;

use crate::config;
use crate::mode::RunOptions;

/// The environment switch that selects a daemon: `1` the runtime daemon,
/// `0` the supervisor; unset (or anything else), the settings choose.
pub(crate) const RUNTIME_DAEMON_ENV: &str = "PRIME_AGENT_RUNTIME_DAEMON";

/// The internal override a launcher that waits on the supervisor's socket
/// sets on the `--mode daemon` process it spawns: the update coordinator's
/// successor (or rollback) while the supervisor is the daemon the update
/// drives, and the interactive launch and the other daemon-backed modes when
/// the supervisor is the selected daemon. That process is the supervisor
/// whatever its own settings say. A restart owner passes it on to the daemon
/// it keeps, which inherits its environment. A launch that waits on the
/// runtime daemon removes it.
pub(crate) const SUPERVISOR_ENV: &str = "PRIME_AGENT_DAEMON_SUPERVISOR";

/// The two daemons `--mode daemon` can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonKind {
    /// The old supervisor on its socket (the old wire).
    Supervisor,
    /// The runtime daemon on `daemon2.sock` (wire2): the default.
    Runtime,
}

impl DaemonKind {
    /// The daemon the process's environment and the agent dir's global
    /// settings select, as they stand now: the environment switch outranks
    /// the settings.
    pub(crate) fn selected(agent_dir: &std::path::Path) -> Self {
        Self::from_env().unwrap_or_else(|| Self::from_settings(agent_dir))
    }

    /// What the environment switch selects, if it selects one.
    pub(crate) fn from_env() -> Option<Self> {
        match std::env::var(RUNTIME_DAEMON_ENV).as_deref() {
            Ok("1") => Some(DaemonKind::Runtime),
            Ok("0") => Some(DaemonKind::Supervisor),
            _ => None,
        }
    }

    /// What the agent dir's global settings select: the supervisor only when
    /// they say `"daemon": "supervisor"`. Read at startup to pick the daemon,
    /// and by the runtime daemon before each scheduled job, so a job never
    /// fires in a runtime daemon the settings stopped selecting.
    pub(crate) fn from_settings(agent_dir: &std::path::Path) -> Self {
        let settings = pa_core::settings::SettingsManager::create(agent_dir, agent_dir);
        if settings.global_settings().daemon.as_deref() == Some("supervisor") {
            DaemonKind::Supervisor
        } else {
            DaemonKind::Runtime
        }
    }
}

pub fn run_daemon_mode(options: &RunOptions) -> Result<i32> {
    let socket_path = config::resolve_daemon_socket_path(options.daemon_socket.as_deref());
    let agent_dir = config::get_agent_dir();
    // The daemon keeps its transcripts in this agent dir's database.
    pa_core::session::store::use_agent_dir(&agent_dir);
    // Process startup: which daemon this process is. A launcher that waits on the
    // supervisor's socket outranks the selection.
    let runtime_selected = std::env::var_os(SUPERVISOR_ENV).is_none()
        && DaemonKind::selected(&agent_dir) == DaemonKind::Runtime;
    // Asked to be the restart owner: keep the daemon this command line starts instead of being
    // it, with the records of the daemon it keeps. Nothing of the session engine is built on
    // this path.
    #[cfg(unix)]
    if std::env::var_os(pa_runtime::daemon::keeper::KEEPER_ENV).is_some() {
        let (socket_path, daemon) = if runtime_selected {
            (
                pa_runtime::daemon::socket_path(&socket_path),
                pa_runtime::daemon::keeper::KeptDaemon::Runtime,
            )
        } else {
            (
                socket_path,
                pa_runtime::daemon::keeper::KeptDaemon::Supervisor {
                    count_sessions: pa_daemon::descriptor::hosted_session_counts,
                },
            )
        };
        return pa_runtime::daemon::keeper::run_keeper(
            &pa_runtime::daemon::keeper::KeeperOptions {
                exe: std::env::current_exe()?,
                args: std::env::args_os().skip(1).collect(),
                socket_path,
                agent_dir,
                daemon,
            },
        );
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
