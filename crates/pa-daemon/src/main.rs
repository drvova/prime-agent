//! `pa-daemon`: supervisor and session-worker binary.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

fn main() -> Result<()> {
    // Same allocator cap as the prime-agent binary: large transient phases
    // (session loads, attach snapshots) must not leave per-arena high-water
    // pages resident.
    pa_types::memory_release::cap_thread_arenas();
    // Still single-threaded: the restart owner's hand-off leaves the environment.
    pa_daemon::keeper::adopt_host_env();
    // Before the runtime's threads exist: see `pin_scheme_home`.
    pa_core::kernel::pin_scheme_home();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| {
        eprintln!("usage: pa-daemon <supervisor|worker> [options]");
        std::process::exit(2);
    });
    match command.as_str() {
        "supervisor" => {
            let mut socket_path = None;
            let mut agent_dir = None;
            let mut args = args.peekable();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--socket" => socket_path = args.next().map(PathBuf::from),
                    "--agent-dir" => agent_dir = args.next().map(PathBuf::from),
                    other => return Err(anyhow!("unknown supervisor option: {other}")),
                }
            }
            let agent_dir = match agent_dir {
                Some(dir) => dir,
                None => pa_daemon::paths::agent_dir()?,
            };
            let options = pa_daemon::supervisor::SupervisorOptions {
                socket_path: socket_path
                    .unwrap_or_else(pa_daemon::socket::default_daemon_socket_path),
                agent_dir,
            };
            // Asked to be the restart owner: keep the supervisor this command line starts
            // instead of being it. The blocking section parks this runtime's worker while the
            // keeper's own single-threaded runtime runs.
            #[cfg(unix)]
            if std::env::var_os(pa_daemon::keeper::KEEPER_ENV).is_some() {
                let keeper = pa_daemon::keeper::KeeperOptions {
                    exe: std::env::current_exe()?,
                    args: std::env::args_os().skip(1).collect(),
                    socket_path: options.socket_path,
                    agent_dir: options.agent_dir,
                };
                let code = tokio::task::block_in_place(|| pa_daemon::keeper::run_keeper(&keeper))?;
                std::process::exit(code);
            }
            pa_daemon::supervisor::run_supervisor(options).await
        }
        "worker" => pa_daemon::worker::run_worker().await,
        other => Err(anyhow!(
            "unknown command: {other} (expected supervisor|worker)"
        )),
    }
}
