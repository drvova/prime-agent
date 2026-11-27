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
/// the client gave it with the options it chose over the daemon's own (the
/// launch flags a client forwards: model, thinking level, system prompt,
/// resources, autonomous run, goal, API key, credential, profile, telemetry
/// switch), and streams the turn driver's events; each subagent it spawns is
/// built as the print run builds its children.
///
/// As the old worker's create does, a session under a profile reads the
/// profile's settings overlay, and a new session's header is pinned to its
/// credential (`AuthAccounts::new_session_pin`: the profile's mapping for its
/// provider, else `credential`, else its parent's pin) and tagged with its
/// profile, so every later build restores the pin as-is. A session
/// created under a parent names it in its header and runs at its depth. A
/// session created with an API key the daemon no longer holds (a later
/// daemon's revival of it) fails rather than run on another key.
struct DaemonSessions {
    options: RunOptions,
    faux: Option<serde_json::Value>,
    /// The script every subagent of a scripted session runs when the
    /// session's script names none for it (`PRIME_AGENT_FAUX_CHILD_SCRIPT`,
    /// the supervisor's `childScript` seam; never set by the product).
    child_faux: Option<serde_json::Value>,
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
                self.child_faux.as_ref(),
                id,
                spec,
                subagent,
                seams,
            )
            .await;
        }
        let mut options = self.options.clone();
        options.config.cwd.clone_from(&spec.cwd);
        let chosen = &spec.options;
        let mode = match chosen.execution_mode.as_deref() {
            None | Some("daemon") => crate::mode::AppMode::Daemon,
            Some("acp") => crate::mode::AppMode::Acp,
            // The terminal UI's sessions (its create's `executionMode`), stamped on their events
            // as the old supervisor's are.
            Some("interactive") => crate::mode::AppMode::Interactive,
            Some(other) => {
                anyhow::bail!("unknown execution mode {other:?} (acp, daemon, interactive)")
            }
        };
        anyhow::ensure!(
            chosen.api_key.is_some() || !chosen.api_key_given,
            "session {id} was created with an API key, which the daemon keeps in memory only: \
             open it again from a client that gives the key"
        );
        if let Some(key) = &chosen.api_key {
            options.config.api_key = Some(key.expose().to_string());
        }
        if let Some(profile) = &chosen.profile {
            // The profile's overlay for every settings read of this lifetime (its scope's).
            pa_core::settings::set_active_profile(Some(profile.clone()));
            options.config.profile = Some(profile.clone());
        }
        if chosen.credential.is_some() {
            options.config.credential.clone_from(&chosen.credential);
        }
        options.config.telemetry_disabled |= chosen.telemetry_disabled;
        if chosen.provider.is_some() {
            options.config.provider.clone_from(&chosen.provider);
        }
        if chosen.model.is_some() {
            options.config.model.clone_from(&chosen.model);
        }
        if chosen.thinking.is_some() {
            options.config.thinking = chosen.thinking;
        }
        if chosen.system_prompt.is_some() {
            options
                .config
                .system_prompt
                .clone_from(&chosen.system_prompt);
        }
        options
            .config
            .append_system_prompt
            .extend(chosen.append_system_prompt.iter().cloned());
        options.config.skills.extend(chosen.skills.iter().cloned());
        options
            .config
            .prompt_templates
            .extend(chosen.prompt_templates.iter().cloned());
        let transcript = spec.transcript.clone().unwrap_or_else(|| {
            pa_core::session::manager::get_session_file_path(
                &super::replacement_session_dir(&options),
                id,
            )
        });
        let session_dir = transcript
            .parent()
            .map_or_else(|| super::replacement_session_dir(&options), PathBuf::from);
        // An ephemeral session (`--no-session`) keeps its transcript in
        // memory only.
        let manager = if spec.ephemeral {
            let mut manager = pa_core::session::manager::SessionManager::in_memory(&spec.cwd);
            manager.new_session(&pa_core::session::manager::NewSessionOptions {
                id: Some(id.to_string()),
                ..Default::default()
            });
            manager
        } else if pa_core::session::store::store().exists(&transcript) {
            pa_core::session::manager::SessionManager::open(&spec.cwd, &session_dir, &transcript)
        } else {
            // The new session's pin (the handler's create refused one that does not resolve).
            let credential_pin = pa_core::auth::AuthAccounts::create(&options.config.agent_dir)
                .new_session_pin(
                    chosen.profile.as_deref(),
                    chosen.provider.as_deref(),
                    chosen.credential.as_deref(),
                    chosen
                        .parent
                        .as_ref()
                        .map(|parent| std::path::Path::new(&parent.path)),
                )
                .map_err(anyhow::Error::msg)?;
            let mut manager =
                pa_core::session::manager::SessionManager::persisted(&spec.cwd, &session_dir);
            manager.new_session(&pa_core::session::manager::NewSessionOptions {
                id: Some(id.to_string()),
                parent_session: chosen.parent.as_ref().map(|parent| parent.path.clone()),
                rlm_depth: chosen
                    .parent
                    .as_ref()
                    .map(|parent| u64::from(parent.rlm_depth)),
                credential_pin,
                profile: chosen.profile.clone(),
            });
            manager
        };
        let headless = super::build_engine(
            &options,
            self.faux.as_ref(),
            Some(manager),
            mode.as_str(),
            &super::HeadlessEmbedding::Root {
                seams,
                faux_lane: Some(id.to_string()),
                rlm_depth: chosen.parent.as_ref().map(|parent| parent.rlm_depth),
            },
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let engine = Arc::new(headless.engine);
        // A fresh branch starts the chosen goal; a resumed one keeps its own.
        if let Some(goal) = &chosen.goal {
            engine
                .seed_initial_goal(&goal.objective, goal.token_budget)
                .await
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        }
        // The session's requests carry its service tier, as the old worker's
        // create applies it: its last tier row (else the settings' default),
        // clamped to the model. A switch moves it from there.
        let preference = {
            let persistence = engine.session.shared_persistence();
            let manager = persistence.lock().await;
            manager.active_context().service_tier
        };
        let preference = preference.or_else(|| {
            Some(
                pa_core::settings::SettingsManager::create(&spec.cwd, &options.config.agent_dir)
                    .get_default_service_tier(),
            )
        });
        if let Some(target) = headless
            .provider_target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            target.service_tier =
                pa_types::ai::clamp_service_tier(Some(&headless.model), preference);
        }
        Ok(BuiltSession {
            engine,
            model: headless.model,
            api_key: headless.api_key,
            provider_target: headless.provider_target,
            agent_dir: options.config.agent_dir.clone(),
            // Its clients read the old daemon's session-event payloads.
            events: DriverEvents::SessionEvents,
            autonomous: chosen.autonomous.clone(),
            // A daemon session's modes follow the settings as they change.
            queue_modes: pa_runtime::QueueModes::Settings,
        })
    }
}

/// Runs the runtime daemon on the agent dir until a stop signal or a
/// client's `daemon.shutdown` / `daemon.restart`: takes the
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
    let child_faux =
        super::script_env("PRIME_AGENT_FAUX_CHILD_SCRIPT").map_err(anyhow::Error::msg)?;
    let socket = pa_runtime::daemon::socket_path(daemon_socket);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // The stop signals are this process's before anything can stop it on purpose: from
        // the lock on, a SIGTERM or SIGINT is the drain it was sent for, however early, never
        // the default death its restart owner would start the daemon again after.
        #[cfg(unix)]
        let (mut terminate, mut interrupt) = {
            use tokio::signal::unix::{signal, SignalKind};
            (
                signal(SignalKind::terminate()).ok(),
                signal(SignalKind::interrupt()).ok(),
            )
        };
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
                socket: socket.clone(),
                // An update's successor: the roster the coordinator spawned it
                // with, read before its boot sweep deletes the file.
                update: pa_runtime::daemon::UpdateBoot {
                    roster: pa_core::update::restart::restore::consume_roster_env(),
                    keeper_pid: pa_daemon::keeper::keeper_pid(),
                    app_version: crate::config::version().to_string(),
                },
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
                // A stop on purpose is not restarted by the restart owner,
                // even one that ends in a kill: its intent is recorded first,
                // as the old daemon records it.
                stopping: {
                    let (agent_dir, socket) = (agent_dir.clone(), socket.clone());
                    Box::new(move || {
                        if pa_daemon::keeper::keeper_pid().is_none() {
                            return;
                        }
                        if let Err(error) =
                            pa_daemon::keeper::record_stop_intent(&agent_dir, &socket)
                        {
                            eprintln!(
                                "prime-agent: the stop intent for the restart owner could not be recorded: {error:#}"
                            );
                        }
                    })
                },
                agent_dir,
                builder: DaemonSessions {
                    options: options.clone(),
                    faux,
                    child_faux,
                },
            },
            async {
                #[cfg(unix)]
                {
                    let received =
                        async |stream: Option<&mut tokio::signal::unix::Signal>| match stream {
                            Some(stream) => {
                                stream.recv().await;
                            }
                            None => std::future::pending().await,
                        };
                    tokio::select! {
                        () = received(interrupt.as_mut()) => {}
                        () = received(terminate.as_mut()) => {}
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
