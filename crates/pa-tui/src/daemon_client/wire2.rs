//! The wire2 link: [`super::DaemonClient`] on the runtime daemon's socket (`daemon2.sock`,
//! `pa_types::wire2`), the TUI's commands and events carried by [`crate::wire2_compat`].
//!
//! - **Connect.** `hello`, then the runtime's `welcome`, then the query `daemon.identity`,
//!   whose fields (and `serverCapabilities`) make the old `daemon_hello` the TUI reads. The link
//!   then follows, for the connection's life and from now, the catalog change
//!   (`runtime/models`), the daemon's closing (`system/closing`), the boot's history import
//!   (`system/history_import`, and the query for one already running) and every session's
//!   schedules (`agent/+/schedules`, the old `heartbeats_changed` broadcast). None of these is
//!   waited for: the connect answers once the identity came.
//! - **Requests.** Each command goes out as the frames [`wire2_compat::outgoing`] makes of it,
//!   under the request's id; its reply becomes the old response
//!   ([`wire2_compat::response_from_reply`]). A followed feed's or a streamed list's query goes
//!   out once its subscription's `backfilled` came; a streamed list is unsubscribed at its reply.
//!   `prompt_and_wait` publishes once the session's `agent/<id>/done` is followed and answers at
//!   the `done` of the task the publish's reply names (a failed task fails the command).
//! - **Attach and resume.** An attach follows the session's topics first
//!   ([`wire2_compat::session_subscription`]): live from now for its stream, which is live only,
//!   and after the last stored event this process saw on this socket for its stored topics
//!   (faults, close), so a reconnect replays what happened while it was away. The attach call's
//!   reply is the snapshot (`lastEventSequence`); the session's events that arrive before it are
//!   held and released after it, those the snapshot already holds (a sequence at or below its
//!   `lastEventSequence`) dropped: the old attach window's contract.
//! - **Events.** Each event becomes the old frame the layer maps it to
//!   ([`wire2_compat::outbound_from_event`], replayed or live by its subscription's
//!   `backfilled`), its `message_update` deltas restored, then the [`DaemonClientEvent`] the old
//!   reader makes of that frame.
//! - **End.** The connection's end fails every pending request with a transport error and wakes
//!   [`super::DaemonClient::reader_dead`]; the event channel stays open until the client closes,
//!   as on the old wire.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::daemon::message_delta::MessageDeltaDecoder;
use pa_types::daemon::{
    DaemonCommand, DaemonOutbound, DaemonResponse, DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use pa_types::event_log::{Event, TopicFilter};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};

use super::{client_event_from_value, command_type_debug, DaemonClientEvent};
use crate::wire2_compat::{self, Delivery, Outgoing};

/// How many `done`s of tasks no request waits for (yet) the link keeps: a task's `done` can come
/// before its publish's reply.
const SEEN_DONES: usize = 256;

/// The subscriptions a connection keeps for its life.
const MODELS_SUB: &str = "compat:models";
const CLOSING_SUB: &str = "compat:closing";
const HEARTBEATS_SUB: &str = "compat:heartbeats";

/// The last stored event's sequence number each runtime socket's connections saw in this process:
/// a reconnect's session subscriptions replay what came after it.
fn resume_points() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static POINTS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    POINTS.get_or_init(Mutex::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a reply is for.
enum Then {
    /// The old response to `command`.
    Respond,
    /// The old response, after the streamed list's subscription is stopped.
    Unsubscribe(ClientFrame),
    /// An attach's reply: the held events of `session` follow it.
    Attach { session: String },
    /// A `prompt_and_wait`'s publish: the response waits for its task's `done`.
    Task,
    /// The running history import's started frame, sent as an event.
    HistoryImport,
}

/// A request waiting for its reply.
struct Pending {
    /// The old command's type, which the response names.
    command: String,
    then: Then,
    /// `None` for the link's own queries.
    reply: Option<oneshot::Sender<Result<DaemonResponse>>>,
}

/// One subscription the link sent.
struct Subscription {
    backfilled: bool,
    /// The frame to send once its `backfilled` came (a feed's query, a prompt's publish).
    then_send: Option<ClientFrame>,
}

/// The connection's state, shared by the requests and the reader.
struct State {
    pending: HashMap<String, Pending>,
    subscriptions: HashMap<String, Subscription>,
    /// The sessions being attached: their events, held until the attach's reply.
    held: HashMap<String, Vec<DaemonClientEvent>>,
    /// Task ids to the response the old `prompt_and_wait` waits on.
    waiting_done: HashMap<String, (String, oneshot::Sender<Result<DaemonResponse>>)>,
    /// `done` payloads no request waits for yet, oldest first in `done_order`.
    seen_done: HashMap<String, Value>,
    done_order: VecDeque<String>,
    /// Restores the stream's delta `message_update`s to their full message.
    deltas: MessageDeltaDecoder,
    /// The client's event channel; `None` once the client closed.
    events: Option<mpsc::UnboundedSender<DaemonClientEvent>>,
    /// The last stored event's sequence number this connection saw.
    last_seq: u64,
    closed: bool,
}

/// A wire2 connection to the runtime daemon.
pub(crate) struct Wire2Link {
    socket_path: PathBuf,
    writer: mpsc::UnboundedSender<Vec<u8>>,
    state: Arc<Mutex<State>>,
    reader_dead: watch::Receiver<bool>,
}

/// What a connect hands the client.
pub(crate) struct Connected {
    pub(crate) link: Arc<Wire2Link>,
    /// The old `daemon_hello` the runtime's identity makes.
    pub(crate) hello: Value,
    pub(crate) events: mpsc::UnboundedReceiver<DaemonClientEvent>,
    pub(crate) reader_dead: watch::Receiver<bool>,
}

fn encode(frame: &ClientFrame) -> Result<Vec<u8>> {
    encode_private_frame(
        &serde_json::to_value(frame)?,
        &[],
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
}

impl Wire2Link {
    /// Connects to the runtime daemon on `socket_path` as `client`, and reads its identity.
    ///
    /// # Errors
    ///
    /// The socket does not answer within `connect_timeout`, or the runtime does not welcome the
    /// client or answer its identity within `hello_timeout`.
    pub(crate) async fn connect(
        socket_path: &Path,
        client: &str,
        connect_timeout: Duration,
        hello_timeout: Duration,
    ) -> Result<Connected> {
        let stream = tokio::time::timeout(
            connect_timeout,
            pa_types::platform::transport::connect_transport(socket_path),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "Timed out after {}ms connecting to the Prime Agent daemon. Socket: {}.",
                connect_timeout.as_millis(),
                socket_path.display()
            )
        })?
        .with_context(|| {
            format!(
                "Failed to connect to the Prime Agent daemon. Socket: {}.",
                socket_path.display()
            )
        })?;
        let (reader_half, mut writer_half) = stream.split();
        let mut reader = PrivateFrameReader::new(reader_half, DEFAULT_PRIVATE_FRAME_LIMITS);
        let greeting = async {
            writer_half
                .write_all(&encode(&ClientFrame::Hello {
                    version: WIRE2_VERSION,
                    client: client.to_string(),
                })?)
                .await?;
            writer_half
                .write_all(&encode(&ClientFrame::Query {
                    id: "compat:identity".to_string(),
                    name: "daemon.identity".to_string(),
                    args: json!({}),
                })?)
                .await?;
            writer_half.flush().await?;
            let mut last_seq = None;
            loop {
                let frame = reader
                    .read_frame()
                    .await?
                    .ok_or_else(|| anyhow!("the daemon connection closed before the handshake"))?;
                match serde_json::from_value::<ServerFrame>(frame.header)? {
                    ServerFrame::Welcome { last_seq: seq, .. } => last_seq = Some(seq),
                    ServerFrame::Reply { id, outcome } if id == "compat:identity" => {
                        let seq = last_seq
                            .ok_or_else(|| anyhow!("the daemon answered before its welcome"))?;
                        return match outcome {
                            Outcome::Ok { result } => anyhow::Ok((seq, result)),
                            Outcome::Error { message, .. } => {
                                Err(anyhow!("the daemon's identity: {message}"))
                            }
                        };
                    }
                    ServerFrame::Closing { reason } => {
                        return Err(anyhow!("the daemon refused the connection: {reason}"))
                    }
                    ServerFrame::Reply { .. }
                    | ServerFrame::Event { .. }
                    | ServerFrame::Backfilled { .. } => {}
                }
            }
        };
        let (welcome_seq, identity) = tokio::time::timeout(hello_timeout, greeting)
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out after {}ms waiting for the Prime Agent daemon handshake. Socket: {}.",
                    hello_timeout.as_millis(),
                    socket_path.display()
                )
            })??;
        let hello = old_hello(socket_path, client, identity);

        let (writer, mut outbound) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(bytes) = outbound.recv().await {
                if bytes.is_empty()
                    || writer_half.write_all(&bytes).await.is_err()
                    || writer_half.flush().await.is_err()
                {
                    break;
                }
            }
            let _ = writer_half.shutdown().await;
        });
        let (event_tx, event_rx) = mpsc::unbounded_channel::<DaemonClientEvent>();
        let resume = lock(resume_points()).get(socket_path).copied();
        let state = Arc::new(Mutex::new(State {
            pending: HashMap::new(),
            subscriptions: HashMap::new(),
            held: HashMap::new(),
            waiting_done: HashMap::new(),
            seen_done: HashMap::new(),
            done_order: VecDeque::new(),
            deltas: MessageDeltaDecoder::new(),
            events: Some(event_tx),
            last_seq: resume.unwrap_or(welcome_seq),
            closed: false,
        }));
        let (dead_tx, dead_rx) = watch::channel(false);
        let link = Arc::new(Wire2Link {
            socket_path: socket_path.to_path_buf(),
            writer,
            state: Arc::clone(&state),
            reader_dead: dead_rx.clone(),
        });
        {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                while let Ok(Some(frame)) = reader.read_frame().await {
                    if let Ok(frame) = serde_json::from_value::<ServerFrame>(frame.header) {
                        link.receive(frame);
                    }
                }
                let _ = dead_tx.send(true);
                link.fail_pending("the daemon connection closed");
            });
        }
        link.follow_connection_feeds()?;
        Ok(Connected {
            link,
            hello,
            events: event_rx,
            reader_dead: dead_rx,
        })
    }

    /// The subscriptions a connection keeps for its life, and the query for a running history
    /// import.
    fn follow_connection_feeds(&self) -> Result<()> {
        for (sub_id, filter) in [
            (MODELS_SUB, "runtime/models"),
            (CLOSING_SUB, "system/closing"),
            (HEARTBEATS_SUB, "agent/+/schedules"),
        ] {
            self.subscribe(
                &ClientFrame::Subscribe {
                    sub_id: sub_id.to_string(),
                    filter: TopicFilter::new(filter)?,
                    since: Since::Now,
                },
                None,
            )?;
        }
        let mut frames =
            wire2_compat::history_import_request("compat:history_import".to_string())?.into_iter();
        let (Some(subscribe), Some(query)) = (frames.next(), frames.next()) else {
            return Err(anyhow!("the history import is a subscription and a query"));
        };
        self.subscribe(&subscribe, None)?;
        self.register(
            "compat:history_import",
            "history_import",
            Then::HistoryImport,
            None,
        );
        self.send(&query)
    }

    fn send(&self, frame: &ClientFrame) -> Result<()> {
        self.writer
            .send(encode(frame)?)
            .map_err(|_| anyhow!("the daemon connection is closed"))
    }

    fn register(
        &self,
        id: &str,
        command: &str,
        then: Then,
        reply: Option<oneshot::Sender<Result<DaemonResponse>>>,
    ) {
        lock(&self.state).pending.insert(
            id.to_string(),
            Pending {
                command: command.to_string(),
                then,
                reply,
            },
        );
    }

    /// Sends `subscribe` (a `Subscribe` frame) and, once its `backfilled` came, `then_send`; a
    /// subscription the link already follows and has backfilled sends `then_send` at once.
    fn subscribe(&self, subscribe: &ClientFrame, then_send: Option<ClientFrame>) -> Result<()> {
        let ClientFrame::Subscribe { sub_id, .. } = subscribe else {
            return Err(anyhow!("not a subscription: {subscribe:?}"));
        };
        let ready = {
            let mut state = lock(&self.state);
            match state.subscriptions.get_mut(sub_id) {
                Some(known) if known.backfilled => Some(then_send),
                Some(known) => {
                    known.then_send = then_send;
                    None
                }
                None => {
                    state.subscriptions.insert(
                        sub_id.clone(),
                        Subscription {
                            backfilled: false,
                            then_send,
                        },
                    );
                    self.send(subscribe)?;
                    None
                }
            }
        };
        if let Some(Some(frame)) = ready {
            self.send(&frame)?;
        }
        Ok(())
    }

    fn unsubscribe(&self, unsubscribe: &ClientFrame) -> Result<()> {
        if let ClientFrame::Unsubscribe { sub_id } = unsubscribe {
            lock(&self.state).subscriptions.remove(sub_id);
        }
        self.send(unsubscribe)
    }

    /// One command as its wire2 frames under request `id`, answered as the old response.
    ///
    /// # Errors
    ///
    /// The connection is closed, the frames cannot be sent, or no reply came within
    /// `timeout_ms`.
    pub(crate) async fn request(
        &self,
        command: DaemonCommand,
        id: &str,
        timeout_ms: u64,
    ) -> Result<DaemonResponse> {
        if *self.reader_dead.borrow() || lock(&self.state).closed {
            return Err(self.closed_error());
        }
        let name = command_type_debug(&command);
        let (tx, rx) = oneshot::channel::<Result<DaemonResponse>>();
        match wire2_compat::outgoing(id.to_string(), command)? {
            Outgoing::Publish(frame) | Outgoing::Query(frame) | Outgoing::Call(frame) => {
                self.register(id, &name, Then::Respond, Some(tx));
                self.send(&frame)?;
            }
            Outgoing::Follow { subscribe, query } => {
                self.register(id, &name, Then::Respond, Some(tx));
                self.subscribe(&subscribe, Some(query))?;
            }
            Outgoing::Stream {
                subscribe,
                query,
                unsubscribe,
            } => {
                self.register(id, &name, Then::Unsubscribe(unsubscribe), Some(tx));
                self.subscribe(&subscribe, Some(query))?;
            }
            Outgoing::Unfollow {
                unsubscribe,
                response,
            } => {
                self.unsubscribe(&unsubscribe)?;
                return old_response(*response);
            }
            Outgoing::Attach { session, call } => {
                let since = Since::After {
                    seq: lock(&self.state).last_seq,
                };
                lock(&self.state).held.entry(session.clone()).or_default();
                self.register(
                    id,
                    &name,
                    Then::Attach {
                        session: session.clone(),
                    },
                    Some(tx),
                );
                self.subscribe(
                    &wire2_compat::session_subscription(&session, since)?,
                    Some(call),
                )?;
            }
            Outgoing::Detach { session, call } => {
                self.register(id, &name, Then::Respond, Some(tx));
                self.send(&call)?;
                self.unsubscribe(&ClientFrame::Unsubscribe {
                    sub_id: wire2_compat::session_subscription_id(&session),
                })?;
            }
            Outgoing::PublishAndWait { session, publish } => {
                self.register(id, &name, Then::Task, Some(tx));
                self.subscribe(
                    &ClientFrame::Subscribe {
                        sub_id: format!("compat:done:{session}"),
                        filter: TopicFilter::new(format!("agent/{session}/done"))?,
                        since: Since::Now,
                    },
                    Some(publish),
                )?;
            }
            Outgoing::Local(response) => return old_response(*response),
            Outgoing::Unmapped { command, reason } => {
                let name = command_type_debug(&command);
                return Ok(DaemonResponse {
                    id: Some(id.to_string()),
                    command: name.clone(),
                    success: false,
                    data: None,
                    error: Some(format!(
                        "the runtime daemon does not take `{name}` from the terminal UI: {reason}"
                    )),
                    error_info: None,
                });
            }
        }
        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.closed_error()),
            Err(_) => {
                let mut state = lock(&self.state);
                state.pending.remove(id);
                state.waiting_done.retain(|_, (waiter, _)| waiter != id);
                Err(anyhow!(
                    "Timed out after {timeout_ms}ms waiting for the Prime Agent daemon response. Socket: {}.",
                    self.socket_path.display()
                ))
            }
        }
    }

    fn closed_error(&self) -> anyhow::Error {
        anyhow!(
            "Connection to the Prime Agent daemon closed. Socket: {}.",
            self.socket_path.display()
        )
    }

    /// Every pending request fails with `error`: the connection that carried it is gone.
    fn fail_pending(&self, error: &str) {
        let (pending, waiting) = {
            let mut state = lock(&self.state);
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.waiting_done),
            )
        };
        for request in pending.into_values() {
            if let Some(reply) = request.reply {
                let _ = reply.send(Err(anyhow!(error.to_string())));
            }
        }
        for (_, reply) in waiting.into_values() {
            let _ = reply.send(Err(anyhow!(error.to_string())));
        }
    }

    /// Closes the connection: pending requests fail, the event channel ends.
    pub(crate) fn close(&self) {
        let events = {
            let mut state = lock(&self.state);
            state.closed = true;
            state.events.take()
        };
        drop(events);
        let _ = self.writer.send(Vec::new());
        self.fail_pending("the daemon connection closed");
    }

    /// One frame from the runtime.
    fn receive(&self, frame: ServerFrame) {
        match frame {
            ServerFrame::Reply { id, outcome } => self.reply(&id, outcome),
            ServerFrame::Backfilled { sub_id, .. } => {
                let then_send =
                    lock(&self.state)
                        .subscriptions
                        .get_mut(&sub_id)
                        .and_then(|subscription| {
                            subscription.backfilled = true;
                            subscription.then_send.take()
                        });
                if let Some(frame) = then_send {
                    let _ = self.send(&frame);
                }
            }
            ServerFrame::Event { sub_id, event } => self.event(&sub_id, &event),
            // The welcome was read at the connect; the connection's end follows a closing.
            ServerFrame::Welcome { .. } | ServerFrame::Closing { .. } => {}
        }
    }

    fn reply(&self, id: &str, outcome: Outcome) {
        let Some(pending) = lock(&self.state).pending.remove(id) else {
            return;
        };
        let reply = pending.reply;
        match pending.then {
            Then::Respond => {
                respond(reply, &pending.command, id, outcome);
            }
            Then::Unsubscribe(unsubscribe) => {
                let _ = self.unsubscribe(&unsubscribe);
                respond(reply, &pending.command, id, outcome);
            }
            Then::Attach { session } => {
                let response = old_response(wire2_compat::response_from_reply(
                    &pending.command,
                    Some(id.to_string()),
                    outcome,
                ));
                let snapshot_sequence = response
                    .as_ref()
                    .ok()
                    .and_then(|response| response.data.as_ref())
                    .and_then(|data| data.get("lastEventSequence"))
                    .and_then(Value::as_u64);
                let (held, events) = {
                    let mut state = lock(&self.state);
                    (
                        state.held.remove(&session).unwrap_or_default(),
                        state.events.clone(),
                    )
                };
                if let Some(reply) = reply {
                    let _ = reply.send(response);
                }
                if let Some(events) = events {
                    for event in held {
                        let covered = match (&event, snapshot_sequence) {
                            (
                                DaemonClientEvent::SessionEvent { meta_sequence, .. },
                                Some(snapshot),
                            ) => *meta_sequence != 0 && *meta_sequence <= snapshot,
                            _ => false,
                        };
                        if !covered {
                            let _ = events.send(event);
                        }
                    }
                }
            }
            Then::Task => {
                let task = match &outcome {
                    Outcome::Ok { result } => result["task"].as_str().map(str::to_string),
                    Outcome::Error { .. } => None,
                };
                let Some(task) = task else {
                    let outcome = match outcome {
                        Outcome::Ok { .. } => Outcome::Error {
                            message: "the prompt was stored but no task was queued".to_string(),
                            info: None,
                        },
                        refused @ Outcome::Error { .. } => refused,
                    };
                    respond(reply, &pending.command, id, outcome);
                    return;
                };
                let Some(reply) = reply else { return };
                let seen = {
                    let mut state = lock(&self.state);
                    if let Some(done) = state.seen_done.remove(&task) {
                        state.done_order.retain(|seen| *seen != task);
                        Some((done, reply))
                    } else {
                        state.waiting_done.insert(task, (id.to_string(), reply));
                        None
                    }
                };
                if let Some((done, reply)) = seen {
                    let _ = reply.send(Ok(done_response(id, &done)));
                }
            }
            Then::HistoryImport => {
                if let Ok(Some(frame)) = wire2_compat::history_import_reply_frame(outcome) {
                    self.deliver(frame);
                }
            }
        }
    }

    /// One event for subscription `sub_id`.
    fn event(&self, sub_id: &str, event: &Event) {
        let topic = &event.event.topic;
        if !topic.is_live_only() && event.seq > 0 {
            let mut state = lock(&self.state);
            if event.seq > state.last_seq {
                state.last_seq = event.seq;
                lock(resume_points()).insert(self.socket_path.clone(), event.seq);
            }
        }
        let levels: Vec<&str> = topic.levels().collect();
        match levels.as_slice() {
            ["agent", _, "done"] => self.done(&event.event.payload),
            ["agent", _, "schedules"] => {
                if sub_id == HEARTBEATS_SUB {
                    self.send_event(DaemonClientEvent::HeartbeatsChanged);
                }
            }
            _ => {
                let delivery = match lock(&self.state).subscriptions.get(sub_id) {
                    Some(subscription) if !subscription.backfilled => Delivery::Backfill,
                    Some(_) | None => Delivery::Live,
                };
                if let Ok(Some(frame)) = wire2_compat::outbound_from_event(event, delivery) {
                    if let Ok(frame) = serde_json::to_value(frame) {
                        self.deliver(frame);
                    }
                }
            }
        }
    }

    /// A task's `done`: the response of the `prompt_and_wait` waiting for it, or kept for the
    /// one whose publish's reply has not come yet.
    fn done(&self, payload: &Value) {
        let Some(task) = payload["task"].as_str().map(str::to_string) else {
            return;
        };
        let waiter = {
            let mut state = lock(&self.state);
            let waiter = state.waiting_done.remove(&task);
            if waiter.is_none() {
                if !state.seen_done.contains_key(&task) {
                    state.done_order.push_back(task.clone());
                }
                state.seen_done.insert(task, payload.clone());
                while state.done_order.len() > SEEN_DONES {
                    if let Some(oldest) = state.done_order.pop_front() {
                        state.seen_done.remove(&oldest);
                    }
                }
            }
            waiter
        };
        if let Some((id, reply)) = waiter {
            let _ = reply.send(Ok(done_response(&id, payload)));
        }
    }

    /// One old frame to the client: its deltas restored, then its event, held while its
    /// session's attach is in flight.
    fn deliver(&self, mut frame: Value) {
        let event = {
            let mut state = lock(&self.state);
            state.deltas.decode(&mut frame);
            #[cfg(debug_assertions)]
            crate::inbound_recording::record(crate::inbound_recording::Via::Wire2, &frame);
            let Some(event) = client_event_from_value(&frame) else {
                return;
            };
            let session = match &event {
                DaemonClientEvent::SessionEvent {
                    active_session_id, ..
                }
                | DaemonClientEvent::SessionClosed {
                    active_session_id, ..
                }
                | DaemonClientEvent::SideQuestionEvent {
                    active_session_id, ..
                } => Some(active_session_id.clone()),
                DaemonClientEvent::DirectLinkLost { .. }
                | DaemonClientEvent::SessionListItem { .. }
                | DaemonClientEvent::SessionListProgress { .. }
                | DaemonClientEvent::DaemonClosing { .. }
                | DaemonClientEvent::AgentTraffic(_)
                | DaemonClientEvent::RosterUpdate { .. }
                | DaemonClientEvent::HeartbeatsChanged
                | DaemonClientEvent::ModelCatalogChanged
                | DaemonClientEvent::HistoryImport { .. }
                | DaemonClientEvent::SessionBinding { .. } => None,
            };
            if let Some(held) = session.and_then(|session| state.held.get_mut(&session)) {
                held.push(event);
                return;
            }
            event
        };
        self.send_event(event);
    }

    fn send_event(&self, event: DaemonClientEvent) {
        if let Some(events) = &lock(&self.state).events {
            let _ = events.send(event);
        }
    }
}

/// The response an old command's reply `outcome` becomes, sent to `reply`.
fn respond(
    reply: Option<oneshot::Sender<Result<DaemonResponse>>>,
    command: &str,
    id: &str,
    outcome: Outcome,
) {
    if let Some(reply) = reply {
        let _ = reply.send(old_response(wire2_compat::response_from_reply(
            command,
            Some(id.to_string()),
            outcome,
        )));
    }
}

/// The response frame's response.
fn old_response(frame: DaemonOutbound) -> Result<DaemonResponse> {
    match frame {
        DaemonOutbound::Response { response } => Ok(response),
        other => Err(anyhow!("not a response: {other:?}")),
    }
}

/// The old `prompt_and_wait` response for a task's `done`: a failed task is the command's failure.
fn done_response(id: &str, done: &Value) -> DaemonResponse {
    let failed = done["outcome"] == "failed";
    DaemonResponse {
        id: Some(id.to_string()),
        command: "prompt_and_wait".to_string(),
        success: !failed,
        data: (!failed).then(|| done.clone()),
        error: failed.then(|| {
            done["error"]
                .as_str()
                .unwrap_or("the prompt failed")
                .to_string()
        }),
        error_info: None,
    }
}

/// The old `daemon_hello` the runtime's `daemon.identity` makes: its identity fields, the old
/// protocol, and the capabilities it names as served through the compat layer (the direct worker
/// link is never one; none when it names none).
fn old_hello(socket_path: &Path, client: &str, identity: Value) -> Value {
    let mut hello = match identity {
        Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    hello.insert("type".to_string(), json!("daemon_hello"));
    hello.insert(
        "protocol".to_string(),
        json!({ "name": DAEMON_PROTOCOL_NAME, "version": DAEMON_PROTOCOL_VERSION }),
    );
    hello
        .entry("socketPath".to_string())
        .or_insert_with(|| json!(socket_path.display().to_string()));
    hello.insert("clientId".to_string(), json!(client));
    let capabilities = hello
        .entry("serverCapabilities".to_string())
        .or_insert_with(|| json!([]));
    if let Some(capabilities) = capabilities.as_array_mut() {
        capabilities.retain(|capability| {
            capability.as_str() != Some(crate::direct_transport::DIRECT_PEER_TRANSPORT_CAPABILITY)
        });
    }
    Value::Object(hello)
}
