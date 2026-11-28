//! The ACP client's connection to a daemon, at the level of the old wire's
//! commands ([`DaemonTransport`]): the ACP surface (`daemon.rs`,
//! `sessions.rs`, `wire_config.rs`) sends `pa_types::daemon` commands and
//! reads their responses and the session's events through it, whatever wire
//! carries them. The runtime daemon's link (`wire2_link.rs`) carries them on
//! wire2.

use std::future::Future;

use pa_types::daemon::{DaemonCommand, DaemonResponse};
use serde_json::Value;

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
