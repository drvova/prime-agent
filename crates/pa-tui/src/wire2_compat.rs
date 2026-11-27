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
//! | `runtime/traffic` | today's `agent_traffic` frame's `entry` | `agent_traffic` |
//! | `runtime/session_list/<request>` | today's `session_list_item` (a `session`) or `session_list_progress` frame without `type` | `session_list_item` / `session_list_progress` |
//! | `runtime/models` | `{}` | `model_catalog_changed` |
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
//! verbatim ([`response_from_reply`]; an error reply is the old failure with its message, and its
//! `info` as the failure's `errorInfo`):
//!
//! | command | query | arguments |
//! | --- | --- | --- |
//! | `get_state` | `session.snapshot` | `shape: "state"` |
//! | `get_messages` | `transcript.page` | `shape: "messages"`, `page`, `historyLeafId`, `capabilities` |
//!
//! each with `session`, the command's `activeSessionId`; an argument the command does not carry is
//! left out.
//!
//! The agents view's feeds become a subscription on their live-only topic, then a query whose
//! reply's result is the old response's `data` (the feed's current state, as the old subscribe
//! answered it); a transport sends the query once the subscription's `backfilled` arrived, so no
//! change falls between the two. Stopping one is an unsubscribe, which the layer answers with the
//! old response itself:
//!
//! | command | subscription (`subId`, filter) | query |
//! | --- | --- | --- |
//! | `roster_subscribe` | `compat:roster`, `runtime/roster` | `runtime.roster` (`{roster}`) |
//! | `agent_traffic_subscribe` | `compat:traffic`, `runtime/traffic` | `runtime.traffic` (`{entries}`) |
//! | `roster_unsubscribe`, `agent_traffic_unsubscribe` | unsubscribed | none |
//!
//! The saved-session catalog streams: `list_saved_sessions` becomes a subscription
//! (`compat:list:<request>`, `Since::Now`) on its request's live-only topic
//! `runtime/session_list/<request>`, then, once its `backfilled` arrived, the query
//! `sessions.list {shape: "saved", stream: <request>}` with the command's `activeSessionId`,
//! `cwd`, `sessionDir`, `scope`, `limit` and `cursor` (those it carries). The topic's events
//! become the old `session_list_item` and `session_list_progress` frames (their `id` is the
//! request's), the reply the old response; the transport unsubscribes at the reply, which is
//! the catalog (a row that reaches the client after it is one the old daemon's full queue
//! would have skipped).
//!
//! A session's setting switches become one call each, whose arguments are the command's own JSON
//! without `type` and `activeSessionId`, plus `session` (the command's `activeSessionId`), and
//! whose reply's result is the old response's `data`; a command the old daemon answered without
//! `data` is answered `null`, which the layer leaves out again ([`response_from_reply`]):
//!
//! | command | call | old response `data` |
//! | --- | --- | --- |
//! | `cycle_model` | `session.cycle_model` | `{model, thinkingLevel, serviceTier, isScoped}`, or `null` |
//! | `cycle_thinking_level` | `session.cycle_effort` | `{level}`, or `null` |
//! | `set_scoped_models` | `session.set_scoped_models` | none |
//! | `set_service_tier` | `session.set_service_tier` | none |
//! | `set_transport` | `session.set_transport` | none |
//! | `set_steering_mode` | `session.set_steering_mode` | none |
//! | `set_follow_up_mode` | `session.set_follow_up_mode` | none |
//! | `set_auto_compaction` | `session.set_auto_compaction` | none |
//! | `set_auto_retry` | `session.set_auto_retry` | none |
//! | `reload` | `session.reload` | none |
//! | `mark_anthropic_warning_shown` | `session.mark_anthropic_warning_shown` | none |
//! | `navigate_tree` | `session.navigate_tree` | `{cancelled, aborted?, editorText?, summaryEntry?}` |
//! | `abort_branch_summary` | `session.abort_branch_summary` | none |
//! | `export_html` | `session.export_html` | `{path}` |
//! | `fork` | `session.fork_entry` | `{cancelled, selectedText?}` |
//! | `import_jsonl` | `session.import_jsonl` | `{cancelled}` |
//! | `mutate_queued_message` | `session.mutate_queued_message` | `{status}` (`applied`, `rejected`) |
//! | `abort_compaction` | `session.abort_compaction` | none |
//! | `abort_retry` | `session.abort_retry` | none |
//! | `detach` (of a session) | `session.detach` | none |
//! | `answer_question` | `session.answer_question` | none |
//! | `start_side_question` | `session.start_side_question` | none (the run streams on `agent/<id>/side_question`) |
//! | `abort_side_question` | `session.abort_side_question` | `{aborted}` |
//! | `record_better_outcome` | `session.record_better_outcome` | none |
//! | `kill_kernel_bash` | `session.kill_kernel_bash` | `{id, killed}` |
//! | `factory_activity` | `session.factory_activity` | the kernel factory's reply (the unscoped graph with `factorySpecs`) |
//! | `kill_persistent_shell` | `persistent_shells.kill` (no session) | `{shellId}` |
//!
//! `attach` is the call `session.attach` with `session` (the command's `activeSessionId`),
//! `clientId`, `capabilities` and `resumeCursor` (those it carries), whose reply's result is the
//! old attach response's `data`: the connection's client is attached to the session until its
//! `detach` or the connection's end, and one that names `user_questions` makes the session's
//! kernel questions wait for its `answer_question`. The questions, the todo list and the kernel's
//! notifications reach the client as the old `question_open`, `question_closed`, `todo_update`
//! and `kernel_notification` session events on `agent/<id>/stream`.
//!
//! What the TUI reads beside a session's turns becomes one query each, built the same way (the
//! command's members, plus `session` for a session's), whose reply's result is the old response's
//! `data`:
//!
//! | command | query | old response `data` |
//! | --- | --- | --- |
//! | `get_commands` | `session.commands` | `{commands}` |
//! | `get_mcp_connections` | `session.mcp_connections` | `{connections, services, credentials, catalogDiagnostics}` |
//! | `get_model_catalog` | `session.model_catalog` | `{models, configuredProviders, billing, accounts, accountChoices}` |
//! | `get_usage` | `usage` (`section`, `forceRefresh`; no session) | the `QuotaReport` or `SpendHistory` |
//! | `get_user_messages_for_forking` | `session.fork_messages` | `{messages: [{entryId, text}]}` |
//! | `get_last_assistant_text` | `session.last_assistant_text` | `{text}` |
//! | `list_kernel_bash` | `session.kernel_bash` | `{activities}` |
//! | `tail_kernel_bash` | `session.kernel_bash_tail` | `{id, tail}` |
//! | `list_persistent_shells` | `persistent_shells` (no session) | `{activities}` |
//! | `tail_persistent_shell` | `persistent_shells.tail` (no session) | `{shellId, tail}` |
//!
//! `model_catalog_changed`, a broadcast on the old wire, is the live-only `runtime/models`: a
//! transport follows it for the connection's life (subscription `compat:models`, `Since::Now`).
//!
//! Every other command stays on the old wire.
//!
//! # Not mapped yet (phase 7 types these queries and calls)
//!
//! - Queries: `sessions.list` for `list`, the chunked `session_snapshot_*` frames (a TUI attach does
//!   not ask for them), `session_attached`, `session_resynced`, `session_replaced`, `settings`
//!   (`get_settings`), `models` (`get_available_models`), `subscriptions` and `tasks`
//!   (`heartbeats_list`, `heartbeats_changed`),
//!   `get_session_stats`, `get_rlm_children`, `get_rlm_max_depth_status`,
//!   `agent_messages_status`, `heartbeat_get`.
//! - Calls: `create`, `stop`/`kill`, `rename`, `delete`, `set_model`, `set_thinking_level`,
//!   `fork` of the whole saved session, `label`, `export`, the auth
//!   commands, `abort`/`abort_and_send_queued`, `compact`, a `detach` that names no session,
//!   `delete_rlm_subagent`, `set_rlm_max_depth`, `agent_messages_pause`/`resume`/`clear`,
//!   `heartbeat_manage`/`set`/`update`.
//! - `prompt_and_wait`: a publish plus a wait for the turn's `agent_end` on `agent/<id>/stream`.
//! - Connection frames: `daemon_hello` from `welcome`, `daemon_closing` from `closing`, the
//!   direct worker link (retired: wire2 has one connection), `history_import`,
//!   `session_binding` (a wire2 session keeps its id across rebuilds).

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonEventMeta, DaemonOutbound, DaemonProtocolInfo, DaemonResponse,
    DaemonSessionClosedReason, DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use pa_types::event_log::{Event, Topic, TopicFilter};
use pa_types::wire2::{ClientFrame, Outcome, Since};
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
    /// A session switch: its reply's result is the old response's `data` ([`response_from_reply`]).
    Call(ClientFrame),
    /// A feed followed: the subscription, then the query whose reply is the old response.
    Follow {
        subscribe: ClientFrame,
        query: ClientFrame,
    },
    /// A streamed read: the subscription to its rows' topic, the query (sent once the
    /// subscription's `backfilled` arrived) whose reply is the old response, and the
    /// unsubscribe the transport sends at that reply.
    Stream {
        subscribe: ClientFrame,
        query: ClientFrame,
        unsubscribe: ClientFrame,
    },
    /// A feed no longer followed: the unsubscribe, and the old response the layer answers.
    Unfollow {
        unsubscribe: ClientFrame,
        response: Box<DaemonOutbound>,
    },
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
        ["runtime", "session_list", _] => {
            let mut frame = payload
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("{}: the payload is not an object", event.event.topic))?;
            let kind = if frame.contains_key("session") {
                "session_list_item"
            } else {
                "session_list_progress"
            };
            frame.insert("type".to_string(), Value::from(kind));
            serde_json::from_value(Value::Object(frame))
                .with_context(|| format!("{}: the payload", event.event.topic))?
        }
        ["runtime", "models"] => DaemonOutbound::ModelCatalogChanged { rest: Map::new() },
        ["runtime", "traffic"] => DaemonOutbound::AgentTraffic {
            entry: serde_json::from_value(payload.clone())
                .context("runtime/traffic: the payload")?,
            rest: Map::new(),
        },
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

/// The feeds the agents view follows: each one's subscription id, topic and query.
const ROSTER_FEED: (&str, &str, &str) = ("compat:roster", "runtime/roster", "runtime.roster");
const TRAFFIC_FEED: (&str, &str, &str) = ("compat:traffic", "runtime/traffic", "runtime.traffic");

/// `command` as wire2 carries it: an input command becomes a publish, a session read a query, a
/// setting switch a call, a feed's subscribe and unsubscribe its subscription's, the
/// saved-session list a stream, each request with `request_id` as its frame id; every other
/// command comes back unmapped.
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
            let mut args = Map::new();
            args.insert(
                "session".to_string(),
                Value::from(active_session_id.as_str()),
            );
            for (key, value) in [
                (
                    "clientId",
                    client_id.as_ref().map(|id| Value::from(id.as_str())),
                ),
                ("capabilities", member(capabilities.as_ref())),
                ("resumeCursor", member(resume_cursor.as_ref())),
            ] {
                if let Some(value) = value {
                    args.insert(key.to_string(), value);
                }
            }
            return Ok(Outgoing::Call(ClientFrame::Call {
                id: request_id,
                method: "session.attach".to_string(),
                args: Value::Object(args),
            }));
        }
        DaemonCommand::Detach {
            active_session_id: Some(active_session_id),
            ..
        } => return session_call(request_id, "session.detach", active_session_id, &command),
        DaemonCommand::AnswerQuestion {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.answer_question",
                active_session_id,
                &command,
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
        DaemonCommand::ListSavedSessions {
            active_session_id,
            cwd,
            session_dir,
            scope,
            limit,
            cursor,
            ..
        } => {
            let mut args = Map::new();
            args.insert("shape".to_string(), Value::from("saved"));
            args.insert("stream".to_string(), Value::from(request_id.as_str()));
            for (key, value) in [
                (
                    "activeSessionId",
                    active_session_id.clone().map(Value::from),
                ),
                ("cwd", cwd.clone().map(Value::from)),
                ("sessionDir", session_dir.clone().map(Value::from)),
                (
                    "scope",
                    Some(scope.clone()).filter(|scope| !scope.is_null()),
                ),
                ("limit", limit.map(Value::from)),
                ("cursor", cursor.clone().map(Value::from)),
            ] {
                if let Some(value) = value {
                    args.insert(key.to_string(), value);
                }
            }
            let sub_id = format!("compat:list:{request_id}");
            return Ok(Outgoing::Stream {
                subscribe: ClientFrame::Subscribe {
                    sub_id: sub_id.clone(),
                    filter: TopicFilter::new(format!("runtime/session_list/{request_id}"))
                        .with_context(|| format!("request {request_id:?} cannot name a stream"))?,
                    since: Since::Now,
                },
                query: ClientFrame::Query {
                    id: request_id,
                    name: "sessions.list".to_string(),
                    args: Value::Object(args),
                },
                unsubscribe: ClientFrame::Unsubscribe { sub_id },
            });
        }
        DaemonCommand::CycleModel {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.cycle_model",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetScopedModels {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_scoped_models",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::CycleThinkingLevel {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.cycle_effort",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetServiceTier {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_service_tier",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetTransport {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_transport",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetSteeringMode {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_steering_mode",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetFollowUpMode {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_follow_up_mode",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetAutoCompaction {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_auto_compaction",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::SetAutoRetry {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.set_auto_retry",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::Reload {
            active_session_id, ..
        } => return session_call(request_id, "session.reload", active_session_id, &command),
        DaemonCommand::MarkAnthropicWarningShown {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.mark_anthropic_warning_shown",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::GetCommands {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.commands",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::GetMcpConnections {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.mcp_connections",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::GetModelCatalog {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.model_catalog",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::GetUserMessagesForForking {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.fork_messages",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::GetLastAssistantText {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.last_assistant_text",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::NavigateTree {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.navigate_tree",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::AbortBranchSummary {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.abort_branch_summary",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::ExportHtml {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.export_html",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::Fork {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.fork_entry",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::ImportJsonl {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.import_jsonl",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::MutateQueuedMessage {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.mutate_queued_message",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::AbortCompaction {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.abort_compaction",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::AbortRetry {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.abort_retry",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::StartSideQuestion {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.start_side_question",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::AbortSideQuestion {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.abort_side_question",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::RecordBetterOutcome {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.record_better_outcome",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::GetUsage { .. } => {
            return command_query(request_id, "usage", None, &command);
        }
        DaemonCommand::ListKernelBash {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.kernel_bash",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::TailKernelBash {
            active_session_id, ..
        } => {
            return command_query(
                request_id,
                "session.kernel_bash_tail",
                Some(active_session_id),
                &command,
            )
        }
        DaemonCommand::KillKernelBash {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.kill_kernel_bash",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::FactoryActivity {
            active_session_id, ..
        } => {
            return session_call(
                request_id,
                "session.factory_activity",
                active_session_id,
                &command,
            )
        }
        DaemonCommand::ListPersistentShells { .. } => {
            return command_query(request_id, "persistent_shells", None, &command);
        }
        DaemonCommand::TailPersistentShell { .. } => {
            return command_query(request_id, "persistent_shells.tail", None, &command);
        }
        DaemonCommand::KillPersistentShell { .. } => {
            return Ok(Outgoing::Call(ClientFrame::Call {
                id: request_id,
                method: "persistent_shells.kill".to_string(),
                args: command_args(&command, None)?,
            }));
        }
        DaemonCommand::RosterSubscribe { .. } => return follow(request_id, ROSTER_FEED),
        DaemonCommand::AgentTrafficSubscribe { .. } => return follow(request_id, TRAFFIC_FEED),
        DaemonCommand::RosterUnsubscribe { .. } => {
            return Ok(unfollow(request_id, ROSTER_FEED, "roster_unsubscribe"))
        }
        DaemonCommand::AgentTrafficUnsubscribe { .. } => {
            return Ok(unfollow(
                request_id,
                TRAFFIC_FEED,
                "agent_traffic_unsubscribe",
            ))
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

/// The arguments `command` carries over wire2: its own members without `type`, `id` and
/// `activeSessionId`, plus `session` when it addresses one.
fn command_args(command: &DaemonCommand, session: Option<&str>) -> Result<Value> {
    let Value::Object(mut args) = serde_json::to_value(command)? else {
        return Err(anyhow!("a command serializes to an object"));
    };
    for key in ["type", "id", "activeSessionId"] {
        args.remove(key);
    }
    if let Some(session) = session {
        args.insert("session".to_string(), Value::from(session));
    }
    Ok(Value::Object(args))
}

/// The call `method` on session `session` that `command` (a setting switch) becomes, with the
/// command's arguments ([`command_args`]).
fn session_call(
    request_id: String,
    method: &str,
    session: &str,
    command: &DaemonCommand,
) -> Result<Outgoing> {
    Ok(Outgoing::Call(ClientFrame::Call {
        id: request_id,
        method: method.to_string(),
        args: command_args(command, Some(session))?,
    }))
}

/// The query `name` that `command` (a read beside the session's turns) becomes, with the
/// command's arguments ([`command_args`]), on `session` when it addresses one.
fn command_query(
    request_id: String,
    name: &str,
    session: Option<&str>,
    command: &DaemonCommand,
) -> Result<Outgoing> {
    Ok(Outgoing::Query(ClientFrame::Query {
        id: request_id,
        name: name.to_string(),
        args: command_args(command, session)?,
    }))
}

/// The commands the old daemon answered without `data`: their call's `null` result is left out
/// of the response again.
const ANSWERED_WITHOUT_DATA: [&str; 14] = [
    "detach",
    "answer_question",
    "set_scoped_models",
    "set_service_tier",
    "set_transport",
    "set_steering_mode",
    "set_follow_up_mode",
    "set_auto_compaction",
    "set_auto_retry",
    "reload",
    "mark_anthropic_warning_shown",
    "abort_branch_summary",
    "start_side_question",
    "record_better_outcome",
];

/// Following `feed` (its subscription id, topic and query): live from now, then its state.
fn follow(request_id: String, (sub_id, topic, query): (&str, &str, &str)) -> Result<Outgoing> {
    Ok(Outgoing::Follow {
        subscribe: ClientFrame::Subscribe {
            sub_id: sub_id.to_string(),
            filter: TopicFilter::new(topic)?,
            since: Since::Now,
        },
        query: ClientFrame::Query {
            id: request_id,
            name: query.to_string(),
            args: Value::Object(Map::new()),
        },
    })
}

/// No longer following `feed`: its unsubscribe, answered as the old daemon answered `command`.
fn unfollow(request_id: String, (sub_id, _, _): (&str, &str, &str), command: &str) -> Outgoing {
    Outgoing::Unfollow {
        unsubscribe: ClientFrame::Unsubscribe {
            sub_id: sub_id.to_string(),
        },
        response: Box::new(DaemonOutbound::Response {
            response: DaemonResponse {
                id: Some(request_id),
                command: command.to_string(),
                success: true,
                data: None,
                error: None,
                error_info: None,
            },
        }),
    }
}

/// An optional command member a mapped query forwards: its JSON, `None` when it is absent.
fn member<T: serde::Serialize>(value: Option<&T>) -> Option<Value> {
    value.and_then(|value| serde_json::to_value(value).ok())
}

/// The old response to the `command` a mapped query or call stood for (request `id`), from its
/// reply: its result is the response's `data` (none for a command the old daemon answered without
/// one), its error the failure's message and its `info` the failure's `errorInfo`.
pub(crate) fn response_from_reply(
    command: &str,
    id: Option<String>,
    outcome: Outcome,
) -> DaemonOutbound {
    let (success, data, error) = match outcome {
        Outcome::Ok {
            result: Value::Null,
        } if ANSWERED_WITHOUT_DATA.contains(&command) => (true, None, None),
        Outcome::Ok { result } => (true, Some(result), None),
        Outcome::Error { message, info } => (
            false,
            None,
            Some((
                message,
                info.and_then(|info| serde_json::from_value(info).ok()),
            )),
        ),
    };
    DaemonOutbound::Response {
        response: DaemonResponse {
            id,
            command: command.to_string(),
            success,
            data,
            error_info: error.as_ref().and_then(|(_, info)| info.clone()),
            error: error.map(|(message, _)| message),
        },
    }
}

/// The wire2 frames `command` becomes (request `request_id`), in the order they are sent, the
/// reply-bearing one last; none for a command wire2 does not carry yet. The test seam pa-cli's
/// `runtime_daemon_e2e` drives the layer's outbound half through, against a real runtime daemon.
///
/// # Errors
///
/// The command's session id is not a topic level.
#[doc(hidden)]
pub fn request_frames(request_id: String, command: DaemonCommand) -> Result<Vec<ClientFrame>> {
    Ok(match outgoing(request_id, command)? {
        Outgoing::Publish(frame) | Outgoing::Query(frame) | Outgoing::Call(frame) => vec![frame],
        Outgoing::Follow { subscribe, query }
        | Outgoing::Stream {
            subscribe, query, ..
        } => vec![subscribe, query],
        Outgoing::Unfollow { unsubscribe, .. } => vec![unsubscribe],
        Outgoing::Unmapped(_) => Vec::new(),
    })
}

/// The old response frame, as JSON, a client of this layer receives for the reply to a mapped
/// `command`'s query; the inbound half of [`request_frames`]'s seam.
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
