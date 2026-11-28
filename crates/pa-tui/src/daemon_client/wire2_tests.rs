//! The wire2 link against a scripted runtime: the attach window and the reconnect's resume.

use std::path::Path;

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::event_log::{Event, EventId, NewEvent, Topic, Trust};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use super::{use_transport, DaemonClient, DaemonClientEvent, Transport};
use pa_types::daemon::DaemonCommand;

/// One scripted runtime connection: what the client sent, and a writer for what it gets back.
struct Served {
    frames: mpsc::UnboundedReceiver<ClientFrame>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Served {
    async fn send(&mut self, frame: &ServerFrame) {
        let bytes = encode_private_frame(
            &serde_json::to_value(frame).unwrap(),
            &[],
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .unwrap();
        self.writer.write_all(&bytes).await.unwrap();
    }

    /// The next frame the client sent that `wanted` picks, answering the handshake's on the way:
    /// the welcome, the identity, the connection's subscriptions and the history-import query.
    async fn next(&mut self, wanted: impl Fn(&ClientFrame) -> bool) -> ClientFrame {
        loop {
            let frame = self.frames.recv().await.expect("the client's frame");
            if wanted(&frame) {
                return frame;
            }
            match &frame {
                ClientFrame::Hello { .. } => {
                    self.send(&ServerFrame::Welcome {
                        version: WIRE2_VERSION,
                        runtime_version: "0.0.0".to_string(),
                        last_seq: 3,
                    })
                    .await;
                }
                ClientFrame::Query { id, .. } => {
                    let reply = ServerFrame::Reply {
                        id: id.clone(),
                        outcome: Outcome::Ok {
                            result: Value::Null,
                        },
                    };
                    self.send(&reply).await;
                }
                ClientFrame::Subscribe { sub_id, .. } => {
                    let backfilled = ServerFrame::Backfilled {
                        sub_id: sub_id.clone(),
                        through_seq: 3,
                    };
                    self.send(&backfilled).await;
                }
                ClientFrame::Unsubscribe { .. }
                | ClientFrame::Publish { .. }
                | ClientFrame::Call { .. } => {}
            }
        }
    }
}

/// Accept the next connection on `listener` and read its frames.
async fn serve(listener: &UnixListener) -> Served {
    let (stream, _) = listener.accept().await.unwrap();
    let (reader, writer) = stream.into_split();
    let (tx, frames) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut reader = PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
        while let Ok(Some(frame)) = reader.read_frame().await {
            if tx
                .send(serde_json::from_value(frame.header).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    Served { frames, writer }
}

fn event(topic: &str, seq: u64, payload: Value) -> Event {
    Event {
        id: EventId(format!("e{seq}")),
        seq,
        ts: 0,
        event: NewEvent {
            topic: Topic::new(topic).unwrap(),
            source: "runtime".to_string(),
            session: None,
            payload,
            trust: Trust::Owner,
            cause: None,
            reply: None,
        },
    }
}

fn attach(session: &str) -> DaemonCommand {
    serde_json::from_value(json!({ "type": "attach", "activeSessionId": session })).unwrap()
}

fn is_attach_call(frame: &ClientFrame) -> bool {
    matches!(frame, ClientFrame::Call { method, .. } if method == "session.attach")
}

fn connect(
    socket: &Path,
) -> tokio::task::JoinHandle<(DaemonClient, mpsc::UnboundedReceiver<DaemonClientEvent>)> {
    let socket = socket.to_path_buf();
    tokio::spawn(async move { DaemonClient::connect(&socket).await.unwrap() })
}

/// The attach window: the session's events that arrive before the attach's reply are held and
/// released after it, without the ones the snapshot already holds (a sequence at or below its
/// `lastEventSequence`).
#[tokio::test]
async fn an_attach_holds_the_session_events_and_drops_what_its_snapshot_holds() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("daemon2.sock");
    use_transport(&socket, Transport::Wire2);
    let listener = UnixListener::bind(&socket).unwrap();
    let connecting = connect(&socket);
    let mut served = serve(&listener).await;
    served
        .next(|frame| matches!(frame, ClientFrame::Query { name, .. } if name == "daemon.identity"))
        .await;
    served
        .send(&ServerFrame::Reply {
            id: "compat:identity".to_string(),
            outcome: Outcome::Ok { result: json!({}) },
        })
        .await;
    let (client, mut events) = connecting.await.unwrap();
    let attaching = tokio::spawn(async move { client.request_ok(attach("s1")).await });
    let ClientFrame::Call { id, .. } = served.next(is_attach_call).await else {
        unreachable!("an attach call")
    };
    let stream = |sequence: u64| {
        event(
            "agent/s1/stream",
            0,
            json!({ "type": "turn_end", "meta": { "sequence": sequence } }),
        )
    };
    for sequence in [5, 7] {
        served
            .send(&ServerFrame::Event {
                sub_id: "compat:agent:s1".to_string(),
                event: stream(sequence),
            })
            .await;
    }
    served
        .send(&ServerFrame::Reply {
            id,
            outcome: Outcome::Ok {
                result: json!({ "activeSessionId": "s1", "lastEventSequence": 6 }),
            },
        })
        .await;
    let attached = attaching.await.unwrap().unwrap();
    let released = events.recv().await;
    assert_eq!(
        (
            attached["lastEventSequence"].clone(),
            released.map(|event| match event {
                DaemonClientEvent::SessionEvent { meta_sequence, .. } => meta_sequence,
                other => panic!("not a session event: {other:?}"),
            }),
            events.try_recv().is_err(),
        ),
        (json!(6), Some(7), true)
    );
}

/// A reconnect resumes the session's stored topics after the last stored event the previous
/// connection saw, so a fault or close that happened while it was away is replayed.
#[tokio::test]
async fn a_reconnect_follows_the_session_after_the_last_stored_event_seen() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("daemon2.sock");
    use_transport(&socket, Transport::Wire2);
    let listener = UnixListener::bind(&socket).unwrap();
    let mut subscriptions = Vec::new();
    for connection in 0..2 {
        let connecting = connect(&socket);
        let mut served = serve(&listener).await;
        served
            .next(|frame| matches!(frame, ClientFrame::Query { name, .. } if name == "daemon.identity"))
            .await;
        served
            .send(&ServerFrame::Reply {
                id: "compat:identity".to_string(),
                outcome: Outcome::Ok { result: json!({}) },
            })
            .await;
        let (client, mut events) = connecting.await.unwrap();
        let attaching = {
            let client = client.clone();
            tokio::spawn(async move { client.request_ok(attach("s1")).await })
        };
        let subscription = served
            .next(|frame| matches!(frame, ClientFrame::Subscribe { sub_id, .. } if sub_id == "compat:agent:s1"))
            .await;
        if let ClientFrame::Subscribe { since, .. } = &subscription {
            subscriptions.push(*since);
        }
        served
            .send(&ServerFrame::Backfilled {
                sub_id: "compat:agent:s1".to_string(),
                through_seq: 3,
            })
            .await;
        let ClientFrame::Call { id, .. } = served.next(is_attach_call).await else {
            unreachable!("an attach call")
        };
        served
            .send(&ServerFrame::Reply {
                id,
                outcome: Outcome::Ok {
                    result: json!({ "activeSessionId": "s1", "lastEventSequence": 0 }),
                },
            })
            .await;
        attaching.await.unwrap().unwrap();
        if connection == 0 {
            // The session is closed while this client follows it; then the connection ends.
            served
                .send(&ServerFrame::Event {
                    sub_id: "compat:agent:s1".to_string(),
                    event: event("agent/s1/closed", 41, json!({ "reason": "killed" })),
                })
                .await;
            assert!(matches!(
                events.recv().await,
                Some(DaemonClientEvent::SessionClosed { .. })
            ));
            client.close();
        }
    }
    assert_eq!(
        subscriptions,
        vec![Since::After { seq: 3 }, Since::After { seq: 41 }]
    );
}

/// A client dropped without `close` ends its connection, as a dropped old-wire client does: the
/// runtime reads the connection's end, so it holds nothing (an attach, a question's waiter) for a
/// client that is gone.
#[tokio::test]
async fn a_dropped_client_ends_its_connection() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("daemon2.sock");
    use_transport(&socket, Transport::Wire2);
    let listener = UnixListener::bind(&socket).unwrap();
    let connecting = connect(&socket);
    let mut served = serve(&listener).await;
    served
        .next(|frame| matches!(frame, ClientFrame::Query { name, .. } if name == "daemon.identity"))
        .await;
    served
        .send(&ServerFrame::Reply {
            id: "compat:identity".to_string(),
            outcome: Outcome::Ok { result: json!({}) },
        })
        .await;
    let (client, events) = connecting.await.unwrap();
    drop((client, events));
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while served.frames.recv().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the connection outlived its dropped client");
}
