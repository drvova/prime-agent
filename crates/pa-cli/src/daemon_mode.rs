//! `--mode daemon`: the supervisor process. The interactive client spawns this
//! mode (detached) when no daemon is listening. With the runtime daemon
//! selected (`PRIME_AGENT_RUNTIME_DAEMON=1`, or `"daemon": "runtime"` in the
//! global settings) it is the runtime daemon instead, on its own socket.

use anyhow::Result;

use crate::config;
use crate::mode::RunOptions;

/// The environment switch that selects the runtime daemon.
const RUNTIME_DAEMON_ENV: &str = "PRIME_AGENT_RUNTIME_DAEMON";

pub fn run_daemon_mode(options: &RunOptions) -> Result<i32> {
    let socket_path = config::resolve_daemon_socket_path(options.daemon_socket.as_deref());
    let agent_dir = config::get_agent_dir();
    // The daemon keeps its transcripts in this agent dir's database.
    pa_core::session::store::use_agent_dir(&agent_dir);
    // Asked to be the restart owner: keep the daemon this command line starts instead of being
    // it. Nothing of the session engine is built on this path.
    #[cfg(unix)]
    if std::env::var_os(pa_daemon::keeper::KEEPER_ENV).is_some() {
        return pa_daemon::keeper::run_keeper(&pa_daemon::keeper::KeeperOptions {
            exe: std::env::current_exe()?,
            args: std::env::args_os().skip(1).collect(),
            socket_path,
            agent_dir,
        });
    }
    // Process startup: the selection is read once, from the process's own
    // environment and the global settings.
    let runtime_selected = std::env::var(RUNTIME_DAEMON_ENV).is_ok_and(|value| value == "1")
        || pa_core::settings::SettingsManager::create(&agent_dir, &agent_dir)
            .global_settings()
            .daemon
            .as_deref()
            == Some("runtime");
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
