//! The CLI's wire2 client for the runtime daemon (`pa_types::wire2`), behind
//! the daemon-backed public commands when the runtime daemon is selected:
//! connect to `daemon2.sock`, say `hello`, then one `query` or `call` at a
//! time, its `reply` matched by id. Like [`crate::daemon_client`] it never
//! spawns a daemon, and an unanswered socket surfaces the same error text.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::platform::transport::{connect_blocking, BlockingTransportStream};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, WIRE2_VERSION};
use serde_json::Value;

use crate::daemon_client::{endpoint_details, node_connect_error};

/// How long a query or call may take to be answered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the daemon may take to welcome the client: the old daemon
/// client's greeting budget.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const READ_POLL: Duration = Duration::from_millis(50);

pub(crate) struct RuntimeDaemonClient {
    socket_path: PathBuf,
    stream: Box<dyn BlockingTransportStream>,
    decoder: PrivateFrameDecoder,
    /// Frames decoded but not read yet.
    frames: VecDeque<ServerFrame>,
    next_id: u64,
    /// The name the client says `hello` with: the old daemon client's
    /// protocol client id, which a message the client sends is from.
    client: String,
}

impl RuntimeDaemonClient {
    /// Connects to the runtime daemon on `socket_path` and says hello.
    ///
    /// # Errors
    ///
    /// The socket does not answer (the old daemon client's error text), or
    /// the daemon refused the client or did not welcome it in time.
    pub(crate) fn connect(socket_path: &Path) -> Result<Self> {
        let stream = connect_blocking(socket_path).map_err(|error| {
            anyhow!(
                "Failed to connect to the Prime Agent daemon: {}. {}",
                node_connect_error(&error, socket_path),
                endpoint_details(socket_path)
            )
        })?;
        let mut client = RuntimeDaemonClient {
            socket_path: socket_path.to_path_buf(),
            stream,
            decoder: PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS),
            frames: VecDeque::new(),
            next_id: 0,
            client: format!("daemon-client:{}", uuid::Uuid::new_v4()),
        };
        client.send(&ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: client.client.clone(),
        })?;
        let deadline = Instant::now() + HELLO_TIMEOUT;
        loop {
            match client.frame(
                deadline,
                HELLO_TIMEOUT,
                "waiting for the Prime Agent daemon handshake",
            )? {
                ServerFrame::Welcome { .. } => return Ok(client),
                ServerFrame::Closing { reason } => return Err(client.closed(Some(&reason))),
                ServerFrame::Event { .. }
                | ServerFrame::Backfilled { .. }
                | ServerFrame::Reply { .. } => {}
            }
        }
    }

    /// The name the client said hello with.
    pub(crate) fn client_name(&self) -> &str {
        &self.client
    }

    /// Asks query `name` with `args`; its result.
    ///
    /// # Errors
    ///
    /// The daemon answered with an error (its message), or the connection
    /// failed or timed out.
    pub(crate) fn query(&mut self, name: &str, args: Value) -> Result<Value> {
        let id = self.id();
        self.send(&ClientFrame::Query {
            id: id.clone(),
            name: name.to_string(),
            args,
        })?;
        self.reply(&id, name)
    }

    /// Makes call `method` with `args`; its result.
    ///
    /// # Errors
    ///
    /// The daemon answered with an error (its message), or the connection
    /// failed or timed out.
    pub(crate) fn call(&mut self, method: &str, args: Value) -> Result<Value> {
        let id = self.id();
        self.send(&ClientFrame::Call {
            id: id.clone(),
            method: method.to_string(),
            args,
        })?;
        self.reply(&id, method)
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("cli_{}", self.next_id)
    }

    fn send(&mut self, frame: &ClientFrame) -> Result<()> {
        let bytes = encode_private_frame(
            &serde_json::to_value(frame)?,
            &[],
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    /// The reply to request `id` (`operation`), as its result.
    fn reply(&mut self, id: &str, operation: &str) -> Result<Value> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let waiting = format!("waiting for the Prime Agent daemon response to \"{operation}\"");
        loop {
            match self.frame(deadline, REQUEST_TIMEOUT, &waiting)? {
                ServerFrame::Reply {
                    id: replied,
                    outcome,
                } if replied == id => {
                    return match outcome {
                        Outcome::Ok { result } => Ok(result),
                        Outcome::Error { message } => Err(anyhow!(message)),
                    }
                }
                ServerFrame::Closing { reason } => return Err(self.closed(Some(&reason))),
                ServerFrame::Reply { .. }
                | ServerFrame::Welcome { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::Backfilled { .. } => {}
            }
        }
    }

    /// The next frame, read by `deadline` (`timeout` from the wait's
    /// start); `waiting` names the wait in the timeout's error.
    fn frame(
        &mut self,
        deadline: Instant,
        timeout: Duration,
        waiting: &str,
    ) -> Result<ServerFrame> {
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Ok(frame);
            }
            self.stream
                .set_read_timeout(READ_POLL)
                .map_err(|error| anyhow!("daemon socket error: {error}"))?;
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(self.closed(None)),
                Ok(read) => {
                    for frame in self.decoder.push(&chunk[..read])? {
                        self.frames.push_back(
                            serde_json::from_value(frame.header).map_err(|error| {
                                anyhow!("invalid runtime daemon frame: {error}")
                            })?,
                        );
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() >= deadline {
                        let timeout_ms = timeout.as_millis();
                        return Err(anyhow!(
                            "Timed out after {timeout_ms}ms {waiting}. {}",
                            endpoint_details(&self.socket_path)
                        ));
                    }
                }
                Err(error) => return Err(anyhow!("daemon socket error: {error}")),
            }
        }
    }

    /// The old daemon client's closed-connection error, with `reason`.
    fn closed(&self, reason: Option<&str>) -> anyhow::Error {
        let reason = reason.map_or_default(|reason| format!(" Reason: {reason}."));
        anyhow!(
            "Connection to the Prime Agent daemon closed.{reason} {}",
            endpoint_details(&self.socket_path)
        )
    }
}
