//! `--mode daemon`: the daemon process. The interactive client spawns this
//! mode (detached) when no daemon is listening. It is the runtime daemon
//! (wire2, on `daemon2.sock` beside the daemon socket it is given), whatever
//! the settings or the environment say: a settings file of an earlier
//! release that still says `"daemon": "supervisor"` loads and is ignored
//! (the daemon log says so at each boot), and the old selection switches
//! (`PRIME_AGENT_RUNTIME_DAEMON`, `PRIME_AGENT_DAEMON_SUPERVISOR`) select
//! nothing.

use anyhow::Result;

use crate::config;
use crate::mode::RunOptions;

pub fn run_daemon_mode(options: &RunOptions) -> Result<i32> {
    let daemon_socket = config::resolve_daemon_socket_path(options.daemon_socket.as_deref());
    let agent_dir = config::get_agent_dir();
    // The daemon keeps its transcripts in this agent dir's database.
    pa_core::session::store::use_agent_dir(&agent_dir);
    // Asked to be the restart owner: keep the daemon this command line starts instead of being
    // it, with the records of the runtime daemon's socket. Nothing of the session engine is
    // built on this path.
    #[cfg(unix)]
    if std::env::var_os(pa_runtime::daemon::keeper::KEEPER_ENV).is_some() {
        return pa_runtime::daemon::keeper::run_keeper(
            &pa_runtime::daemon::keeper::KeeperOptions {
                exe: std::env::current_exe()?,
                args: std::env::args_os().skip(1).collect(),
                socket_path: pa_runtime::daemon::socket_path(&daemon_socket),
                agent_dir,
            },
        );
    }
    crate::print_runtime::daemon_session::run_runtime_daemon(options, &daemon_socket, agent_dir)
}
