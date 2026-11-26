//! The ACP client's connection to a daemon, at the level of the old wire's
//! commands ([`DaemonTransport`]): the ACP surface (`daemon.rs`,
//! `sessions.rs`, `wire_config.rs`) sends `pa_types::daemon` commands and
//! reads their responses and the session's events through it, whatever wire
//! carries them. [`DaemonLink`] carries them on the old wire, to the
//! supervisor's socket.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use pa_types::daemon::{
    DaemonCommand, DaemonCommandEnvelope, DaemonCommandFrameType, DaemonProtocolInfo,
    DaemonResponse, DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

/// How long a request waits for its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseWait {
    /// At most the session-scoped response timeout (30 s).
    Bounded,
    /// Until the response or the connection's close, with no fixed cap: a
    /// turn-long command (TS caps them at 24 h; the close is the liveness
    /// bound here, not a timer).
    UntilClose,
}

/// One frame the daemon sent the ACP client, in the order it sent them.
pub(crate) enum TransportFrame {
    /// One session event, in the old wire's `session_event` frame shape
    /// (`{"type": "session_event", "event": {...}}`).
    Event(Value),
    /// The response to one request; the frame consumer hands it on with
    /// [`DaemonTransport::deliver`].
    Response(DaemonResponse),
    /// The daemon-global heartbeat-catalog broadcast (`heartbeats_changed`).
    HeartbeatsChanged,
}

/// The ACP client's command-level connection to a daemon.
///
/// A request sends one `pa_types::daemon` command and resolves with its
/// response. The responses come back as frames, in one stream with the
/// session events ([`DaemonTransport::next_frame`]), which one frame consumer
/// reads in order and through which it hands each response to its request
/// ([`DaemonTransport::deliver`]): every event the daemon sent before a
/// response is handled before that response resolves its request, so a
/// turn's events are all published by the time its prompt's response is
/// read. An implementation translates the commands to its wire and its
/// wire's events and answers to these frames, keeping that order, and fails
/// requests fast once [`DaemonTransport::close`] was called.
pub(crate) trait DaemonTransport: Send + Sync + 'static {
    /// Sends `command` and waits for its response as long as `wait` says.
    ///
    /// # Errors
    ///
    /// The connection is closed or closed mid-request, the command could not
    /// be sent, or a bounded wait timed out.
    fn exchange(
        &self,
        command: DaemonCommand,
        wait: ResponseWait,
    ) -> impl Future<Output = anyhow::Result<DaemonResponse>> + Send;

    /// The next frame, or `None` once the connection closed and every frame
    /// read from it was given out. Only the frame consumer calls it.
    fn next_frame(&self) -> impl Future<Output = Option<TransportFrame>> + Send;

    /// Hands `response`, a frame [`DaemonTransport::next_frame`] gave, to the
    /// request waiting for it; a response no request waits for is dropped.
    fn deliver(&self, response: DaemonResponse);

    /// The frames ended: fails every pending request (each answers the
    /// mid-request close) and every later one.
    fn close(&self);

    /// [`DaemonTransport::exchange`] under the session-scoped response
    /// timeout.
    fn request(
        &self,
        command: DaemonCommand,
    ) -> impl Future<Output = anyhow::Result<DaemonResponse>> + Send {
        self.exchange(command, ResponseWait::Bounded)
    }

    /// [`DaemonTransport::exchange`] until the response or the close.
    fn request_until_close(
        &self,
        command: DaemonCommand,
    ) -> impl Future<Output = anyhow::Result<DaemonResponse>> + Send {
        self.exchange(command, ResponseWait::UntilClose)
    }

    /// [`DaemonTransport::request`], with a failed response as the error.
    fn request_ok(
        &self,
        command: DaemonCommand,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            let response = self.request(command).await?;
            if !response.success {
                anyhow::bail!(response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string()));
            }
            Ok(())
        }
    }
}

/// Response timeout for session-scoped commands.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A client connection to the supervisor socket: JSONL command envelopes
/// out, responses matched by id, session events forwarded raw.
pub(crate) struct DaemonLink {
    pub(super) writer: mpsc::UnboundedSender<String>,
    pub(super) pending: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<DaemonResponse>>>>,
    /// Set once the frame channel ends (the socket closed and every
    /// frame the reader read is delivered): every new request fails
    /// fast instead of waiting for a response no one will send.
    pub(super) closed: Arc<std::sync::atomic::AtomicBool>,
    pub(super) frames: Mutex<mpsc::UnboundedReceiver<TransportFrame>>,
    pub(super) protocol_version: u64,
    pub(super) next_request_id: std::sync::atomic::AtomicU64,
}

impl DaemonLink {
    pub(super) async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            pa_types::platform::transport::connect_transport(socket_path),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to the daemon socket"))??;
        let (reader_half, writer_half) = stream.split();
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<TransportFrame>();
        let (hello_tx, hello_rx) = oneshot::channel::<Value>();
        let pending: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<DaemonResponse>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let closed: Arc<std::sync::atomic::AtomicBool> =
            Arc::new(std::sync::atomic::AtomicBool::new(false));

        tokio::spawn(async move {
            let mut writer = writer_half;
            while let Some(line) = line_rx.recv().await {
                let mut payload = line.into_bytes();
                payload.push(b'\n');
                if writer.write_all(&payload).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        // The reader only classifies frames; the consumer loop below owns
        // the ordering, so a turn's chunks always precede its boundary
        // frames.
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader_half);
            let mut line = String::new();
            let mut hello_tx = Some(hello_tx);
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                match value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "daemon_hello" => {
                        if let Some(tx) = hello_tx.take() {
                            let _ = tx.send(value);
                        }
                    }
                    "response" => {
                        let Ok(response) = serde_json::from_value::<DaemonResponse>(value.clone())
                        else {
                            continue;
                        };
                        let _ = frame_tx.send(TransportFrame::Response(response));
                    }
                    "session_event" => {
                        let _ = frame_tx.send(TransportFrame::Event(value));
                    }
                    "heartbeats_changed" => {
                        let _ = frame_tx.send(TransportFrame::HeartbeatsChanged);
                    }
                    _ => {}
                }
            }
        });

        let hello = tokio::time::timeout(std::time::Duration::from_secs(3), hello_rx)
            .await
            .map_err(|_| anyhow::anyhow!("the daemon did not send its handshake"))?
            .map_err(|_| anyhow::anyhow!("the daemon connection closed before the handshake"))?;
        let protocol = hello
            .get("protocol")
            .cloned()
            .and_then(|p| serde_json::from_value::<DaemonProtocolInfo>(p).ok())
            .unwrap_or(DaemonProtocolInfo {
                name: DAEMON_PROTOCOL_NAME.to_string(),
                version: DAEMON_PROTOCOL_VERSION,
            });
        let version = protocol.version.min(DAEMON_PROTOCOL_VERSION);
        Ok(DaemonLink {
            writer: line_tx,
            pending,
            closed,
            frames: Mutex::new(frame_rx),
            protocol_version: version,
            next_request_id: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

impl DaemonTransport for DaemonLink {
    /// One exchange: register the response slot, send the envelope, and
    /// wait for the answer.
    async fn exchange(
        &self,
        command: DaemonCommand,
        wait: ResponseWait,
    ) -> anyhow::Result<DaemonResponse> {
        use std::sync::atomic::Ordering;
        let id = format!(
            "acp-{}",
            self.next_request_id.fetch_add(1, Ordering::SeqCst) + 1
        );
        let envelope = DaemonCommandEnvelope {
            frame_type: DaemonCommandFrameType::Command,
            id: id.clone(),
            protocol: DaemonProtocolInfo {
                name: DAEMON_PROTOCOL_NAME.to_string(),
                version: self.protocol_version,
            },
            client_id: Some(format!("acp:{}", std::process::id())),
            command,
        };
        let line = serde_json::to_string(&envelope)?;
        let (tx, rx) = oneshot::channel::<DaemonResponse>();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        // A request that races the close loses either way: the flag
        // fails it here, or the pending clear already dropped its
        // sender. The flag is stored before the clear, so no request
        // parks unnoticed.
        if self.closed.load(Ordering::SeqCst) {
            self.pending.lock().unwrap().remove(&id);
            anyhow::bail!("the daemon connection is closed");
        }
        if self.writer.send(line).is_err() {
            // A closed writer leaves the pending slot behind otherwise; a
            // link that never answers again would grow one entry per
            // request.
            self.pending.lock().unwrap().remove(&id);
            anyhow::bail!("the daemon connection is closed");
        }
        match wait {
            ResponseWait::Bounded => {
                tokio::time::timeout(REQUEST_TIMEOUT, rx)
                    .await
                    .map_err(|_| {
                        self.pending.lock().unwrap().remove(&id);
                        anyhow::anyhow!("timed out waiting for the daemon response")
                    })?
            }
            ResponseWait::UntilClose => rx.await,
        }
        .map_err(|_| anyhow::anyhow!("the daemon connection closed mid-request"))
    }

    async fn next_frame(&self) -> Option<TransportFrame> {
        self.frames.lock().await.recv().await
    }

    fn deliver(&self, response: DaemonResponse) {
        let id = response.id.clone().unwrap_or_default();
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(response);
        }
    }

    fn close(&self) {
        // The flag goes up before the clear; `exchange` covers the race.
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
    }
}
