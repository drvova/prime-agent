//! The wire2 compatibility layer (TUI port stage (a)): wire2 traffic in terms of the inputs the
//! TUI already consumes, so a later phase can move the `daemon_client` facade onto wire2 without
//! touching the views that speak `DaemonCommand` and `DaemonClientEvent`. Nothing calls it on the
//! live transport yet; `tests/goldens/inbound/` is its oracle.
//!
//! # The contract the runtime honours
//!
//! Inbound, one wire2 [`Event`] stands for one old [`DaemonOutbound`] frame, chosen by topic:
//!
//! | topic | payload | old frame |
//! | --- | --- | --- |
//! | `agent/<id>/stream` | today's `session_event.event` object, plus `meta.sequence` (below) | `session_event` |
//! | `agent/<id>/side_question` | today's `side_question_event.event` object, verbatim | `side_question_event` |
//! | `agent/<id>/closed` | `{"reason": "killed" \| "shutdown" \| "completed" \| "replaced" \| "update"}` | `session_closed` |
//! | `agent/<id>/crashed`, `recovered`, `failed`, replayed only | today's `session_crashed` / `session_recovered` / `session_failed` event, verbatim | `session_event` |
//! | `runtime/roster` | today's `roster_update` frame without `type` (`changed`, `removed`, `resync`) | `roster_update` |
//!
//! `<id>` is the session's active id, the one `DaemonCommand`s address. On the agent topics the
//! event's metadata becomes the frame's `meta`: `id` is the event id, `emittedAt` its `ts`
//! (ISO-8601, milliseconds), and `replayed` is set for an event delivered before its
//! subscription's `backfilled` frame. `sequence` is the session's own event counter, the one the
//! snapshot reports as `lastEventSequence` and the TUI keeps the highest of as its resume point:
//!
//! - On `agent/<id>/closed`, a stored topic, it is the event's `seq`, and a subscription resumes
//!   with `Since::After { seq }` from it (gaps are fine: the log's `seq` qualifies). A
//!   `side_question_event` frame carries no `meta`.
//! - `agent/<id>/stream` is live only (`Topic::is_live_only`): its events reach a subscription
//!   only live, with `seq` 0, which is no resume point. The runtime carries the session's counter
//!   in the payload instead, as `meta.sequence` beside the old event's own members; the layer
//!   takes it out, so the TUI sees the old event unchanged. A stream event without it has no
//!   `sequence`. A reconnecting client resumes the stream from the snapshot, not from the log.
//!
//! A session's faults are stored topics the runtime also mirrors onto its stream, unnumbered, as
//! they happen. Live, the stream's copy is the frame and the stored one is ignored; replayed (a
//! client that attaches after the fault, such as one the daemon's death and revival happened
//! without), the stored one is, so a later client sees the fault the session's attached clients
//! were shown. Its frame carries no `sequence`, as the stream's copy does not.
//!
//! There is no cursor generation on wire2 (the log outlives the process), so `meta.cursor` is
//! absent; the TUI reads it only when `sequence` is missing. An event on any other topic is not a
//! TUI input today and is ignored, like an unknown broadcast on the old wire.
//!
//! Outbound, the input commands become one publish each on `agent/<id>/input` (owner trust): the
//! payload is the command's own JSON without `type` and `activeSessionId`, plus `kind` - the old
//! command type (`prompt`, `steer`, `follow_up`).
//!
//! The session reads become one query each, whose reply's result is the old response's `data`
//! verbatim ([`response_from_reply`]; an error reply is the old failure with its message):
//!
//! | command | query | arguments |
//! | --- | --- | --- |
//! | `attach` | `session.snapshot` | `shape: "attach"`, `clientId`, `capabilities`, `resumeCursor` |
//! | `get_state` | `session.snapshot` | `shape: "state"` |
//! | `get_messages` | `transcript.page` | `shape: "messages"`, `page`, `historyLeafId`, `capabilities` |
//!
//! each with `session`, the command's `activeSessionId`; an argument the command does not carry is
//! left out. Every other command stays on the old wire.
//!
//! # Not mapped yet (phase 7 types these queries and calls)
//!
//! - Queries: `sessions.list` (`list`, `list_saved_sessions` and its `session_list_item` /
//!   `session_list_progress` stream), the chunked `session_snapshot_*` frames (a TUI attach does
//!   not ask for them), `session_attached`, `session_resynced`, `session_replaced`, `settings`
//!   (`get_settings`), `models` (`get_model_catalog`, `get_available_models`,
//!   `model_catalog_changed`), `subscriptions` and `tasks` (`heartbeats_list`,
//!   `heartbeats_changed`, `list_kernel_bash`, `factory_activity`), `get_session_stats`,
//!   `get_commands`, `get_rlm_children`, `roster_subscribe`'s snapshot.
//! - Calls: `create`, `stop`/`kill`, `rename`, `delete`, `set_model`, `set_thinking_level`,
//!   `fork`, `label`, `export`, the auth commands, `abort`/`abort_and_send_queued`,
//!   `abort_compaction`, `compact`, the queue mutations, `detach`.
//! - `prompt_and_wait`: a publish plus a wait for the turn's `agent_end` on `agent/<id>/stream`.
//! - Connection frames: `daemon_hello` from `welcome`, `daemon_closing` from `closing`, the
//!   direct worker link (retired: wire2 has one connection), `agent_traffic`, `history_import`,
//!   `session_binding` (a wire2 session keeps its id across rebuilds).

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonEventMeta, DaemonOutbound, DaemonProtocolInfo, DaemonResponse,
    DaemonSessionClosedReason, DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use pa_types::event_log::{Event, Topic};
use pa_types::wire2::{ClientFrame, Outcome};
use serde_json::{Map, Value};

/// How an event reached the client: in a subscription's replay, or live after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Backfill,
    Live,
}

/// A TUI command, as wire2 carries it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outgoing {
    /// An input command, published on `agent/<id>/input`.
    Publish(ClientFrame),
    /// A session read: its reply's result is the old response's `data` ([`response_from_reply`]).
    Query(ClientFrame),
    /// A command wire2 does not carry yet (see the module docs); it stays on the old wire.
    Unmapped(Box<DaemonCommand>),
}

/// The old frame `event` stands for, or `None` for a topic the TUI has no input for.
///
/// # Errors
///
/// The event is on a mapped topic but its payload is not that topic's shape.
pub(crate) fn outbound_from_event(
    event: &Event,
    delivery: Delivery,
) -> Result<Option<DaemonOutbound>> {
    let levels: Vec<&str> = event.event.topic.levels().collect();
    let payload = &event.event.payload;
    let meta = |active_session_id: &str, sequence: Option<u64>| DaemonEventMeta {
        id: event.id.0.clone(),
        protocol: DaemonProtocolInfo {
            name: DAEMON_PROTOCOL_NAME.to_string(),
            version: DAEMON_PROTOCOL_VERSION,
        },
        active_session_id: Some(active_session_id.to_string()),
        sequence,
        cursor: None,
        emitted_at: crate::update_restart_wait::iso_from_unix_ms(event.ts),
        replayed: match delivery {
            Delivery::Backfill => Some(true),
            Delivery::Live => None,
        },
    };
    let outbound = match levels.as_slice() {
        ["agent", id, "stream"] => {
            let mut payload = payload.clone();
            let sequence = payload
                .as_object_mut()
                .ok_or_else(|| anyhow!("{}: the payload is not an object", event.event.topic))?
                .remove("meta")
                .map(|meta| {
                    meta.get("sequence").and_then(Value::as_u64).ok_or_else(|| {
                        anyhow!("{}: `meta.sequence` is not a count", event.event.topic)
                    })
                })
                .transpose()?;
            DaemonOutbound::SessionEvent {
                active_session_id: (*id).to_string(),
                event: payload,
                meta: Some(meta(id, sequence)),
                rest: Map::new(),
            }
        }
        ["agent", id, "crashed" | "recovered" | "failed"] => match delivery {
            Delivery::Backfill => DaemonOutbound::SessionEvent {
                active_session_id: (*id).to_string(),
                event: payload.clone(),
                meta: Some(meta(id, None)),
                rest: Map::new(),
            },
            // The stream carried it live.
            Delivery::Live => return Ok(None),
        },
        ["agent", id, "side_question"] => DaemonOutbound::SideQuestionEvent {
            active_session_id: (*id).to_string(),
            event: payload.clone(),
            rest: Map::new(),
        },
        ["agent", id, "closed"] => DaemonOutbound::SessionClosed {
            active_session_id: (*id).to_string(),
            reason: serde_json::from_value::<DaemonSessionClosedReason>(
                payload.get("reason").cloned().unwrap_or(Value::Null),
            )
            .with_context(|| format!("{}: the close reason", event.event.topic))?,
            meta: Some(meta(id, Some(event.seq))),
            rest: Map::new(),
        },
        ["runtime", "roster"] => {
            let mut frame = payload
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("runtime/roster: the payload is not an object"))?;
            frame.insert("type".to_string(), Value::from("roster_update"));
            serde_json::from_value(Value::Object(frame)).context("runtime/roster: the payload")?
        }
        // Not a TUI input today: ignored, like an unknown broadcast on the old wire.
        _ => return Ok(None),
    };
    Ok(Some(outbound))
}

/// The old frames, as JSON, that a client of this layer receives for `events` delivered live, in
/// order; an event on a topic the TUI has no input for is left out. The test seam a verifier
/// outside the crate drives this layer through: pa-cli's `runtime_daemon_e2e` replays the inbound
/// goldens' scripts against a real runtime daemon, captures the events it sends, and compares
/// what this returns with the recorded supervisor stream.
///
/// # Errors
///
/// An event is on a mapped topic but its payload is not that topic's shape.
#[doc(hidden)]
pub fn live_frames(events: &[Event]) -> Result<Vec<Value>> {
    let mut frames = Vec::with_capacity(events.len());
    for event in events {
        if let Some(frame) = outbound_from_event(event, Delivery::Live)? {
            frames.push(serde_json::to_value(frame)?);
        }
    }
    Ok(frames)
}

/// `command` as wire2 carries it: an input command becomes a publish and a session read a query,
/// each with `request_id` as its frame id; every other command comes back unmapped.
///
/// # Errors
///
/// The command's session id is not a topic level.
pub(crate) fn outgoing(request_id: String, command: DaemonCommand) -> Result<Outgoing> {
    let query = |name: &str, session: &str, shape: &str, rest: Vec<(&str, Option<Value>)>| {
        let mut args = Map::new();
        args.insert("session".to_string(), Value::from(session));
        args.insert("shape".to_string(), Value::from(shape));
        for (key, value) in rest {
            if let Some(value) = value {
                args.insert(key.to_string(), value);
            }
        }
        Ok(Outgoing::Query(ClientFrame::Query {
            id: request_id.clone(),
            name: name.to_string(),
            args: Value::Object(args),
        }))
    };
    let active_session_id = match &command {
        DaemonCommand::Prompt {
            active_session_id, ..
        }
        | DaemonCommand::Steer {
            active_session_id, ..
        }
        | DaemonCommand::FollowUp {
            active_session_id, ..
        } => active_session_id.clone(),
        DaemonCommand::Attach {
            active_session_id,
            client_id,
            capabilities,
            resume_cursor,
            ..
        } => {
            return query(
                "session.snapshot",
                active_session_id,
                "attach",
                vec![
                    (
                        "clientId",
                        client_id.as_ref().map(|id| Value::from(id.as_str())),
                    ),
                    ("capabilities", member(capabilities.as_ref())),
                    ("resumeCursor", member(resume_cursor.as_ref())),
                ],
            )
        }
        DaemonCommand::GetState {
            active_session_id, ..
        } => return query("session.snapshot", active_session_id, "state", Vec::new()),
        DaemonCommand::GetMessages {
            active_session_id,
            page,
            capabilities,
            rest,
            ..
        } => {
            return query(
                "transcript.page",
                active_session_id,
                "messages",
                vec![
                    ("page", member(page.as_ref())),
                    ("historyLeafId", rest.get("historyLeafId").cloned()),
                    ("capabilities", member(capabilities.as_ref())),
                ],
            )
        }
        // The rest is the module docs' list of commands wire2 does not carry yet.
        _ => return Ok(Outgoing::Unmapped(Box::new(command))),
    };
    let topic = Topic::new(format!("agent/{active_session_id}/input"))
        .with_context(|| format!("session {active_session_id:?} has no input topic"))?;
    let Value::Object(mut payload) = serde_json::to_value(&command)? else {
        return Err(anyhow!("a command serializes to an object"));
    };
    payload.remove("activeSessionId");
    let kind = payload
        .remove("type")
        .ok_or_else(|| anyhow!("a command carries its type"))?;
    payload.insert("kind".to_string(), kind);
    Ok(Outgoing::Publish(ClientFrame::Publish {
        id: request_id,
        topic,
        payload: Value::Object(payload),
    }))
}

/// An optional command member a mapped query forwards: its JSON, `None` when it is absent.
fn member<T: serde::Serialize>(value: Option<&T>) -> Option<Value> {
    value.and_then(|value| serde_json::to_value(value).ok())
}

/// The old response to the `command` a mapped query stood for (request `id`), from the query's
/// reply: its result is the response's `data`, its error the failure's message.
pub(crate) fn response_from_reply(
    command: &str,
    id: Option<String>,
    outcome: Outcome,
) -> DaemonOutbound {
    let (success, data, error) = match outcome {
        Outcome::Ok { result } => (true, Some(result), None),
        Outcome::Error { message } => (false, None, Some(message)),
    };
    DaemonOutbound::Response {
        response: DaemonResponse {
            id,
            command: command.to_string(),
            success,
            data,
            error,
            error_info: None,
        },
    }
}

/// The wire2 frame `command` becomes (request `request_id`), or `None` for a command wire2 does
/// not carry yet; the test seam pa-cli's `runtime_daemon_e2e` drives the layer's outbound half
/// through, against a real runtime daemon.
///
/// # Errors
///
/// The command's session id is not a topic level.
#[doc(hidden)]
pub fn request_frame(request_id: String, command: DaemonCommand) -> Result<Option<ClientFrame>> {
    Ok(match outgoing(request_id, command)? {
        Outgoing::Publish(frame) | Outgoing::Query(frame) => Some(frame),
        Outgoing::Unmapped(_) => None,
    })
}

/// The old response frame, as JSON, a client of this layer receives for the reply to a mapped
/// `command`'s query; the inbound half of [`request_frame`]'s seam.
///
/// # Errors
///
/// The frame does not serialize (it always does).
#[doc(hidden)]
pub fn response_frame(command: &str, outcome: Outcome) -> Result<Value> {
    Ok(serde_json::to_value(response_from_reply(
        command, None, outcome,
    ))?)
}

#[cfg(test)]
mod tests;
