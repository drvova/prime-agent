//! `--mode daemon`: the supervisor process. The interactive client spawns this
//! mode (detached) when no daemon is listening.

use anyhow::Result;

use crate::config;

pub fn run_daemon_mode(daemon_socket: Option<&str>) -> Result<i32> {
    let socket_path = config::resolve_daemon_socket_path(daemon_socket);
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
