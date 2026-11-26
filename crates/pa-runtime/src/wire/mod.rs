//! The wire2 server: one client connection, carried by the private-frame
//! codec (`pa_types::daemon::framing`; each frame's header is one
//! `pa_types::wire2` frame's JSON and its payload is empty).
//!
//! - **Hello.** The first frame must be `hello` with [`WIRE2_VERSION`]; any
//!   other first frame or version gets `closing` with a reason that says to
//!   restart the client, and the connection ends. Then `welcome` with the
//!   log's last sequence number.
//! - **Subscribe.** Replays from the log what `since` asks for (`now`:
//!   nothing; `after`: every matching event above `seq`; `tail`: the last
//!   `count` matching events, at most [`MAX_TAIL`]), then `backfilled` with
//!   the sequence number the replay covers, then live events above it, with
//!   no gap and no duplicate (see `feed`). Every subscribe gets exactly one
//!   `backfilled`, `now` too. A subscribe with an id in use restarts that
//!   subscription; `unsubscribe` of an unknown id does nothing.
//! - **Slow clients.** Frames wait in a bounded per-connection queue. A
//!   client that does not read fills it, its connection stops reading the
//!   bus, and the bus drops the oldest events for that connection alone
//!   (publishing never waits); the connection then catches up from the log
//!   in order. Live-only events (stream and tool deltas) are not in the log
//!   and are lost.
//! - **Live only.** A live-only event goes to each subscription whose filter
//!   matches it as the bus fans it out, with `seq` 0; it is never replayed.
//! - **Publish.** Only on [`CLIENT_TOPICS`] (`agent/<id>/input`), stored with
//!   `Trust::Owner`, the client's `hello` name as `source`, and `<id>` as the
//!   session; the reply's result is `{"seq": n}`, with `task` and
//!   `admission` when the publish's routes announced its task on
//!   `agent/<id>/accepted` (an `AgentAccepted`, as the runtime's input route
//!   does): a client that publishes a prompt and waits for it waits for that
//!   task's `agent/<id>/done`. Publishes are handled in the order sent.
//! - **Attached.** While one of a connection's subscriptions matches a
//!   session's `agent/<id>/done`, the connection is one of the session's
//!   attached clients (`attached`), as the session list counts them.
//! - **Query and call.** Parsed into [`Query`] / [`Call`] (an unknown name is
//!   an error reply) and answered by [`Handlers`] on their own tasks, at most
//!   [`IN_FLIGHT`] at once per connection.
//! - Any frame that is not a wire2 client frame, or a second `hello`, ends
//!   the connection with `closing`. So does an outbound frame larger than the
//!   frame limits.

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameLimits, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::event_log::{AgentAccepted, NewEvent, TopicFilter, Trust};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, WIRE2_VERSION};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::bus::{Bus, Published};
use crate::routes::{ModelCaller, TaskSink};
use crate::store::{EventStore, StateStore};
use crate::subscriptions::SessionTree;

mod attached;
mod feed;
mod handlers;

pub(crate) use attached::Attached;
pub use feed::MAX_TAIL;
use feed::{last_seq, Control, Feed};
pub use handlers::{Call, ConnectionId, Handlers, Query, UnknownOperation};

/// The topics a client may publish on: `agent/<id>/input`, a session's
/// input (a prompt, a steer, a follow-up).
pub const CLIENT_TOPICS: &[&str] = &["agent/+/input"];

/// How many queries and calls of one connection run at once; the next one
/// waits for one of them to finish.
pub const IN_FLIGHT: usize = 16;

/// How the server treats its connections.
#[derive(Debug, Clone, Copy)]
pub struct WireConfig {
    /// How many frames may wait for a client that is slow to read before
    /// its connection stops reading the bus (and later catches up).
    pub outbound_capacity: NonZeroUsize,
    /// The largest frame read or written.
    pub frame_limits: PrivateFrameLimits,
}

impl Default for WireConfig {
    fn default() -> Self {
        WireConfig {
            outbound_capacity: NonZeroUsize::new(1024).expect("1024 is not zero"),
            frame_limits: DEFAULT_PRIVATE_FRAME_LIMITS,
        }
    }
}

/// Serves wire2 connections against a bus; clones share it.
pub struct WireServer<E, K, G, M, T, H> {
    bus: Bus<E, K, G, M, T>,
    handlers: Arc<H>,
    config: WireConfig,
    client_topics: Arc<Vec<TopicFilter>>,
    attached: Arc<Attached>,
}

impl<E, K, G, M, T, H> Clone for WireServer<E, K, G, M, T, H> {
    fn clone(&self) -> Self {
        WireServer {
            bus: self.bus.clone(),
            handlers: Arc::clone(&self.handlers),
            config: self.config,
            client_topics: Arc::clone(&self.client_topics),
            attached: Arc::clone(&self.attached),
        }
    }
}

impl<E, K, G, M, T, H> WireServer<E, K, G, M, T, H>
where
    E: EventStore,
    K: StateStore,
    G: SessionTree,
    M: ModelCaller,
    T: TaskSink,
    H: Handlers,
{
    /// A server whose clients read and publish on `bus` and whose queries
    /// and calls `handlers` answers.
    ///
    /// # Panics
    ///
    /// A filter in [`CLIENT_TOPICS`] is not valid (a fixed list).
    #[must_use]
    pub fn new(bus: Bus<E, K, G, M, T>, handlers: H, config: WireConfig) -> Self {
        Self::attached_to(bus, handlers, config, Arc::default())
    }

    /// [`Self::new`], recording which connections follow which sessions in
    /// `attached`.
    pub(crate) fn attached_to(
        bus: Bus<E, K, G, M, T>,
        handlers: H,
        config: WireConfig,
        attached: Arc<Attached>,
    ) -> Self {
        WireServer {
            bus,
            handlers: Arc::new(handlers),
            config,
            client_topics: Arc::new(
                CLIENT_TOPICS
                    .iter()
                    .map(|filter| TopicFilter::new(*filter).expect("client topics are valid"))
                    .collect(),
            ),
            attached,
        }
    }

    /// Serves one connection until the client leaves or the server ends it.
    ///
    /// # Errors
    ///
    /// Reading or writing the transport failed, or the event log could not
    /// be read.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let limits = self.config.frame_limits;
        // Registered before `welcome` reads the log: see `feed`.
        let listener = self.bus.listen();
        let mut frames = PrivateFrameReader::new(reader, limits);
        let Some(first) = Box::pin(frames.read_frame()).await? else {
            return Ok(());
        };
        let client = match serde_json::from_value::<ClientFrame>(first.header) {
            Ok(ClientFrame::Hello { version, client }) if version == WIRE2_VERSION => client,
            Ok(ClientFrame::Hello { version, .. }) => {
                let reason = format!(
                    "this runtime speaks wire version {WIRE2_VERSION} and the client {version}: \
                     restart the client"
                );
                return write_now(&mut writer, &ServerFrame::Closing { reason }, limits).await;
            }
            Ok(
                ClientFrame::Subscribe { .. }
                | ClientFrame::Unsubscribe { .. }
                | ClientFrame::Publish { .. }
                | ClientFrame::Query { .. }
                | ClientFrame::Call { .. },
            )
            | Err(_) => {
                let reason = format!(
                    "the first frame must be hello for wire version {WIRE2_VERSION}: restart the \
                     client"
                );
                return write_now(&mut writer, &ServerFrame::Closing { reason }, limits).await;
            }
        };
        let welcome_seq = last_seq(self.bus.events()).await?;
        let welcome = ServerFrame::Welcome {
            version: WIRE2_VERSION,
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
            last_seq: welcome_seq,
        };
        write_now(&mut writer, &welcome, limits).await?;

        let (out, outbound) = mpsc::channel(self.config.outbound_capacity.get());
        let (control, controls) = mpsc::channel(1);
        let feed = Feed::new(self.bus.events(), out.clone(), welcome_seq);
        let connection = self.attached.connect(&client);
        let session = async {
            // Whichever ends first ends the other; ending drops their
            // senders, so the writer sends what is queued and ends.
            tokio::select! {
                read = self.read(&mut frames, client, control, out, &connection) => read,
                feeding = feed.run(listener, controls) => feeding,
            }
        };
        // Boxed: the reader's buffers would make `serve`'s own future large.
        let mut session = Box::pin(session);
        let mut write = Box::pin(write_queued(writer, outbound, limits));
        let served = tokio::select! {
            ended = &mut session => {
                let written = (&mut write).await;
                ended.and(written)
            }
            // The client takes no more frames: stop reading it too.
            written = &mut write => written,
        };
        // Its calls still running are dropped with the reader: what they
        // made it hold goes now.
        drop(session);
        self.handlers.disconnected(connection.id()).await;
        served
    }

    /// Reads the client's frames after `hello` and handles each.
    #[expect(
        clippy::too_many_lines,
        reason = "one match over the client's frames, each handled where it is read"
    )]
    async fn read<R: AsyncRead + Unpin + Send>(
        &self,
        frames: &mut PrivateFrameReader<R>,
        client: String,
        control: mpsc::Sender<Control>,
        out: mpsc::Sender<ServerFrame>,
        connection: &attached::Connection,
    ) -> anyhow::Result<()> {
        let in_flight = Arc::new(Semaphore::new(IN_FLIGHT));
        // Dropped with the connection, which aborts what is still running.
        let mut running = JoinSet::new();
        loop {
            let frame = tokio::select! {
                frame = frames.read_frame() => frame?,
                Some(_) = running.join_next(), if !running.is_empty() => continue,
            };
            let Some(frame) = frame else {
                return Ok(());
            };
            let gone = |_| anyhow::anyhow!("the client connection is closed");
            let frame = match serde_json::from_value::<ClientFrame>(frame.header) {
                Ok(frame) => frame,
                Err(error) => {
                    let reason = format!("not a wire2 client frame: {error}");
                    return out
                        .send(ServerFrame::Closing { reason })
                        .await
                        .map_err(gone);
                }
            };
            match frame {
                ClientFrame::Hello { .. } => {
                    let reason = "hello was sent twice".to_string();
                    return out
                        .send(ServerFrame::Closing { reason })
                        .await
                        .map_err(gone);
                }
                ClientFrame::Subscribe {
                    sub_id,
                    filter,
                    since,
                } => {
                    connection.subscribed(&sub_id, &filter);
                    control
                        .send(Control::Subscribe {
                            sub_id,
                            filter,
                            since,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("the connection's feed ended"))?;
                }
                ClientFrame::Unsubscribe { sub_id } => {
                    connection.unsubscribed(&sub_id);
                    control
                        .send(Control::Unsubscribe { sub_id })
                        .await
                        .map_err(|_| anyhow::anyhow!("the connection's feed ended"))?;
                }
                ClientFrame::Publish { id, topic, payload } => {
                    let session = topic.levels().nth(1).map(str::to_string);
                    let outcome = if self.client_topics.iter().any(|f| f.matches(&topic)) {
                        let event = NewEvent {
                            topic,
                            source: client.clone(),
                            session: session.clone(),
                            payload,
                            trust: Trust::Owner,
                            cause: None,
                            reply: None,
                        };
                        match self.bus.publish(event).await {
                            Ok(published) => {
                                let mut result = json!({ "seq": published.event.seq });
                                let accepted = session
                                    .as_deref()
                                    .and_then(|session| accepted(&published, session));
                                if let Some(accepted) = accepted {
                                    result["task"] = json!(accepted.task);
                                    result["admission"] = json!(accepted.admission);
                                }
                                Outcome::Ok { result }
                            }
                            Err(error) => Outcome::Error {
                                message: format!("the event was not published: {error:#}"),
                            },
                        }
                    } else {
                        Outcome::Error {
                            message: format!(
                                "clients may publish only on {}, not {topic}",
                                CLIENT_TOPICS.join(", ")
                            ),
                        }
                    };
                    out.send(ServerFrame::Reply { id, outcome })
                        .await
                        .map_err(gone)?;
                }
                ClientFrame::Query { id, name, args } => {
                    let permit = Arc::clone(&in_flight).acquire_owned().await?;
                    let handlers = Arc::clone(&self.handlers);
                    let answer = async move { handlers.query(name.parse()?, args).await };
                    running.spawn(answering(id, answer, out.clone(), permit));
                }
                ClientFrame::Call { id, method, args } => {
                    let permit = Arc::clone(&in_flight).acquire_owned().await?;
                    let handlers = Arc::clone(&self.handlers);
                    let connection = connection.id();
                    let answer =
                        async move { handlers.call(method.parse()?, args, connection).await };
                    running.spawn(answering(id, answer, out.clone(), permit));
                }
            }
        }
    }
}

/// The task `published`'s routes announced for `session`'s input on
/// `agent/<session>/accepted`, if any.
pub(crate) fn accepted(published: &Published, session: &str) -> Option<AgentAccepted> {
    let topic = format!("agent/{session}/accepted");
    published
        .caused
        .iter()
        .find(|caused| caused.event.topic.as_str() == topic)
        .and_then(|caused| serde_json::from_value(caused.event.payload.clone()).ok())
}

/// Replies to request `id` with what `answer` comes to, holding `permit`
/// until then.
async fn answering(
    id: String,
    answer: impl Future<Output = anyhow::Result<serde_json::Value>>,
    out: mpsc::Sender<ServerFrame>,
    permit: OwnedSemaphorePermit,
) {
    let outcome = match answer.await {
        Ok(result) => Outcome::Ok { result },
        Err(error) => Outcome::Error {
            message: format!("{error:#}"),
        },
    };
    drop(permit);
    // A client gone before its answer needs none.
    let _ = out.send(ServerFrame::Reply { id, outcome }).await;
}

/// Writes `frame` straight to the transport, before the queue exists.
async fn write_now<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ServerFrame,
    limits: PrivateFrameLimits,
) -> anyhow::Result<()> {
    let bytes = encode_private_frame(&serde_json::to_value(frame)?, &[], limits)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Writes queued frames until the queue closes or a `closing` is written,
/// then shuts the transport down. A frame over the limits is replaced by a
/// `closing` that says so.
async fn write_queued<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut outbound: mpsc::Receiver<ServerFrame>,
    limits: PrivateFrameLimits,
) -> anyhow::Result<()> {
    while let Some(frame) = outbound.recv().await {
        let closing = matches!(frame, ServerFrame::Closing { .. });
        match encode_private_frame(&serde_json::to_value(&frame)?, &[], limits) {
            Ok(bytes) => {
                writer.write_all(&bytes).await?;
                writer.flush().await?;
                if closing {
                    break;
                }
            }
            Err(error) => {
                let reason = format!("the runtime could not send a frame: {error:#}");
                write_now(&mut writer, &ServerFrame::Closing { reason }, limits).await?;
                break;
            }
        }
    }
    // Closing the queue first stops the reader and the feed.
    drop(outbound);
    writer.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
