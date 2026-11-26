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
//! There is no cursor generation on wire2 (the log outlives the process), so `meta.cursor` is
//! absent; the TUI reads it only when `sequence` is missing. An event on any other topic is not a
//! TUI input today and is ignored, like an unknown broadcast on the old wire.
//!
//! Outbound, the input commands become one publish each on `agent/<id>/input` (owner trust): the
//! payload is the command's own JSON without `type` and `activeSessionId`, plus `kind` - the old
//! command type (`prompt`, `steer`, `follow_up`). Every other command stays on the old wire.
//!
//! # Not mapped yet (phase 7 types these queries and calls)
//!
//! - Queries: `sessions.list` (`list`, `list_saved_sessions` and its `session_list_item` /
//!   `session_list_progress` stream), `session.snapshot` (`attach`, `get_state`, the chunked
//!   `session_snapshot_*` frames, `session_attached`, `session_resynced`, `session_replaced`),
//!   `transcript.page` (`get_messages_page`), `settings` (`get_settings`), `models`
//!   (`get_model_catalog`, `get_available_models`, `model_catalog_changed`), `subscriptions` and
//!   `tasks` (`heartbeats_list`, `heartbeats_changed`, `list_kernel_bash`, `factory_activity`),
//!   `get_session_stats`, `get_commands`, `get_rlm_children`, `roster_subscribe`'s snapshot.
//! - Calls: `create`, `stop`/`kill`, `rename`, `delete`, `set_model`, `set_thinking_level`,
//!   `fork`, `label`, `export`, the auth commands, `abort`/`abort_and_send_queued`,
//!   `abort_compaction`, `compact`, the queue mutations, `detach`.
//! - `prompt_and_wait`: a publish plus a wait for the turn's `agent_end` on `agent/<id>/stream`.
//! - Connection frames: `daemon_hello` from `welcome`, `daemon_closing` from `closing`, the
//!   direct worker link (retired: wire2 has one connection), `agent_traffic`, `history_import`,
//!   `session_binding` (a wire2 session keeps its id across rebuilds).

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonEventMeta, DaemonOutbound, DaemonProtocolInfo, DaemonSessionClosedReason,
    DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use pa_types::event_log::{Event, Topic};
use pa_types::wire2::ClientFrame;
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

/// `command` as wire2 carries it: an input command becomes a publish with `request_id` as its
/// frame id; every other command comes back unmapped.
///
/// # Errors
///
/// The command's session id is not a topic level.
pub(crate) fn outgoing(request_id: String, command: DaemonCommand) -> Result<Outgoing> {
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

#[cfg(test)]
mod tests;
