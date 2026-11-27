//! Which daemon an update restart drives: the runtime daemon on
//! `daemon2.sock` (the default), or the supervisor on its socket (the old
//! wire) when it is selected (`"daemon": "supervisor"` or
//! `PRIME_AGENT_RUNTIME_DAEMON=0`), whose update calls and queries answer
//! the old responses (pa-tui's `wire2_compat` maps the commands and replies).
//! The coordinator's phases go to the daemon it finds running
//! ([`Endpoint::running`]), and the successor's spawn and the restore report
//! to the selected one, so an update restores its sessions on the selected
//! daemon, also when the daemon that held them was the other one (the
//! supervisor of a release from before the runtime daemon was the default).

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
        if crate::daemon_mode::DaemonKind::selected(agent_dir)
            == crate::daemon_mode::DaemonKind::Runtime
        {
            Endpoint::Runtime {
                daemon_socket: daemon_socket.to_path_buf(),
                socket: pa_runtime::daemon::socket_path(daemon_socket),
            }
        } else {
            Endpoint::Supervisor(daemon_socket.to_path_buf())
        }
    }

    /// The daemon an update finds holding the sessions, which it prepares
    /// and stops: the selected one when it answers, else the other one when
    /// it answers (a daemon the selection has moved away from, such as the
    /// supervisor an upgrade to the runtime default finds running, whose
    /// sessions the update then restores on the selected one), else the
    /// selected one (an update with no daemon to stop).
    pub async fn running(daemon_socket: &Path, agent_dir: &Path) -> Self {
        let selected = Self::selected(daemon_socket, agent_dir);
        let other = match &selected {
            Endpoint::Supervisor(_) => Endpoint::Runtime {
                daemon_socket: daemon_socket.to_path_buf(),
                socket: pa_runtime::daemon::socket_path(daemon_socket),
            },
            Endpoint::Runtime { .. } => Endpoint::Supervisor(daemon_socket.to_path_buf()),
        };
        if pa_daemon::socket::can_connect(selected.socket(), ANSWER_PROBE).await
            || !pa_daemon::socket::can_connect(other.socket(), ANSWER_PROBE).await
        {
            selected
        } else {
            other
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

    /// The update stops the daemon that holds the sessions: the other one when only it answers
    /// (the supervisor an upgrade to the runtime default finds running), else the selected one.
    #[tokio::test]
    async fn the_running_daemon_is_the_one_that_answers_preferring_the_selected() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_socket = dir.path().join("daemon.sock");
        let selected = Endpoint::selected(&daemon_socket, dir.path());
        let other = if selected.is_runtime() {
            daemon_socket.clone()
        } else {
            pa_runtime::daemon::socket_path(&daemon_socket)
        };
        let mut answers = vec![(
            "nothing answers",
            Endpoint::running(&daemon_socket, dir.path()).await,
        )];
        let _other = std::os::unix::net::UnixListener::bind(&other).unwrap();
        answers.push((
            "only the other answers",
            Endpoint::running(&daemon_socket, dir.path()).await,
        ));
        let _selected = std::os::unix::net::UnixListener::bind(selected.socket()).unwrap();
        answers.push((
            "both answer",
            Endpoint::running(&daemon_socket, dir.path()).await,
        ));
        let sockets: Vec<(&str, PathBuf)> = answers
            .into_iter()
            .map(|(case, endpoint)| (case, endpoint.socket().to_path_buf()))
            .collect();
        assert_eq!(
            sockets,
            vec![
                ("nothing answers", selected.socket().to_path_buf()),
                ("only the other answers", other),
                ("both answer", selected.socket().to_path_buf()),
            ]
        );
    }
}
