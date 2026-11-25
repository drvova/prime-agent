use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use pa_types::daemon::framing::{write_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS};
use pa_types::event_log::{Event, NewEvent, Topic, TopicFilter, Trust};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::oneshot;

use super::{Call, Handlers, Query, WireConfig, WireServer};
use crate::bus::{Bus, BusParts};
use crate::lock;
use crate::routes::{ModelCall, ModelCaller, TaskRequest, TaskSink};
use crate::store::{EventStore, MemoryEventStore, MemoryStateStore};
use crate::subscriptions::MemorySessionTree;

/// The longest a test waits for one server frame before failing.
const RECV_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Takes no route's work: no test here adds a subscription.
struct Idle;

impl ModelCaller for Idle {
    fn call(&self, _call: ModelCall) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}

impl TaskSink for Idle {
    fn queue(&self, _request: TaskRequest) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Answers with what it was asked.
struct Echo;

impl Handlers for Echo {
    fn query(
        &self,
        query: Query,
        args: Value,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send {
        std::future::ready(Ok(json!({ "query": format!("{query:?}"), "args": args })))
    }

    fn call(&self, call: Call, args: Value) -> impl Future<Output = anyhow::Result<Value>> + Send {
        std::future::ready(Ok(json!({ "call": format!("{call:?}"), "args": args })))
    }
}

/// A log whose next `since` or `before` read can be held at a sync point.
#[derive(Clone, Default)]
struct Gated {
    log: MemoryEventStore,
    gates: Arc<Mutex<HashMap<&'static str, Gate>>>,
}

struct Gate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl Gated {
    /// Holds the next `read` (`since` or `before`): the first receiver fires
    /// when it is reached, and sending on the second lets it read.
    fn arm(&self, read: &'static str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, at_gate) = oneshot::channel();
        let (release, held) = oneshot::channel();
        lock(&self.gates).insert(
            read,
            Gate {
                reached,
                release: held,
            },
        );
        (at_gate, release)
    }

    async fn pass(gate: Option<Gate>) {
        if let Some(gate) = gate {
            gate.reached.send(()).expect("the test waits at the gate");
            gate.release.await.expect("the test releases the gate");
        }
    }
}

impl EventStore for Gated {
    fn append(&self, event: NewEvent) -> impl Future<Output = anyhow::Result<Event>> + Send {
        self.log.append(event)
    }

    fn since(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send {
        let gate = lock(&self.gates).remove("since");
        let log = self.log.clone();
        async move {
            Gated::pass(gate).await;
            log.since(seq, limit).await
        }
    }

    fn before(
        &self,
        seq: u64,
        limit: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send {
        let gate = lock(&self.gates).remove("before");
        let log = self.log.clone();
        async move {
            Gated::pass(gate).await;
            log.before(seq, limit).await
        }
    }
}

type TestBus = Bus<Gated, MemoryStateStore, MemorySessionTree, Idle, Idle>;
type TestServer = WireServer<Gated, MemoryStateStore, MemorySessionTree, Idle, Idle, Echo>;

struct Fixture {
    bus: TestBus,
    log: Gated,
    server: TestServer,
}

fn fixture(listener_capacity: usize, outbound_capacity: usize) -> Fixture {
    let log = Gated::default();
    let bus = Bus::new(BusParts {
        events: log.clone(),
        state: MemoryStateStore::default(),
        sessions: MemorySessionTree::default(),
        model: Idle,
        tasks: Idle,
        listener_capacity: NonZeroUsize::new(listener_capacity).expect("a capacity"),
    });
    let config = WireConfig {
        outbound_capacity: NonZeroUsize::new(outbound_capacity).expect("a capacity"),
        ..WireConfig::default()
    };
    Fixture {
        server: WireServer::new(bus.clone(), Echo, config),
        bus,
        log,
    }
}

fn topic(text: &str) -> Topic {
    Topic::new(text).expect("a topic")
}

fn event(on: &str) -> NewEvent {
    NewEvent {
        topic: topic(on),
        source: "test".to_string(),
        session: None,
        payload: json!({ "on": on }),
        trust: Trust::Owner,
        cause: None,
        reply: None,
    }
}

impl Fixture {
    async fn publish(&self, on: &str) -> Event {
        self.bus.publish(event(on)).await.expect("published").event
    }

    async fn stored(&self) -> Vec<Event> {
        self.log.since(0, usize::MAX).await.expect("the log reads")
    }

    /// A transport to a connection the server serves, before `hello`.
    fn open(&self, buffer: usize) -> Client {
        let (client, server_side) = tokio::io::duplex(buffer);
        let (reader, writer) = tokio::io::split(server_side);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(reader, writer).await });
        let (reader, writer) = tokio::io::split(client);
        Client {
            frames: PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS),
            writer,
        }
    }

    /// A connection that said `hello` as `name` and heard `welcome`.
    async fn connect(&self, name: &str, buffer: usize) -> Client {
        let mut client = self.open(buffer);
        client
            .send(&ClientFrame::Hello {
                version: WIRE2_VERSION,
                client: name.to_string(),
            })
            .await;
        let welcome = client.recv().await;
        assert!(
            matches!(welcome, Some(ServerFrame::Welcome { .. })),
            "{welcome:?}"
        );
        client
    }
}

struct Client {
    frames: PrivateFrameReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl Client {
    async fn send(&mut self, frame: &ClientFrame) {
        let header = serde_json::to_value(frame).expect("a frame serializes");
        write_frame(&mut self.writer, &header, &[], DEFAULT_PRIVATE_FRAME_LIMITS)
            .await
            .expect("the frame is written");
    }

    /// The next frame, or `None` once the server closed. A frame that never
    /// comes fails the test after `RECV_LIMIT` instead of hanging it: a
    /// broken catch-up or a stalled connection is a failure, not a stuck run.
    async fn recv(&mut self) -> Option<ServerFrame> {
        let frame = tokio::time::timeout(RECV_LIMIT, self.frames.read_frame())
            .await
            .expect("the server sends the next frame within the limit")
            .expect("a frame reads")?;
        Some(serde_json::from_value(frame.header).expect("a server frame"))
    }

    async fn recv_n(&mut self, count: usize) -> Vec<ServerFrame> {
        let mut frames = Vec::new();
        for _ in 0..count {
            frames.push(self.recv().await.expect("the connection is open"));
        }
        frames
    }

    async fn subscribe(&mut self, sub_id: &str, filter: &str, since: Since) {
        self.send(&ClientFrame::Subscribe {
            sub_id: sub_id.to_string(),
            filter: TopicFilter::new(filter).expect("a filter"),
            since,
        })
        .await;
    }
}

fn delivered(sub_id: &str, event: &Event) -> ServerFrame {
    ServerFrame::Event {
        sub_id: sub_id.to_string(),
        event: event.clone(),
    }
}

#[tokio::test]
async fn a_client_of_another_wire_version_is_told_to_restart_and_closed() {
    let fixture = fixture(16, 16);
    let mut client = fixture.open(4096);
    client
        .send(&ClientFrame::Hello {
            version: WIRE2_VERSION + 1,
            client: "tui".to_string(),
        })
        .await;
    let heard = (client.recv().await, client.recv().await);
    let reason = format!(
        "this runtime speaks wire version {WIRE2_VERSION} and the client {}: restart the client",
        WIRE2_VERSION + 1
    );
    assert_eq!(heard, (Some(ServerFrame::Closing { reason }), None));
}

/// The race between replay and live events, driven through both windows: an
/// event stored after the connection's listener but before the replay reads
/// the log's end (it is in both: sent once), and one stored while the
/// replay reads (it is after the replay's end: sent live).
#[tokio::test]
async fn after_replays_the_matching_log_then_live_events_without_gap_or_duplicate() {
    let fixture = fixture(64, 64);
    for on in [
        "agent/a/stream",
        "agent/b/stream",
        "agent/a/stream",
        "agent/a/tool",
    ] {
        fixture.publish(on).await;
    }
    let mut client = fixture.connect("tui", 1 << 16).await;
    let (at_end_read, read_end) = fixture.log.arm("before");
    let (at_replay_read, read_replay) = fixture.log.arm("since");
    client
        .subscribe("chat", "agent/a/#", Since::After { seq: 1 })
        .await;

    at_end_read.await.expect("the replay reads the log's end");
    fixture.publish("agent/a/stream").await;
    fixture.publish("agent/b/stream").await;
    read_end.send(()).expect("the replay waits");
    at_replay_read.await.expect("the replay reads the log");
    fixture.publish("agent/a/stream").await;
    read_replay.send(()).expect("the replay waits");
    fixture.publish("agent/a/stream").await;

    let log = fixture.stored().await;
    let expected = vec![
        delivered("chat", &log[2]),
        delivered("chat", &log[3]),
        delivered("chat", &log[4]),
        ServerFrame::Backfilled {
            sub_id: "chat".to_string(),
            through_seq: 6,
        },
        delivered("chat", &log[6]),
        delivered("chat", &log[7]),
    ];
    assert_eq!(client.recv_n(expected.len()).await, expected);
}

#[tokio::test]
async fn tail_replays_the_last_matching_events_then_live_ones() {
    let fixture = fixture(64, 64);
    for on in [
        "agent/a/stream",
        "agent/a/stream",
        "agent/b/stream",
        "agent/a/stream",
    ] {
        fixture.publish(on).await;
    }
    let mut client = fixture.connect("tui", 1 << 16).await;
    client
        .subscribe("chat", "agent/a/#", Since::Tail { count: 2 })
        .await;
    let replayed = client.recv_n(3).await;
    fixture.publish("agent/a/stream").await;
    let live = client.recv().await;

    let log = fixture.stored().await;
    assert_eq!(
        (replayed, live),
        (
            vec![
                delivered("chat", &log[1]),
                delivered("chat", &log[3]),
                ServerFrame::Backfilled {
                    sub_id: "chat".to_string(),
                    through_seq: 4,
                },
            ],
            Some(delivered("chat", &log[4])),
        )
    );
}

/// A client that stops reading holds up neither the bus nor another client,
/// and once it reads again it gets every event, in order, from the log.
#[tokio::test]
async fn a_slow_client_blocks_neither_publishing_nor_other_clients_and_catches_up() {
    let fixture = fixture(2, 1);
    let mut slow = fixture.connect("slow", 256).await;
    let mut fast = fixture.connect("fast", 1 << 16).await;
    let backfilled = |sub_id: &str| ServerFrame::Backfilled {
        sub_id: sub_id.to_string(),
        through_seq: 0,
    };
    slow.subscribe("slow", "agent/#", Since::Now).await;
    assert_eq!(slow.recv().await, Some(backfilled("slow")));
    fast.subscribe("fast", "agent/#", Since::Now).await;
    assert_eq!(fast.recv().await, Some(backfilled("fast")));

    let mut heard_fast = Vec::new();
    for _ in 0..40 {
        let published = fixture.publish("agent/a/stream").await;
        heard_fast.push((fast.recv().await, published.clone()));
    }
    let heard_slow = slow.recv_n(40).await;

    let log = fixture.stored().await;
    assert_eq!(
        (heard_fast, heard_slow),
        (
            log.iter()
                .map(|event| (Some(delivered("fast", event)), event.clone()))
                .collect(),
            log.iter().map(|event| delivered("slow", event)).collect(),
        )
    );
}

#[tokio::test]
async fn a_publish_is_the_owners_on_a_session_input_and_refused_elsewhere() {
    let fixture = fixture(16, 16);
    let mut client = fixture.connect("prime-agent-tui/0.10", 4096).await;
    let payload = json!({ "kind": "prompt", "message": "hi" });
    for (id, on) in [("p1", "agent/s1/input"), ("p2", "agent/s1/stream")] {
        client
            .send(&ClientFrame::Publish {
                id: id.to_string(),
                topic: topic(on),
                payload: payload.clone(),
            })
            .await;
    }
    let replies = client.recv_n(2).await;
    let stored: Vec<NewEvent> = fixture
        .stored()
        .await
        .into_iter()
        .map(|event| event.event)
        .collect();
    assert_eq!(
        (replies, stored),
        (
            vec![
                ServerFrame::Reply {
                    id: "p1".to_string(),
                    outcome: Outcome::Ok {
                        result: json!({ "seq": 1 })
                    },
                },
                ServerFrame::Reply {
                    id: "p2".to_string(),
                    outcome: Outcome::Error {
                        message: "clients may publish only on agent/+/input, not agent/s1/stream"
                            .to_string()
                    },
                },
            ],
            vec![NewEvent {
                topic: topic("agent/s1/input"),
                source: "prime-agent-tui/0.10".to_string(),
                session: Some("s1".to_string()),
                payload,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            }],
        )
    );
}

#[tokio::test]
async fn queries_and_calls_reach_their_handler_and_an_unknown_name_is_refused() {
    let fixture = fixture(16, 16);
    let mut client = fixture.connect("tui", 4096).await;
    let args = json!({ "session": "s1" });
    for frame in [
        ClientFrame::Query {
            id: "q1".to_string(),
            name: "transcript.page".to_string(),
            args: args.clone(),
        },
        ClientFrame::Query {
            id: "q2".to_string(),
            name: "transcripts".to_string(),
            args: Value::Null,
        },
        ClientFrame::Call {
            id: "c1".to_string(),
            method: "session.rename".to_string(),
            args: args.clone(),
        },
    ] {
        client.send(&frame).await;
    }
    // Each runs on its own task, so replies come in any order.
    let mut replies: Vec<(String, Outcome)> = client
        .recv_n(3)
        .await
        .into_iter()
        .map(|frame| {
            let ServerFrame::Reply { id, outcome } = frame else {
                panic!("not a reply: {frame:?}");
            };
            (id, outcome)
        })
        .collect();
    replies.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        replies,
        vec![
            (
                "c1".to_string(),
                Outcome::Ok {
                    result: json!({ "call": "SessionRename", "args": args })
                }
            ),
            (
                "q1".to_string(),
                Outcome::Ok {
                    result: json!({ "query": "TranscriptPage", "args": args })
                }
            ),
            (
                "q2".to_string(),
                Outcome::Error {
                    message: "this runtime has no query \"transcripts\"".to_string()
                }
            ),
        ]
    );
}
