//! Which daemon an update restart drives. The successor is the runtime
//! daemon on `daemon2.sock`, the only daemon this release starts. The
//! daemon the update prepares and stops is the one it finds running
//! ([`Endpoint::running`]): the runtime daemon, or the supervisor on its
//! socket (the old wire) that a release from before this one left running,
//! whose update calls and queries answer the old responses. Its sessions are
//! then restored on the runtime daemon. That old-wire client is pa-tui's
//! (`pa_tui::daemon_client`, built on `pa_types::daemon`); no supervisor code
//! is linked for it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use pa_types::daemon::{DaemonCommand, DaemonResponse};
use serde_json::{json, Value};

/// How long [`Endpoint::running`] waits for a daemon's socket to accept.
const ANSWER_PROBE: Duration = Duration::from_millis(250);

/// The daemon an update restart drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A supervisor an earlier release left running on its socket.
    Supervisor(PathBuf),
    /// The runtime daemon on `socket`, started with `--daemon-socket
    /// daemon_socket` (the supervisor's socket it derives its own from).
    Runtime {
        daemon_socket: PathBuf,
        socket: PathBuf,
    },
}

impl Endpoint {
    /// The runtime daemon beside the supervisor's `daemon_socket`.
    #[must_use]
    pub fn runtime(daemon_socket: &Path) -> Self {
        Endpoint::Runtime {
            daemon_socket: daemon_socket.to_path_buf(),
            socket: pa_runtime::daemon::socket_path(daemon_socket),
        }
    }

    /// The daemon an update finds holding the sessions, which it prepares
    /// and stops: the runtime daemon when it answers, else the supervisor on
    /// `daemon_socket` when it answers (one a release from before this one
    /// left running, whose sessions the update then restores on the runtime
    /// daemon), else the runtime daemon (an update with no daemon to stop).
    pub async fn running(daemon_socket: &Path) -> Self {
        let runtime = Self::runtime(daemon_socket);
        if pa_core::platform::daemon_socket::can_connect(runtime.socket(), ANSWER_PROBE).await
            || !pa_core::platform::daemon_socket::can_connect(daemon_socket, ANSWER_PROBE).await
        {
            runtime
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The update stops the daemon that holds the sessions: the supervisor when only it answers
    /// (one an earlier release left running), else the runtime daemon.
    #[tokio::test]
    async fn the_running_daemon_is_the_one_that_answers_preferring_the_runtime_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_socket = dir.path().join("daemon.sock");
        let runtime = Endpoint::runtime(&daemon_socket);
        let mut answers = vec![("nothing answers", Endpoint::running(&daemon_socket).await)];
        let _supervisor = std::os::unix::net::UnixListener::bind(&daemon_socket).unwrap();
        answers.push((
            "only the supervisor answers",
            Endpoint::running(&daemon_socket).await,
        ));
        let _runtime = std::os::unix::net::UnixListener::bind(runtime.socket()).unwrap();
        answers.push(("both answer", Endpoint::running(&daemon_socket).await));
        assert_eq!(
            answers,
            vec![
                ("nothing answers", runtime.clone()),
                (
                    "only the supervisor answers",
                    Endpoint::Supervisor(daemon_socket.clone())
                ),
                ("both answer", runtime),
            ]
        );
    }
}
