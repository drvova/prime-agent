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
        DaemonOutbound::AgentTraffic { entry, .. } => Some(event(
            "runtime/traffic".to_string(),
            serde_json::to_value(entry).expect("an entry serializes"),
            None,
        )),
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
                other => panic!("not a publish: {other:?}"),
            },
        )
        .collect();
    // A detach that names no session stays on the old wire.
    let detach = DaemonCommand::Detach {
        id: None,
        active_session_id: None,
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

/// The session reads go out as their queries (the attach as its call), each argument the command carries forwarded and
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
                Outgoing::Query(frame) | Outgoing::Call(frame) => frame,
                other => panic!("not a query or call: {other:?}"),
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
            // The attach is a call: it attaches the connection's client.
            ClientFrame::Call {
                id: "q".to_string(),
                method: "session.attach".to_string(),
                args: json!({ "session": "s1", "clientId": "tui:1",
                              "capabilities": ["windowed_snapshot"] }),
            },
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
            info: None,
        },
    );
    actual.push(serde_json::to_value(failed).unwrap());
    expected.push(
        json!({ "type": "response", "command": "get_messages", "success": false,
                          "error": "History branch changed" }),
    );
    assert_eq!(actual, expected);
}

/// The agents view's feeds: a subscribe follows the feed's live-only topic from now and queries
/// its state, whose reply is the old response (the recorded `roster_subscribe` reply round trips);
/// an unsubscribe is answered by the layer as the old daemon answered it; a traffic entry on its
/// topic comes back as the `agent_traffic` frame.
#[test]
fn the_feeds_follow_their_topics_and_their_frames_round_trip() {
    let command = |kind: &str| -> DaemonCommand {
        serde_json::from_value(json!({ "type": kind })).expect("a feed command")
    };
    let follow = |sub_id: &str, topic: &str, query: &str| Outgoing::Follow {
        subscribe: ClientFrame::Subscribe {
            sub_id: sub_id.to_string(),
            filter: pa_types::event_log::TopicFilter::new(topic).unwrap(),
            since: pa_types::wire2::Since::Now,
        },
        query: ClientFrame::Query {
            id: "f".to_string(),
            name: query.to_string(),
            args: json!({}),
        },
    };
    let unfollow = |sub_id: &str, kind: &str| Outgoing::Unfollow {
        unsubscribe: ClientFrame::Unsubscribe {
            sub_id: sub_id.to_string(),
        },
        response: Box::new(
            serde_json::from_value(
                json!({ "type": "response", "id": "f", "command": kind, "success": true }),
            )
            .unwrap(),
        ),
    };
    let kinds = [
        "roster_subscribe",
        "agent_traffic_subscribe",
        "roster_unsubscribe",
        "agent_traffic_unsubscribe",
    ];
    let mapped: Vec<Outgoing> = kinds
        .iter()
        .map(|kind| outgoing("f".to_string(), command(kind)).unwrap())
        .collect();
    let recorded = recorded_frames("subagent_roster")
        .into_iter()
        .find(|frame| frame["type"] == "response" && frame["command"] == "roster_subscribe")
        .expect("the recording subscribed to the roster");
    let replied = response_from_reply(
        "roster_subscribe",
        recorded["id"].as_str().map(str::to_string),
        Outcome::Ok {
            result: recorded["data"].clone(),
        },
    );
    let traffic = json!({ "type": "agent_traffic", "entry": {
        "rootActiveSessionId": "root", "from": "alpha", "fromActiveSessionId": "c1",
        "to": "root", "toActiveSessionId": "root", "relationship": "child", "kind": "direct",
        "outcome": "delivered", "chars": 5, "preview": "hello", "timestampMs": 1 } });
    let frame: DaemonOutbound = serde_json::from_value(traffic.clone()).unwrap();
    let (event, delivery) = event_from_outbound(&frame).expect("traffic is an event");
    assert_eq!(
        (
            mapped,
            serde_json::to_value(replied).unwrap(),
            event.event.topic.as_str().to_string(),
            serde_json::to_value(outbound_from_event(&event, delivery).unwrap().unwrap()).unwrap(),
        ),
        (
            vec![
                follow("compat:roster", "runtime/roster", "runtime.roster"),
                follow("compat:traffic", "runtime/traffic", "runtime.traffic"),
                unfollow("compat:roster", "roster_unsubscribe"),
                unfollow("compat:traffic", "agent_traffic_unsubscribe"),
            ],
            recorded,
            "runtime/traffic".to_string(),
            traffic,
        )
    );
}

/// `list_saved_sessions` streams on its request's topic: the subscription, the saved-shape query
/// with the command's arguments, and the unsubscribe at its reply; the topic's row and progress
/// events come back as the old frames, tagged with the request's id.
#[test]
fn the_saved_session_list_streams_on_its_request_topic_and_its_frames_round_trip() {
    let command: DaemonCommand = serde_json::from_value(json!({
        "type": "list_saved_sessions", "cwd": "/work", "scope": "current", "limit": 2,
        "cursor": "snapshot:c:2",
    }))
    .unwrap();
    let item = json!({
        "type": "session_list_item", "id": "l1", "command": "list_saved_sessions",
        "session": { "path": "/s/a.jsonl", "id": "a", "cwd": "/work", "created": "c",
            "modified": "m", "messageCount": 2, "firstMessage": "hi", "rlmDepth": 0 },
    });
    let progress = json!({
        "type": "session_list_progress", "id": "l1", "command": "list_saved_sessions",
        "loaded": 1, "total": 3,
    });
    let without_type = |frame: &Value| {
        let mut payload = frame.clone();
        payload.as_object_mut().unwrap().remove("type");
        payload
    };
    let frames: Vec<Value> = [&item, &progress]
        .into_iter()
        .map(|frame| {
            let event = Event {
                id: EventId("live-1".to_string()),
                seq: 0,
                ts: 0,
                event: NewEvent {
                    topic: Topic::new("runtime/session_list/l1").unwrap(),
                    source: "runtime".to_string(),
                    session: None,
                    payload: without_type(frame),
                    trust: Trust::Owner,
                    cause: None,
                    reply: None,
                },
            };
            serde_json::to_value(
                outbound_from_event(&event, Delivery::Live)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        (outgoing("l1".to_string(), command).unwrap(), frames),
        (
            Outgoing::Stream {
                subscribe: ClientFrame::Subscribe {
                    sub_id: "compat:list:l1".to_string(),
                    filter: pa_types::event_log::TopicFilter::new("runtime/session_list/l1")
                        .unwrap(),
                    since: pa_types::wire2::Since::Now,
                },
                query: ClientFrame::Query {
                    id: "l1".to_string(),
                    name: "sessions.list".to_string(),
                    args: json!({ "shape": "saved", "stream": "l1", "cwd": "/work",
                        "scope": "current", "limit": 2, "cursor": "snapshot:c:2" }),
                },
                unsubscribe: ClientFrame::Unsubscribe {
                    sub_id: "compat:list:l1".to_string(),
                },
            },
            vec![item, progress],
        )
    );
}

/// The setting switches go out as their calls, each with the command's own members and its
/// session; a switch the old daemon answered without `data` comes back without it, a cycle's
/// `null` stays `null`.
#[test]
fn the_setting_switches_call_and_their_replies_round_trip() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let calls: Vec<ClientFrame> = [
        json!({ "type": "cycle_model", "activeSessionId": "s1", "direction": "backward" }),
        json!({ "type": "cycle_thinking_level", "activeSessionId": "s1" }),
        json!({ "type": "set_scoped_models", "activeSessionId": "s1",
                "scopedModels": [{ "model": { "provider": "p", "id": "m" } }] }),
        json!({ "type": "set_service_tier", "id": "x", "activeSessionId": "s1",
                "serviceTier": "priority" }),
        json!({ "type": "set_transport", "activeSessionId": "s1", "transport": "sse" }),
        json!({ "type": "set_steering_mode", "activeSessionId": "s1", "mode": "all" }),
        json!({ "type": "set_follow_up_mode", "activeSessionId": "s1", "mode": "all" }),
        json!({ "type": "set_auto_compaction", "activeSessionId": "s1", "enabled": false }),
        json!({ "type": "set_auto_retry", "activeSessionId": "s1", "enabled": true }),
    ]
    .into_iter()
    .map(
        |value| match outgoing("c".to_string(), command(value)).unwrap() {
            Outgoing::Call(frame) => frame,
            other => panic!("not a call: {other:?}"),
        },
    )
    .collect();
    let call = |method: &str, args: Value| ClientFrame::Call {
        id: "c".to_string(),
        method: method.to_string(),
        args,
    };
    assert_eq!(
        calls,
        [
            call(
                "session.cycle_model",
                json!({ "session": "s1", "direction": "backward" })
            ),
            call("session.cycle_effort", json!({ "session": "s1" })),
            call(
                "session.set_scoped_models",
                json!({ "session": "s1",
                        "scopedModels": [{ "model": { "provider": "p", "id": "m" } }] })
            ),
            call(
                "session.set_service_tier",
                json!({ "session": "s1", "serviceTier": "priority" })
            ),
            call(
                "session.set_transport",
                json!({ "session": "s1", "transport": "sse" })
            ),
            call(
                "session.set_steering_mode",
                json!({ "session": "s1", "mode": "all" })
            ),
            call(
                "session.set_follow_up_mode",
                json!({ "session": "s1", "mode": "all" })
            ),
            call(
                "session.set_auto_compaction",
                json!({ "session": "s1", "enabled": false })
            ),
            call(
                "session.set_auto_retry",
                json!({ "session": "s1", "enabled": true })
            ),
        ]
    );
    let replied = |command: &str| {
        serde_json::to_value(response_from_reply(
            command,
            None,
            Outcome::Ok {
                result: Value::Null,
            },
        ))
        .unwrap()
    };
    assert_eq!(
        [replied("set_transport"), replied("cycle_model")],
        [
            json!({ "type": "response", "command": "set_transport", "success": true }),
            json!({ "type": "response", "command": "cycle_model", "success": true,
                    "data": null }),
        ]
    );
}

/// The reads beside a session's turns go out as their queries (`get_usage` on no session), the
/// reload and the warning's marker as their calls, each with the command's own members; a
/// `runtime/models` event comes back as `model_catalog_changed`.
#[test]
fn the_session_info_reads_query_and_the_catalog_change_comes_back() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "get_commands", "activeSessionId": "s1" }),
        json!({ "type": "get_mcp_connections", "id": "x", "activeSessionId": "s1" }),
        json!({ "type": "get_model_catalog", "activeSessionId": "s1" }),
        json!({ "type": "get_usage", "section": "quotas", "forceRefresh": true }),
        json!({ "type": "reload", "activeSessionId": "s1" }),
        json!({ "type": "mark_anthropic_warning_shown", "activeSessionId": "s1" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let query = |name: &str, args: Value| {
        Outgoing::Query(ClientFrame::Query {
            id: "i".to_string(),
            name: name.to_string(),
            args,
        })
    };
    let call = |method: &str| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args: json!({ "session": "s1" }),
        })
    };
    let changed = Event {
        id: EventId("live-3".to_string()),
        seq: 0,
        ts: 0,
        event: NewEvent {
            topic: Topic::new("runtime/models").unwrap(),
            source: "runtime".to_string(),
            session: None,
            payload: json!({}),
            trust: Trust::Owner,
            cause: None,
            reply: None,
        },
    };
    let marked = serde_json::to_value(response_from_reply(
        "mark_anthropic_warning_shown",
        None,
        Outcome::Ok {
            result: Value::Null,
        },
    ))
    .unwrap();
    assert_eq!(
        (
            frames,
            serde_json::to_value(outbound_from_event(&changed, Delivery::Live).unwrap()).unwrap(),
            marked
        ),
        (
            vec![
                query("session.commands", json!({ "session": "s1" })),
                query("session.mcp_connections", json!({ "session": "s1" })),
                query("session.model_catalog", json!({ "session": "s1" })),
                query(
                    "usage",
                    json!({ "section": "quotas", "forceRefresh": true })
                ),
                call("session.reload"),
                call("session.mark_anthropic_warning_shown"),
            ],
            json!({ "type": "model_catalog_changed" }),
            json!({ "type": "response", "command": "mark_anthropic_warning_shown",
                    "success": true }),
        )
    );
}

#[test]
fn the_tree_reads_query_and_the_tree_moves_call() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "get_user_messages_for_forking", "activeSessionId": "s1" }),
        json!({ "type": "get_last_assistant_text", "activeSessionId": "s1" }),
        json!({ "type": "navigate_tree", "activeSessionId": "s1", "targetId": "e1",
                "summarize": true, "restoreFiles": true }),
        json!({ "type": "abort_branch_summary", "activeSessionId": "s1" }),
        json!({ "type": "export_html", "activeSessionId": "s1", "outputPath": "/tmp/s.html" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let query = |name: &str| {
        Outgoing::Query(ClientFrame::Query {
            id: "i".to_string(),
            name: name.to_string(),
            args: json!({ "session": "s1" }),
        })
    };
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let aborted = serde_json::to_value(response_from_reply(
        "abort_branch_summary",
        None,
        Outcome::Ok {
            result: Value::Null,
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, aborted),
        (
            vec![
                query("session.fork_messages"),
                query("session.last_assistant_text"),
                call(
                    "session.navigate_tree",
                    json!({ "session": "s1", "targetId": "e1", "summarize": true,
                            "restoreFiles": true })
                ),
                call("session.abort_branch_summary", json!({ "session": "s1" })),
                call(
                    "session.export_html",
                    json!({ "session": "s1", "outputPath": "/tmp/s.html" })
                ),
            ],
            json!({ "type": "response", "command": "abort_branch_summary", "success": true }),
        )
    );
}

#[test]
fn the_replacements_call_and_their_refusal_keeps_its_error_info() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "fork", "activeSessionId": "s1", "entryId": "e1", "position": "at" }),
        json!({ "type": "import_jsonl", "activeSessionId": "s1", "inputPath": "/tmp/in.jsonl",
                "cwdOverride": "/tmp" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let refused = serde_json::to_value(response_from_reply(
        "import_jsonl",
        None,
        Outcome::Error {
            message: "File not found: /tmp/in.jsonl".to_string(),
            info: Some(json!({ "code": "session_import_file_not_found",
                               "filePath": "/tmp/in.jsonl" })),
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, refused),
        (
            vec![
                call(
                    "session.fork_entry",
                    json!({ "session": "s1", "entryId": "e1", "position": "at" })
                ),
                call(
                    "session.import_jsonl",
                    json!({ "session": "s1", "inputPath": "/tmp/in.jsonl", "cwdOverride": "/tmp" })
                ),
            ],
            json!({ "type": "response", "command": "import_jsonl", "success": false,
                    "error": "File not found: /tmp/in.jsonl",
                    "errorInfo": { "code": "session_import_file_not_found",
                                   "filePath": "/tmp/in.jsonl" } }),
        )
    );
}

#[test]
fn the_queue_edit_and_the_aborts_call() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "mutate_queued_message", "activeSessionId": "s1", "lane": "followUp",
                "index": 1, "expectedText": "two", "mutation": { "type": "delete" } }),
        json!({ "type": "abort_compaction", "activeSessionId": "s1" }),
        json!({ "type": "abort_retry", "activeSessionId": "s1" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let edited = serde_json::to_value(response_from_reply(
        "mutate_queued_message",
        None,
        Outcome::Ok {
            result: json!({ "status": "applied" }),
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, edited),
        (
            vec![
                call(
                    "session.mutate_queued_message",
                    json!({ "session": "s1", "lane": "followUp", "index": 1,
                            "expectedText": "two", "mutation": { "type": "delete" } })
                ),
                call("session.abort_compaction", json!({ "session": "s1" })),
                call("session.abort_retry", json!({ "session": "s1" })),
            ],
            json!({ "type": "response", "command": "mutate_queued_message", "success": true,
                    "data": { "status": "applied" } }),
        )
    );
}

/// A session's detach and a question's answer are calls answered without `data`, as the old
/// daemon answered them; an answer's refusal keeps its message.
#[test]
fn the_detach_and_the_answer_call_and_answer_without_data() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "detach", "activeSessionId": "s1" }),
        json!({ "type": "answer_question", "activeSessionId": "s1", "questionId": "q1",
                "reply": { "status": "answered", "answer": ["yes"] } }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let answered = |command: &str, outcome: Outcome| {
        serde_json::to_value(response_from_reply(command, None, outcome)).unwrap()
    };
    assert_eq!(
        (
            frames,
            answered(
                "detach",
                Outcome::Ok {
                    result: Value::Null
                }
            ),
            answered(
                "answer_question",
                Outcome::Ok {
                    result: Value::Null
                }
            ),
            answered(
                "answer_question",
                Outcome::Error {
                    message: "question \"q1\" is not open".to_string(),
                    info: None,
                },
            ),
        ),
        (
            vec![
                call("session.detach", json!({ "session": "s1" })),
                call(
                    "session.answer_question",
                    json!({ "session": "s1", "questionId": "q1",
                            "reply": { "status": "answered", "answer": ["yes"] } })
                ),
            ],
            json!({ "type": "response", "command": "detach", "success": true }),
            json!({ "type": "response", "command": "answer_question", "success": true }),
            json!({ "type": "response", "command": "answer_question", "success": false,
                    "error": "question \"q1\" is not open" }),
        )
    );
}

#[test]
fn the_side_runs_call_and_answer_as_the_old_daemon() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "start_side_question", "activeSessionId": "s1", "sideQuestionId": "q1",
                "question": "why?", "previousTurns": [], "kind": "better" }),
        json!({ "type": "abort_side_question", "activeSessionId": "s1", "sideQuestionId": "q1" }),
        json!({ "type": "record_better_outcome", "activeSessionId": "s1", "outcome": "edited" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let answered = |command: &str, result: Value| {
        serde_json::to_value(response_from_reply(command, None, Outcome::Ok { result })).unwrap()
    };
    assert_eq!(
        (
            frames,
            answered("start_side_question", Value::Null),
            answered("abort_side_question", json!({ "aborted": true })),
            answered("record_better_outcome", Value::Null),
        ),
        (
            vec![
                call(
                    "session.start_side_question",
                    json!({ "session": "s1", "sideQuestionId": "q1", "question": "why?",
                            "previousTurns": [], "kind": "better" })
                ),
                call(
                    "session.abort_side_question",
                    json!({ "session": "s1", "sideQuestionId": "q1" })
                ),
                call(
                    "session.record_better_outcome",
                    json!({ "session": "s1", "outcome": "edited" })
                ),
            ],
            json!({ "type": "response", "command": "start_side_question", "success": true }),
            json!({ "type": "response", "command": "abort_side_question", "success": true,
                    "data": { "aborted": true } }),
            json!({ "type": "response", "command": "record_better_outcome", "success": true }),
        )
    );
}

/// The agents view's kernel activity: a session's kernel shell commands and factory lane are
/// queries and calls on the session, the persistent shells machine-wide ones without one, each
/// answered with the old response's `data`.
#[test]
fn the_kernel_activity_queries_and_calls_carry_their_commands() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "list_kernel_bash", "activeSessionId": "s1" }),
        json!({ "type": "tail_kernel_bash", "activeSessionId": "s1", "activityId": "a1",
                "lines": 20 }),
        json!({ "type": "kill_kernel_bash", "activeSessionId": "s1", "activityId": "a1" }),
        json!({ "type": "factory_activity", "activeSessionId": "s1", "action": "graph" }),
        json!({ "type": "list_persistent_shells" }),
        json!({ "type": "tail_persistent_shell", "shellId": "p1", "lines": 50 }),
        json!({ "type": "kill_persistent_shell", "shellId": "p1" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let query = |name: &str, args: Value| {
        Outgoing::Query(ClientFrame::Query {
            id: "i".to_string(),
            name: name.to_string(),
            args,
        })
    };
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let killed = serde_json::to_value(response_from_reply(
        "kill_persistent_shell",
        None,
        Outcome::Ok {
            result: json!({ "shellId": "p1" }),
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, killed),
        (
            vec![
                query("session.kernel_bash", json!({ "session": "s1" })),
                query(
                    "session.kernel_bash_tail",
                    json!({ "session": "s1", "activityId": "a1", "lines": 20 })
                ),
                call(
                    "session.kill_kernel_bash",
                    json!({ "session": "s1", "activityId": "a1" })
                ),
                call(
                    "session.factory_activity",
                    json!({ "session": "s1", "action": "graph" })
                ),
                query("persistent_shells", json!({})),
                query(
                    "persistent_shells.tail",
                    json!({ "shellId": "p1", "lines": 50 })
                ),
                call("persistent_shells.kill", json!({ "shellId": "p1" })),
            ],
            json!({ "type": "response", "command": "kill_persistent_shell", "success": true,
                    "data": { "shellId": "p1" } }),
        )
    );
}

/// The agents view's session controls: the agent-message ingest's status, pause, resume and
/// clear (on every running session when the command names none), and the RLM depth bound and
/// the subagent delete on the session, each answered with the old response's `data`.
#[test]
fn the_agent_controls_queries_and_calls_carry_their_commands() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "agent_messages_status", "activeSessionId": "s1" }),
        json!({ "type": "agent_messages_status" }),
        json!({ "type": "agent_messages_pause", "activeSessionId": "s1" }),
        json!({ "type": "agent_messages_resume" }),
        json!({ "type": "agent_messages_clear", "activeSessionId": "s1" }),
        json!({ "type": "get_rlm_max_depth_status", "activeSessionId": "s1" }),
        json!({ "type": "set_rlm_max_depth", "activeSessionId": "s1", "maxDepth": 3,
                "global": true }),
        json!({ "type": "delete_rlm_subagent", "activeSessionId": "s1", "childId": "c1" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let query = |name: &str, args: Value| {
        Outgoing::Query(ClientFrame::Query {
            id: "i".to_string(),
            name: name.to_string(),
            args,
        })
    };
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let deleted = serde_json::to_value(response_from_reply(
        "delete_rlm_subagent",
        None,
        Outcome::Ok {
            result: json!({ "deleted": false, "reason": "running" }),
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, deleted),
        (
            vec![
                query("agent_messages", json!({ "session": "s1" })),
                query("agent_messages", json!({})),
                call("agent_messages.pause", json!({ "session": "s1" })),
                call("agent_messages.resume", json!({})),
                call("agent_messages.clear", json!({ "session": "s1" })),
                query("session.rlm_max_depth", json!({ "session": "s1" })),
                call(
                    "session.set_rlm_max_depth",
                    json!({ "session": "s1", "maxDepth": 3, "global": true })
                ),
                call(
                    "session.delete_child",
                    json!({ "session": "s1", "child": "c1" })
                ),
            ],
            json!({ "type": "response", "command": "delete_rlm_subagent", "success": true,
                    "data": { "deleted": false, "reason": "running" } }),
        )
    );
}

/// A session's heartbeat: its read is a query and its changes are calls on the session, each
/// answered with the old response's `{heartbeat}`.
#[test]
fn the_heartbeat_commands_carry_their_members() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "heartbeat_get", "activeSessionId": "s1" }),
        json!({ "type": "heartbeat_set", "activeSessionId": "s1", "schedule": "5m",
                "prompt": "check", "deliveryMode": "steer" }),
        json!({ "type": "heartbeat_update", "activeSessionId": "s1", "action": "pause" }),
        json!({ "type": "heartbeat_manage", "activeSessionId": "s1", "jobId": "j1",
                "action": "stop" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let cleared = serde_json::to_value(response_from_reply(
        "heartbeat_update",
        None,
        Outcome::Ok {
            result: json!({ "heartbeat": null }),
        },
    ))
    .unwrap();
    assert_eq!(
        (frames, cleared),
        (
            vec![
                Outgoing::Query(ClientFrame::Query {
                    id: "i".to_string(),
                    name: "session.heartbeat".to_string(),
                    args: json!({ "session": "s1" }),
                }),
                call(
                    "session.set_heartbeat",
                    json!({ "session": "s1", "schedule": "5m", "prompt": "check",
                            "deliveryMode": "steer" })
                ),
                call(
                    "session.update_heartbeat",
                    json!({ "session": "s1", "action": "pause" })
                ),
                call(
                    "session.manage_heartbeat",
                    json!({ "session": "s1", "jobId": "j1", "action": "stop" })
                ),
            ],
            json!({ "type": "response", "command": "heartbeat_update", "success": true,
                    "data": { "heartbeat": null } }),
        )
    );
}

/// The daemon's own end: `shutdown` and `restart` are calls on no session, answered without
/// `data`, and the `system/closing` every connection hears comes back as `daemon_closing`.
#[test]
fn the_daemon_stops_are_calls_and_its_closing_comes_back() {
    let command = |value: Value| serde_json::from_value::<DaemonCommand>(value).unwrap();
    let frames: Vec<Outgoing> = [
        json!({ "type": "shutdown", "force": true }),
        json!({ "type": "restart", "id": "x" }),
    ]
    .into_iter()
    .map(|value| outgoing("i".to_string(), command(value)).unwrap())
    .collect();
    let call = |method: &str, args: Value| {
        Outgoing::Call(ClientFrame::Call {
            id: "i".to_string(),
            method: method.to_string(),
            args,
        })
    };
    let closing = Event {
        id: EventId("live-9".to_string()),
        seq: 0,
        ts: 0,
        event: NewEvent {
            topic: Topic::new("system/closing").unwrap(),
            source: "runtime".to_string(),
            session: None,
            payload: json!({ "reason": "shutdown" }),
            trust: Trust::Owner,
            cause: None,
            reply: None,
        },
    };
    let stopped = serde_json::to_value(response_from_reply(
        "shutdown",
        None,
        Outcome::Ok {
            result: Value::Null,
        },
    ))
    .unwrap();
    assert_eq!(
        (
            frames,
            stopped,
            serde_json::to_value(outbound_from_event(&closing, Delivery::Live).unwrap()).unwrap(),
        ),
        (
            vec![
                call("daemon.shutdown", json!({ "force": true })),
                call("daemon.restart", json!({})),
            ],
            json!({ "type": "response", "command": "shutdown", "success": true }),
            json!({ "type": "daemon_closing", "reason": "shutdown" }),
        )
    );
}
