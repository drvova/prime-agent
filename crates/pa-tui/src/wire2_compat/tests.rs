//! The round-trip oracle: every recorded frame on a mapped topic, published the way the module
//! docs' contract says the runtime publishes it, comes back as the frame the TUI received.

use pa_types::daemon::{DaemonCommand, DaemonOutbound, PromptInput};
use pa_types::event_log::{Event, EventId, NewEvent, Topic, Trust};
use pa_types::wire2::ClientFrame;
use serde_json::{json, Map, Value};

use super::{outbound_from_event, outgoing, Delivery, Outgoing};

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
