//! A legacy supervisor on the old wire (`daemon.sock`): the daemon an earlier release left
//! running, which a launch on the runtime daemon finds and stops once it is idle
//! (`retire_idle_supervisor` in pa-cli `interactive_mode/daemon.rs`). It answers what that
//! launch sends it, built from pa-types' old wire types: the `daemon_hello` each connection
//! opens with, `list` (its sessions' rows, which the launch reads for an idle probe), and
//! `shutdown`, after which it tells every client `daemon_closing` and stops listening. Any
//! other command is refused. It runs on threads of the test process and keeps the type of
//! every command it was sent.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pa_types::daemon::{
    DaemonClosingReason, DaemonCommandWire, DaemonOutbound, DaemonProtocolInfo, DaemonResponse,
    DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION, DAEMON_SCHEMA_ID, DAEMON_SCHEMA_REVISION,
};
use serde_json::{json, Value};

/// What the supervisor was sent, and whether it stopped.
#[derive(Default)]
struct State {
    commands: Vec<String>,
    clients: Vec<UnixStream>,
    stopped: bool,
}

/// The shared state and the signal of its changes.
type Shared = Arc<(Mutex<State>, Condvar)>;

/// A legacy supervisor listening on its socket until a `shutdown` stops it.
pub struct LegacySupervisor {
    socket: PathBuf,
    shared: Shared,
}

impl LegacySupervisor {
    /// Listens on `socket` (listening once this returns); `sessions` are the rows its `list`
    /// answers.
    ///
    /// # Panics
    ///
    /// The socket cannot be bound.
    #[must_use]
    pub fn start(socket: &Path, sessions: Vec<Value>) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the legacy supervisor's socket");
        let shared: Shared = Arc::default();
        let accepting = Arc::clone(&shared);
        let path = socket.to_path_buf();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                if accepting.0.lock().unwrap().stopped {
                    return;
                }
                let (shared, sessions, path) =
                    (Arc::clone(&accepting), sessions.clone(), path.clone());
                std::thread::spawn(move || serve(&stream, &shared, &sessions, &path));
            }
        });
        LegacySupervisor {
            socket: socket.to_path_buf(),
            shared,
        }
    }

    /// The type of every command it was sent, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<String> {
        self.shared.0.lock().unwrap().commands.clone()
    }

    /// Waits up to `wait` for a `shutdown` to stop it; whether one did.
    #[must_use]
    pub fn wait_stopped(&self, wait: Duration) -> bool {
        let (state, changed) = &*self.shared;
        let deadline = Instant::now() + wait;
        let mut state = state.lock().unwrap();
        while !state.stopped {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = changed.wait_timeout(state, left).unwrap().0;
        }
        true
    }
}

impl Drop for LegacySupervisor {
    fn drop(&mut self) {
        stop(&self.shared, &self.socket);
    }
}

/// Stops listening: the socket file goes, and the accept loop ends on the connect that wakes it.
fn stop(shared: &Shared, socket: &Path) {
    let mut state = shared.0.lock().unwrap();
    if std::mem::replace(&mut state.stopped, true) {
        return;
    }
    for client in state.clients.drain(..) {
        let _ = client.shutdown(std::net::Shutdown::Both);
    }
    drop(state);
    let _ = UnixStream::connect(socket);
    let _ = std::fs::remove_file(socket);
    shared.1.notify_all();
}

/// Writes `frame` as one line.
fn send(mut stream: &UnixStream, frame: &DaemonOutbound) {
    let mut line = serde_json::to_string(frame).unwrap();
    line.push('\n');
    let _ = stream.write_all(line.as_bytes());
}

/// One client's connection: the hello, then an answer per command line.
fn serve(stream: &UnixStream, shared: &Shared, sessions: &[Value], socket: &Path) {
    let protocol = DaemonProtocolInfo {
        name: DAEMON_PROTOCOL_NAME.to_string(),
        version: DAEMON_PROTOCOL_VERSION,
    };
    send(
        stream,
        &DaemonOutbound::DaemonHello {
            socket_path: socket.display().to_string(),
            protocol,
            schema_id: Some(DAEMON_SCHEMA_ID.to_string()),
            schema_revision: Some(DAEMON_SCHEMA_REVISION),
            app_version: Some("0.0.0-legacy".to_string()),
            runtime: None,
            supervisor_generation: Some("legacy".to_string()),
            supervisor_pid: Some(u64::from(std::process::id())),
            supervisor_owner_token: None,
            supervisor_process_start_id: None,
            supervisor_socket_path: Some(socket.display().to_string()),
            update_resume: None,
            daemon_death: None,
            keeper_pid: None,
            transcript_store_fallback: None,
            client_id: "legacy-client".to_string(),
            server_capabilities: Vec::new(),
            rest: serde_json::Map::default(),
        },
    );
    shared
        .0
        .lock()
        .unwrap()
        .clients
        .push(stream.try_clone().unwrap());
    for line in BufReader::new(stream.try_clone().unwrap()).lines() {
        let Ok(line) = line else { return };
        let Ok(wire) = serde_json::from_str::<DaemonCommandWire>(&line) else {
            continue;
        };
        let (id, command) = match wire {
            DaemonCommandWire::Envelope(envelope) => (Some(envelope.id), envelope.command),
            DaemonCommandWire::Command(command) => (None, command),
        };
        let kind = command.type_name().to_string();
        shared.0.lock().unwrap().commands.push(kind.clone());
        let answered = |data: Option<Value>, error: Option<&str>| DaemonOutbound::Response {
            response: DaemonResponse {
                id: id.clone(),
                command: kind.clone(),
                success: error.is_none(),
                data,
                error: error.map(str::to_string),
                error_info: None,
            },
        };
        match kind.as_str() {
            "list" => send(
                stream,
                &answered(Some(json!({ "sessions": sessions })), None),
            ),
            "shutdown" => {
                send(stream, &answered(None, None));
                let closing = DaemonOutbound::DaemonClosing {
                    reason: DaemonClosingReason::Shutdown,
                    rest: serde_json::Map::default(),
                };
                for client in &shared.0.lock().unwrap().clients {
                    send(client, &closing);
                }
                stop(shared, socket);
                return;
            }
            _ => send(
                stream,
                &answered(
                    None,
                    Some("the legacy supervisor stub does not answer this"),
                ),
            ),
        }
    }
}
