//! The legacy greeter: the old supervisor's socket (`daemon.sock`, the old
//! wire), answered by the runtime daemon once no supervisor holds it.
//!
//! A client of an earlier release (an old window reconnecting after an
//! update, a launcher that started `--mode daemon` and waits on the old
//! socket) would otherwise find nothing there, or a daemon that never comes,
//! and wait out its timeouts. The greeter tells it to restart instead, in the
//! old wire's own frames, so it reports the reason:
//!
//! - **Hello.** Each connection is sent a `daemon_hello` first, as the old
//!   supervisor sent it: the old protocol's name and version, this build's
//!   schema and version, and `legacyGreeter` (the reason and the runtime
//!   daemon's socket). It names no supervisor process, so nothing reads it
//!   as a daemon to stop; this build's discovery skips a greeter.
//! - **Refusal.** The first line the client sends is answered with a failed
//!   `response` ([`RESTART_REFUSAL`]), under the request's `id` and command
//!   type when the line carries them, and the connection ends. A client that
//!   sends nothing within [`IDLE_BUDGET`] is closed.
//! - **Only a free socket.** The socket is taken only when nothing answers
//!   on it: a supervisor an earlier release left running keeps its socket
//!   until a launch retires it. The greeter's socket file is removed when the
//!   daemon ends, unless another server took the path.

use std::path::PathBuf;
use std::time::Duration;

use pa_types::daemon::{DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION, DAEMON_SCHEMA_ID};
use pa_types::platform::transport::TransportListener;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The refusal every old-wire request is answered with.
pub const RESTART_REFUSAL: &str = "This Prime Agent daemon no longer speaks the old daemon \
     protocol: the client is from an earlier release. Restart the client (quit and run \
     prime-agent again).";

/// How long a connection may stay silent before the greeter closes it.
const IDLE_BUDGET: Duration = Duration::from_secs(30);

/// The longest request line the greeter reads; a longer one is cut there.
const LINE_LIMIT: u64 = 64 * 1024;

/// The bound greeter socket: on drop, its listener closes and its file is
/// removed while it is still the one this greeter bound.
struct Bound {
    listener: Option<Box<dyn TransportListener>>,
    path: PathBuf,
    identity: Option<pa_types::daemon::SocketIdentity>,
}

impl Drop for Bound {
    fn drop(&mut self) {
        drop(self.listener.take());
        pa_core::platform::daemon_socket::cleanup_socket_path_after_close(
            &self.path,
            self.identity.take(),
        );
    }
}

/// Serves the greeter on `legacy_socket` until the task is dropped, naming
/// `runtime_socket` (the runtime daemon's own) in its hello. Takes the socket
/// only when nothing answers on it; returns at once otherwise.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn serve(legacy_socket: PathBuf, runtime_socket: PathBuf) {
    if let Err(error) = pa_core::platform::daemon_socket::prepare_socket_path(&legacy_socket).await
    {
        tracing::debug!("the legacy greeter stays off: {error:#}");
        return;
    }
    let listener = match pa_types::platform::transport::bind_transport(&legacy_socket).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::debug!(
                "the legacy greeter could not bind {}: {error:#}",
                legacy_socket.display()
            );
            return;
        }
    };
    let identity = pa_types::platform::socket_identity(&legacy_socket);
    pa_core::platform::daemon_socket::restrict_socket_path(&legacy_socket);
    let bound = Bound {
        listener: Some(listener),
        path: legacy_socket,
        identity,
    };
    // The `daemon_hello` each connection is sent first.
    let mut hello = json!({
        "type": "daemon_hello",
        "socketPath": bound.path,
        "protocol": { "name": DAEMON_PROTOCOL_NAME, "version": DAEMON_PROTOCOL_VERSION },
        "schemaId": DAEMON_SCHEMA_ID,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "legacyGreeter": { "reason": RESTART_REFUSAL, "runtimeSocket": runtime_socket },
        "serverCapabilities": [],
    })
    .to_string();
    hello.push('\n');
    let mut connections = tokio::task::JoinSet::new();
    while let Some(listener) = bound.listener.as_ref() {
        match listener.accept().await {
            Ok(stream) => {
                while connections.try_join_next().is_some() {}
                let hello = hello.clone();
                // One connection: the hello, then the refusal of its first request.
                connections.spawn(async move {
                    let (reader, mut writer) = stream.split();
                    if writer.write_all(hello.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut reader = BufReader::new(reader.take(LINE_LIMIT));
                    let mut line = String::new();
                    let read = tokio::time::timeout(IDLE_BUDGET, reader.read_line(&mut line)).await;
                    if !matches!(read, Ok(Ok(read)) if read > 0) {
                        return;
                    }
                    // The failed `response`, under the request's `id` and command type when
                    // the line is an old-wire command (an envelope or a bare command).
                    let request = serde_json::from_str::<Value>(line.trim()).unwrap_or(Value::Null);
                    let command = request.get("command").unwrap_or(&request);
                    let mut reply = json!({
                        "type": "response",
                        "command":
                            command.get("type").and_then(Value::as_str).unwrap_or("unknown"),
                        "success": false,
                        "error": RESTART_REFUSAL,
                    });
                    if let Some(id) = request.get("id").filter(|id| id.is_string()) {
                        reply["id"] = id.clone();
                    }
                    let mut reply = reply.to_string();
                    reply.push('\n');
                    let _ = writer.write_all(reply.as_bytes()).await;
                    let _ = writer.shutdown().await;
                });
            }
            Err(error) => tracing::debug!("the legacy greeter's accept failed: {error}"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// An old-wire client is greeted and its first request refused with the restart reason, in
    /// the old wire's frames; a second greeter on a socket that answers stays off.
    #[tokio::test]
    async fn an_old_client_is_told_to_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (legacy, runtime) = (
            dir.path().join("daemon.sock"),
            dir.path().join("daemon2.sock"),
        );
        let greeter = tokio::spawn(serve(legacy.clone(), runtime.clone()));
        let started = std::time::Instant::now();
        let stream = loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(&legacy).await {
                break stream;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the greeter never bound"
            );
            tokio::task::yield_now().await;
        };
        // A second greeter finds the socket answering and leaves it.
        tokio::time::timeout(
            Duration::from_secs(10),
            serve(legacy.clone(), runtime.clone()),
        )
        .await
        .expect("a greeter on an answering socket returns at once");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let hello: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let envelope = json!({
            "type": "command",
            "id": "daemon_1",
            "protocol": { "name": DAEMON_PROTOCOL_NAME, "version": DAEMON_PROTOCOL_VERSION },
            "command": { "type": "list" },
        });
        writer
            .write_all(format!("{envelope}\n").as_bytes())
            .await
            .unwrap();
        let reply: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            (hello, reply, lines.next_line().await.unwrap()),
            (
                json!({
                    "type": "daemon_hello",
                    "socketPath": legacy,
                    "protocol": { "name": DAEMON_PROTOCOL_NAME, "version": DAEMON_PROTOCOL_VERSION },
                    "schemaId": DAEMON_SCHEMA_ID,
                    "appVersion": env!("CARGO_PKG_VERSION"),
                    "legacyGreeter": { "reason": RESTART_REFUSAL, "runtimeSocket": runtime },
                    "serverCapabilities": [],
                }),
                json!({
                    "type": "response",
                    "id": "daemon_1",
                    "command": "list",
                    "success": false,
                    "error": RESTART_REFUSAL,
                }),
                None,
            )
        );
        greeter.abort();
        let _ = greeter.await;
        assert!(!legacy.exists(), "the greeter's socket file goes with it");
    }
}
