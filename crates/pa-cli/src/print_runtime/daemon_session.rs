//! The runtime daemon's composition (`pa_runtime::daemon`): its sessions'
//! engines are built through the same headless assembly as the print run's,
//! and its endpoint goes through the old daemon's socket lifecycle (stale
//! socket cleanup, owner-only permissions, the exit cleanup that spares a
//! successor).

use std::path::PathBuf;
use std::sync::Arc;

use pa_runtime::{BuiltSession, DriverEvents, SessionBuilder, SessionSpec};

use crate::mode::RunOptions;

/// The runtime daemon's engine builds: a session opens the transcript its
/// spec names (creating it under its id the first time), runs in the cwd
/// the client gave it, and streams the turn driver's events; each subagent
/// it spawns is built as the print run builds its children.
struct DaemonSessions {
    options: RunOptions,
    faux: Option<serde_json::Value>,
}

impl SessionBuilder for DaemonSessions {
    async fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: pa_runtime::SessionSeams,
    ) -> anyhow::Result<BuiltSession> {
        if let Some(subagent) = &spec.subagent {
            return super::print_session::build_subagent(
                &self.options,
                self.faux.as_ref(),
                id,
                spec,
                subagent,
                seams,
            )
            .await;
        }
        let mut options = self.options.clone();
        options.config.cwd.clone_from(&spec.cwd);
        let transcript = spec.transcript.clone().unwrap_or_else(|| {
            pa_core::session::manager::get_session_file_path(
                &super::replacement_session_dir(&options),
                id,
            )
        });
        let session_dir = transcript
            .parent()
            .map_or_else(|| super::replacement_session_dir(&options), PathBuf::from);
        let manager = if pa_core::session::store::store().exists(&transcript) {
            pa_core::session::manager::SessionManager::open(&spec.cwd, &session_dir, &transcript)
        } else {
            let mut manager =
                pa_core::session::manager::SessionManager::persisted(&spec.cwd, &session_dir);
            manager.new_session(&pa_core::session::manager::NewSessionOptions {
                id: Some(id.to_string()),
                ..Default::default()
            });
            manager
        };
        let headless = super::build_engine(
            &options,
            self.faux.as_ref(),
            Some(manager),
            crate::mode::AppMode::Daemon.as_str(),
            &super::HeadlessEmbedding::Root { seams },
        )
        .await
        .map_err(anyhow::Error::msg)?;
        Ok(BuiltSession {
            engine: Arc::new(headless.engine),
            model: headless.model,
            api_key: headless.api_key,
            agent_dir: options.config.agent_dir.clone(),
            events: DriverEvents::Stream,
            autonomous: None,
            // A daemon session's modes follow the settings as they change.
            queue_modes: pa_runtime::QueueModes::Settings,
        })
    }
}

/// Runs the runtime daemon on the agent dir until a stop signal: takes the
/// agent dir's runtime-daemon lock (reaping a wedged predecessor on the same
/// socket), binds `daemon2.sock` beside
/// `daemon_socket` (the old daemon's stale-socket cleanup first), serves,
/// and removes the socket on the way out unless a successor took it.
///
/// # Errors
///
/// Another runtime daemon runs on the agent dir, the socket cannot be
/// prepared or bound, or the runtime could not be opened.
pub(crate) fn run_runtime_daemon(
    options: &RunOptions,
    daemon_socket: &std::path::Path,
    agent_dir: PathBuf,
) -> anyhow::Result<i32> {
    let faux = super::faux_script_env().map_err(anyhow::Error::msg)?;
    let socket = pa_runtime::daemon::socket_path(daemon_socket);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let lock =
            pa_runtime::daemon::take_lock(&agent_dir, &socket, pa_daemon::keeper::keeper_pid())
                .await?;
        pa_daemon::socket::prepare_socket_path(&socket).await?;
        let listener = pa_types::platform::transport::bind_transport(&socket)
            .await
            .map_err(|error| error.context(format!("bind {}", socket.display())))?;
        let identity = pa_daemon::socket::socket_identity(&socket);
        pa_daemon::socket::restrict_socket_path(&socket);
        let served = pa_runtime::daemon::run(
            listener,
            pa_runtime::daemon::DaemonOptions {
                session_dir: super::replacement_session_dir(options),
                cwd: options.config.cwd.clone(),
                selected: {
                    let (by_env, agent_dir) = (
                        crate::daemon_mode::env_selects_runtime_daemon(),
                        agent_dir.clone(),
                    );
                    Arc::new(move || {
                        by_env || crate::daemon_mode::settings_select_runtime_daemon(&agent_dir)
                    })
                },
                agent_dir,
                builder: DaemonSessions {
                    options: options.clone(),
                    faux,
                },
            },
            async {
                #[cfg(unix)]
                {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .ok();
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        () = async {
                            match terminate.as_mut() {
                                Some(terminate) => { terminate.recv().await; }
                                None => std::future::pending().await,
                            }
                        } => {}
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                }
            },
        )
        .await;
        pa_daemon::socket::cleanup_socket_path_after_close(&socket, identity);
        drop(lock);
        served
    })?;
    Ok(0)
}
