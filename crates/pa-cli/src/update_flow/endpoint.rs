//! Which daemon an update restart drives: the supervisor on its socket (the
//! old wire), or, when the runtime daemon is the selected daemon
//! (`PRIME_AGENT_RUNTIME_DAEMON=1` or `"daemon": "runtime"`), the runtime
//! daemon on `daemon2.sock` beside it, whose update calls and queries answer
//! the old responses (pa-tui's `wire2_compat` maps the commands and replies).
//! The coordinator's phases, the successor's spawn and the restore report go
//! through it, so an update restores its sessions on the daemon that held
//! them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use pa_types::daemon::{DaemonCommand, DaemonResponse};
use serde_json::{json, Value};

/// The daemon an update restart drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The supervisor on its socket.
    Supervisor(PathBuf),
    /// The runtime daemon on `socket`, started with `--daemon-socket
    /// daemon_socket` (the supervisor's socket it derives its own from).
    Runtime {
        daemon_socket: PathBuf,
        socket: PathBuf,
    },
}

impl Endpoint {
    /// The selected daemon for the supervisor's `daemon_socket` over
    /// `agent_dir`, as `--mode daemon` selects it.
    #[must_use]
    pub fn selected(daemon_socket: &Path, agent_dir: &Path) -> Self {
        if crate::daemon_mode::env_selects_runtime_daemon()
            || crate::daemon_mode::settings_select_runtime_daemon(agent_dir)
        {
            Endpoint::Runtime {
                daemon_socket: daemon_socket.to_path_buf(),
                socket: pa_runtime::daemon::socket_path(daemon_socket),
            }
        } else {
            Endpoint::Supervisor(daemon_socket.to_path_buf())
        }
    }

    /// The socket the daemon serves.
    #[must_use]
    pub fn socket(&self) -> &Path {
        match self {
            Endpoint::Supervisor(socket) | Endpoint::Runtime { socket, .. } => socket,
        }
    }

    /// The socket `--mode daemon` is started with for this daemon.
    #[must_use]
    pub fn daemon_socket(&self) -> &Path {
        match self {
            Endpoint::Supervisor(socket)
            | Endpoint::Runtime {
                daemon_socket: socket,
                ..
            } => socket,
        }
    }

    /// Whether this is the runtime daemon.
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        matches!(self, Endpoint::Runtime { .. })
    }

    /// Connects and greets: the supervisor's `daemon_hello`, or the runtime
    /// daemon's `daemon.identity` (the hello's identity fields).
    ///
    /// # Errors
    ///
    /// Nothing answers on the socket, or the greeting failed.
    pub async fn connect(&self) -> Result<Connection> {
        match self {
            Endpoint::Supervisor(socket) => {
                let (client, _events) =
                    pa_tui::daemon_client::DaemonClient::connect(socket).await?;
                Ok(Connection::Supervisor(client))
            }
            Endpoint::Runtime { socket, .. } => {
                let path = socket.clone();
                let hello = tokio::task::spawn_blocking(move || {
                    crate::runtime_daemon_client::RuntimeDaemonClient::connect(&path)?
                        .query("daemon.identity", json!({}))
                })
                .await
                .context("the runtime daemon greeting's thread panicked")??;
                Ok(Connection::Runtime {
                    socket: socket.clone(),
                    hello,
                })
            }
        }
    }
}

/// A greeted daemon.
pub enum Connection {
    /// The supervisor's client.
    Supervisor(pa_tui::daemon_client::DaemonClient),
    /// The runtime daemon, each request on a connection of its own.
    Runtime { socket: PathBuf, hello: Value },
}

impl Connection {
    /// The daemon's greeting (`supervisorPid`, `supervisorProcessStartId`,
    /// `supervisorGeneration`, `keeperPid`, `appVersion`, `schemaId`, ...).
    #[must_use]
    pub fn hello(&self) -> &Value {
        match self {
            Connection::Supervisor(client) => client.hello(),
            Connection::Runtime { hello, .. } => hello,
        }
    }

    /// Sends `command`, answered within `timeout_ms`, as the old response.
    ///
    /// # Errors
    ///
    /// The command did not reach the daemon or was not answered in time.
    pub async fn request(&self, command: DaemonCommand, timeout_ms: u64) -> Result<DaemonResponse> {
        match self {
            Connection::Supervisor(client) => {
                client.request_with_timeout(command, timeout_ms).await
            }
            Connection::Runtime { socket, .. } => {
                let socket = socket.clone();
                tokio::task::spawn_blocking(move || {
                    let kind = serde_json::to_value(&command)?["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let mut frames = pa_tui::wire2_request_frames("update".to_string(), command)?;
                    let (Some(frame), None) = (frames.pop(), frames.pop()) else {
                        anyhow::bail!("{kind} has no runtime daemon request");
                    };
                    let mut client =
                        crate::runtime_daemon_client::RuntimeDaemonClient::connect(&socket)?;
                    let outcome =
                        client.request(&frame, Duration::from_millis(timeout_ms.max(1)))?;
                    Ok(serde_json::from_value(pa_tui::wire2_response_frame(
                        &kind, outcome,
                    )?)?)
                })
                .await
                .context("the runtime daemon request's thread panicked")?
            }
        }
    }

    /// Closes the connection.
    pub fn close(self) {
        match self {
            Connection::Supervisor(client) => client.close(),
            Connection::Runtime { .. } => {}
        }
    }
}
