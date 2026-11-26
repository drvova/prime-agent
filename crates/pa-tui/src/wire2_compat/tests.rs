//! The round-trip oracle: every recorded frame on a mapped topic, published the way the module
//! docs' contract says the runtime publishes it, comes back as the frame the TUI received.

use pa_types::daemon::{DaemonCommand, DaemonOutbound, PromptInput};
use pa_types::event_log::{Event, EventId, NewEvent, Topic, Trust};
use pa_types::wire2::{ClientFrame, Outcome};
use serde_json::{json, Map, Value};

use super::{outbound_from_event, outgoing, response_from_reply, Delivery, Outgoing};

/// The recorded scenarios under `tests/goldens/inbound/`.
const GOLDENS: [&str; 5] = [
    "prompt_stream",
    "eval_cell",
    "compaction",
    "subagent_roster",
    "attach_paging",
];

/// Every frame one golden recorded, in arrival order.
fn recorded_frames(name: &str) -> Vec<Value> {
    let path = format!(
        "{}/tests/goldens/inbound/{name}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{path}: {error}"))
        .lines()
        .map(|line| {
            let mut recorded: Value = serde_json::from_str(line).expect("a recorded line is JSON");
            recorded["frame"].take()
        })
        .collect()
}

/// The runtime's half of the contract: the wire2 event an old frame becomes, or `None` for a
/// frame wire2 does not carry as an event.
fn event_from_outbound(frame: &DaemonOutbound) -> Option<(Event, Delivery)> {
    let event =
        |topic: String, payload: Value, meta: Option<&pa_types::daemon::DaemonEventMeta>| {
            let delivery = match meta.and_then(|meta| meta.replayed) {
                Some(true) => Delivery::Backfill,
                Some(false) | None => Delivery::Live,
            };
            let id = EventId(meta.map_or_else(|| format!("{topic}:0"), |meta| meta.id.clone()));
            let topic = Topic::new(topic).expect("a recorded session id is a topic level");
            let event = Event {
                id,
                seq: if topic.is_live_only() {
                    0
                } else {
                    meta.and_then(|meta| meta.sequence).unwrap_or_default()
                },
                ts: meta
                    .and_then(|meta| crate::agents_view_state::iso_to_unix_ms(&meta.emitted_at))
                    .map_or(0, |ms| {
                        u64::try_from(ms).expect("a recorded time is after 1970")
                    }),
                event: NewEvent {
                    topic,
                    source: "runtime".to_string(),
                    session: None,
                    payload,
                    trust: Trust::Owner,
                    cause: None,
                    reply: None,
                },
            };
            (event, delivery)
        };
    match frame {
        DaemonOutbound::SessionEvent {
            active_session_id,
            event: payload,
            meta,
            ..
        } => {
            // Live only: `seq` 0, the session's counter in the payload's `meta`.
            let mut payload = payload.clone();
            if let Some(sequence) = meta.as_ref().and_then(|meta| meta.sequence) {
                payload
                    .as_object_mut()
                    .expect("a session event is an object")
                    .insert("meta".to_string(), json!({ "sequence": sequence }));
            }
            Some(event(
                format!("agent/{active_session_id}/stream"),
                payload,
                meta.as_ref(),
            ))
        }
        DaemonOutbound::SideQuestionEvent {
            active_session_id,
            event: payload,
            ..
        } => Some(event(
            format!("agent/{active_session_id}/side_question"),
            payload.clone(),
            None,
        )),
        DaemonOutbound::SessionClosed {
            active_session_id,
            reason,
            meta,
            ..
        } => Some(event(
            format!("agent/{active_session_id}/closed"),
            json!({ "reason": reason }),
            meta.as_ref(),
        )),
        DaemonOutbound::RosterUpdate { .. } => {
            let mut payload = serde_json::to_value(frame).expect("a frame serializes");
            payload
                .as_object_mut()
                .expect("a frame is an object")
                .remove("type");
            Some(event("runtime/roster".to_string(), payload, None))
        }
        _ => None,
    }
}

/// The frame the TUI receives through the compat layer: the recorded one without the cursor
/// generation wire2 has no counterpart for (the TUI reads it only when `sequence` is missing).
fn without_cursor(mut frame: Value) -> Value {
    if let Some(meta) = frame.get_mut("meta").and_then(Value::as_object_mut) {
        meta.remove("cursor");
    }
    frame
}

#[test]
fn recorded_streams_round_trip_through_wire2() {
    let mut expected = Vec::new();
    let mut actual = Vec::new();
    for name in GOLDENS {
        for value in recorded_frames(name) {
            let Ok(frame) = serde_json::from_value::<DaemonOutbound>(value.clone()) else {
                continue;
            };
            let Some((event, delivery)) = event_from_outbound(&frame) else {
                continue;
            };
            expected.push((name, without_cursor(value)));
            let back = outbound_from_event(&event, delivery)
                .expect("a mapped event decodes")
                .expect("a mapped topic yields a frame");
            actual.push((
                name,
                serde_json::to_value(back).expect("a frame serializes"),
            ));
        }
    }
    assert!(
        expected.len() > 100,
        "the goldens carry the streams: {}",
        expected.len()
    );
    assert_eq!(actual, expected);
}

#[test]
fn closes_and_side_questions_round_trip_and_other_topics_are_ignored() {
    let frames = [
        json!({ "type": "session_closed", "activeSessionId": "s1", "reason": "killed",
                "meta": { "id": "s1:9", "protocol": { "name": "prime-agent.daemon", "version": 7 },
                          "activeSessionId": "s1", "sequence": 9,
                          "emittedAt": "2026-11-25T00:12:25.013Z", "replayed": true } }),
        json!({ "type": "side_question_event", "activeSessionId": "s1",
                "event": { "type": "side_question_end", "id": "q1" } }),
        // A stream event the runtime sent without the session's counter: no resume point.
        json!({ "type": "session_event", "activeSessionId": "s1",
                "event": { "type": "agent_start" },
                "meta": { "id": "e2", "protocol": { "name": "prime-agent.daemon", "version": 7 },
                          "activeSessionId": "s1", "emittedAt": "2026-11-25T00:12:25.013Z" } }),
    ];
    let back: Vec<Value> = frames
        .iter()
        .map(|value| {
            let frame: DaemonOutbound = serde_json::from_value(value.clone()).expect("frame");
            let (event, delivery) = event_from_outbound(&frame).expect("mapped");
            serde_json::to_value(outbound_from_event(&event, delivery).unwrap().unwrap()).unwrap()
        })
        .collect();
    let unmapped = Event {
        id: EventId("e1".to_string()),
        seq: 1,
        ts: 0,
        event: NewEvent {
            topic: Topic::new("source/inbox/received").unwrap(),
            source: "inbox".to_string(),
            session: None,
            payload: json!({}),
            trust: Trust::Untrusted,
            cause: None,
            reply: None,
        },
    };
    assert_eq!(
        (
            back,
            outbound_from_event(&unmapped, Delivery::Live).unwrap()
        ),
        (frames.to_vec(), None)
    );
}

/// A fault stored while no client listened (the crash a daemon's death leaves, recorded at the
/// next boot) reaches a client that attaches later as the frame the stream mirrored live; live,
/// the stream's own copy is the frame and the stored one is ignored.
#[test]
fn a_stored_fault_replays_as_its_session_event_and_is_ignored_live() {
    let crashed = json!({ "type": "session_crashed", "kind": "daemon_restart",
        "message": "the daemon stopped", "location": "", "thread": "", "at": "2026-11-26T00:00:00.000Z" });
    let fault = |topic: &str, seq: u64| Event {
        id: EventId(format!("e{seq}")),
        seq,
        ts: 1_795_700_000_000,
        event: NewEvent {
            topic: Topic::new(topic).unwrap(),
            source: "s1".to_string(),
            session: Some("s1".to_string()),
            payload: if topic.ends_with("crashed") {
                crashed.clone()
            } else {
                json!({ "type": "session_recovered", "failures": 1, "queuedInputSuspended": false })
            },
            trust: Trust::Owner,
            cause: None,
            reply: None,
        },
    };
    let frame = |event: &Event, delivery| {
        outbound_from_event(event, delivery)
            .unwrap()
            .map(|frame| serde_json::to_value(frame).unwrap())
    };
    let meta = |id: &str| {
        json!({ "id": id, "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "activeSessionId": "s1", "emittedAt": "2026-11-26T13:33:20.000Z", "replayed": true })
    };
    assert_eq!(
        [
            frame(&fault("agent/s1/crashed", 4), Delivery::Backfill),
            frame(&fault("agent/s1/recovered", 5), Delivery::Backfill),
            frame(&fault("agent/s1/crashed", 6), Delivery::Live),
        ],
        [
            Some(
                json!({ "type": "session_event", "activeSessionId": "s1", "event": crashed,
                         "meta": meta("e4") })
            ),
            Some(json!({ "type": "session_event", "activeSessionId": "s1",
                         "event": { "type": "session_recovered", "failures": 1,
                                    "queuedInputSuspended": false },
                         "meta": meta("e5") })),
            None,
        ]
    );
}

#[test]
fn input_commands_publish_on_the_session_input_topic() {
    let input: PromptInput = serde_json::from_value(json!({
        "images": [{ "type": "image", "data": "AA==", "mimeType": "image/png" }],
        "streamingBehavior": "followUp",
    }))
    .expect("prompt input");
    let plain: PromptInput = serde_json::from_value(json!({})).expect("prompt input");
    let commands = [
        DaemonCommand::Prompt {
            id: None,
            active_session_id: "s1".to_string(),
            message: "hi".to_string(),
            input: input.clone(),
            rest: Map::new(),
        },
        DaemonCommand::Steer {
            id: None,
            active_session_id: "s1".to_string(),
            message: "stop that".to_string(),
            input: plain,
            rest: Map::new(),
        },
        DaemonCommand::FollowUp {
            id: None,
            active_session_id: "s1".to_string(),
            message: "then this".to_string(),
            input,
            rest: Map::new(),
        },
    ];
    // The runtime's half: the publish read back as the command it carries.
    let read_back = |frame: ClientFrame| {
        let ClientFrame::Publish { topic, payload, .. } = frame else {
            panic!("an input command publishes: {frame:?}");
        };
        let session = topic.levels().nth(1).expect("agent/<id>/input").to_string();
        let mut payload = payload.as_object().cloned().expect("an object payload");
        let kind = payload.remove("kind").expect("the input kind");
        payload.insert("type".to_string(), kind);
        payload.insert("activeSessionId".to_string(), Value::from(session));
        (
            topic.as_str().to_string(),
            serde_json::from_value::<DaemonCommand>(Value::Object(payload)).expect("a command"),
        )
    };
    let published: Vec<(String, DaemonCommand)> = commands
        .iter()
        .enumerate()
        .map(
            |(index, command)| match outgoing(format!("p{index}"), command.clone()).unwrap() {
                Outgoing::Publish(frame) => read_back(frame),
                Outgoing::Query(frame) => panic!("a query: {frame:?}"),
                Outgoing::Unmapped(command) => panic!("unmapped: {command:?}"),
            },
        )
        .collect();
    let detach = DaemonCommand::Detach {
        id: None,
        active_session_id: Some("s1".to_string()),
        rest: Map::new(),
    };
    assert_eq!(
        (
            published,
            outgoing("d".to_string(), detach.clone()).unwrap()
        ),
        (
            commands
                .iter()
                .map(|command| ("agent/s1/input".to_string(), command.clone()))
                .collect(),
            Outgoing::Unmapped(Box::new(detach))
        )
    );
}

/// The session reads go out as their queries, each argument the command carries forwarded and
/// none it lacks; every recorded reply to one comes back from its query's result as the response
/// the TUI received, and an error reply as the old failure.
#[test]
fn session_reads_query_and_their_replies_round_trip() {
    let capabilities = vec!["windowed_snapshot".to_string()];
    let commands = [
        DaemonCommand::Attach {
            id: None,
            active_session_id: "s1".to_string(),
            client_id: Some("tui:1".to_string()),
            capabilities: Some(capabilities.clone()),
            resume_cursor: None,
            telemetry_disabled: Some(true),
            recovery_config: None,
            env: None,
            launch_env: None,
            rest: Map::new(),
        },
        DaemonCommand::GetState {
            id: None,
            active_session_id: "s1".to_string(),
            rest: Map::new(),
        },
        DaemonCommand::GetMessages {
            id: None,
            active_session_id: "s1".to_string(),
            page: Some(pa_types::daemon::TranscriptPage::Start {
                after: None,
                until: Some("u5".to_string()),
                limit: 256,
            }),
            capabilities: Some(capabilities),
            rest: Map::from_iter([("historyLeafId".to_string(), json!("a9"))]),
        },
        DaemonCommand::GetMessages {
            id: None,
            active_session_id: "s1".to_string(),
            page: None,
            capabilities: None,
            rest: Map::new(),
        },
    ];
    let queries: Vec<ClientFrame> = commands
        .into_iter()
        .map(
            |command| match outgoing("q".to_string(), command).unwrap() {
                Outgoing::Query(frame) => frame,
                other => panic!("not a query: {other:?}"),
            },
        )
        .collect();
    let query = |name: &str, args: Value| ClientFrame::Query {
        id: "q".to_string(),
        name: name.to_string(),
        args,
    };
    assert_eq!(
        queries,
        [
            query(
                "session.snapshot",
                json!({ "session": "s1", "shape": "attach", "clientId": "tui:1",
                        "capabilities": ["windowed_snapshot"] })
            ),
            query(
                "session.snapshot",
                json!({ "session": "s1", "shape": "state" })
            ),
            query(
                "transcript.page",
                json!({ "session": "s1", "shape": "messages",
                        "page": { "from": "start", "until": "u5", "limit": 256 },
                        "historyLeafId": "a9", "capabilities": ["windowed_snapshot"] })
            ),
            query(
                "transcript.page",
                json!({ "session": "s1", "shape": "messages" })
            ),
        ]
    );
    let mut expected = Vec::new();
    let mut actual = Vec::new();
    for name in GOLDENS {
        for frame in recorded_frames(name) {
            let command = frame["command"].as_str().unwrap_or_default();
            if frame["type"] != "response"
                || !matches!(command, "attach" | "get_state" | "get_messages")
            {
                continue;
            }
            let back = response_from_reply(
                command,
                frame["id"].as_str().map(str::to_string),
                Outcome::Ok {
                    result: frame["data"].clone(),
                },
            );
            actual.push(serde_json::to_value(back).unwrap());
            expected.push(frame);
        }
    }
    assert_eq!(
        expected.len(),
        11,
        "every golden attaches and reads its state"
    );
    let failed = response_from_reply(
        "get_messages",
        None,
        Outcome::Error {
            message: "History branch changed".to_string(),
        },
    );
    actual.push(serde_json::to_value(failed).unwrap());
    expected.push(
        json!({ "type": "response", "command": "get_messages", "success": false,
                          "error": "History branch changed" }),
    );
    assert_eq!(actual, expected);
}
