//! The wire2 compatibility layer: wire2 traffic in terms of the inputs the TUI already consumes,
//! so the `daemon_client` facade's wire2 link (`daemon_client/wire2.rs`) carries the TUI on the
//! runtime daemon without touching the views that speak `DaemonCommand` and `DaemonClientEvent`.
//! `tests/goldens/inbound/` is its oracle, replayed through both transports
//! (`tests/inbound_goldens_headless.rs`).
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
//! | `system/closing` | today's `daemon_closing` frame without `type` (`reason`, `update`) | `daemon_closing` |
//! | `system/history_import` | today's `history_import` frame without `type` (`phase`, `sessions`, `kept`, `config`, `error`) | `history_import` |
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
//! Outbound, the input commands become one publish each on `agent/<id>/input` (owner trust), the
//! runtime's input (`pa_types::event_log::AgentInput`): the command's `message` as `text`, its
//! images, and its place in the lane (`steer` a steer, `follow_up` a follow-up, `prompt` its
//! `streamingBehavior`, none without one). A prompt that carries content blocks, a custom message
//! or prefix messages is not carried: the runtime's input has no room for them.
//! `prompt_and_wait` is the same publish as a follow-up, answered at the `agent/<id>/done` of the
//! task the publish's reply names (a failed task fails it); the transport follows the session's
//! `done` before it publishes.
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
//! | `agent_messages_pause` | `agent_messages.pause` (every running session without one) | the safety status |
//! | `agent_messages_resume` | `agent_messages.resume` (every running session without one) | the safety status |
//! | `agent_messages_clear` | `agent_messages.clear` | `{steering, followUp}` |
//! | `set_rlm_max_depth` | `session.set_rlm_max_depth` | `{maxDepth, source, globalSaved, globalError?}` |
//! | `delete_rlm_subagent` | `session.delete_child` (`childId` as `child`) | `{deleted, reason?}` |
//! | `heartbeat_set` | `session.set_heartbeat` | `{heartbeat}` |
//! | `heartbeat_update` | `session.update_heartbeat` | `{heartbeat}` (`null` for none) |
//! | `heartbeat_manage` | `session.manage_heartbeat` (an open or saved session) | `{heartbeat}` |
//! | `shutdown` | `daemon.shutdown` (`force`; no session) | none |
//! | `restart` | `daemon.restart` (no session) | none |
//! | `prepare_update_restart` | `daemon.prepare_update_restart` (`updateId`, `toVersion`; no session) | `{updateId, state, acceptedAt, expiresAt?}` |
//! | `commit_update_restart` | `daemon.commit_update_restart` (`updateId`; no session) | `{updateId, state, stopped}` |
//!
//! `update_restore_status` is the query `daemon.update_restore_status` (no session), whose result
//! is the old response's `data`: `{updateId, inFlight, complete, counts, failures}`.
//!
//! `attach` is the call `session.attach` (sent once the transport follows the session's topics,
//! [`session_subscription`]: its stream live, its stored topics after the last stored event the
//! process saw on the socket, so a reconnect replays a fault or close it missed) with `session`
//! (the command's `activeSessionId`),
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
//! | `agent_messages_status` | `agent_messages` (the first running session's without one) | the safety status |
//! | `get_rlm_max_depth_status` | `session.rlm_max_depth` | `{maxDepth, source}` |
//! | `heartbeat_get` | `session.heartbeat` | `{heartbeat}` (`null` for none) |
//!
//! `model_catalog_changed`, a broadcast on the old wire, is the live-only `runtime/models`: a
//! transport follows it for the connection's life (subscription `compat:models`, `Since::Now`).
//! So is `daemon_closing`, the live-only `system/closing` (subscription `compat:closing`,
//! `Since::Now`): the runtime publishes it to every connection when it stops (its stop signal,
//! `shutdown`, `restart`, an update's `commit_update_restart`), after the reply to the call that
//! stopped it, and then ends each connection with the wire's `closing`. An update's closing is
//! the old one whole: `reason: "update"` and its `payload` (`updateId`, `resume`, `estSeconds`,
//! `sessions`).
//!
//! `history_import`, a broadcast the old daemon also sends a client that connects mid-import
//! after its hello, is the live-only `system/history_import`: a transport follows it for the
//! connection's life (subscription `compat:history_import`, `Since::Now`), then asks the
//! `system.history_import` query ([`history_import_request`]), whose reply is the running
//! import's started frame without `type`, or `null` while none runs
//! ([`history_import_reply_frame`]).
//!
//! `heartbeats_changed`, the old daemon-global broadcast, is any session's live
//! `agent/<id>/schedules` (subscription `compat:heartbeats` on `agent/+/schedules`, `Since::Now`).
//! `daemon_hello` is the runtime's `welcome` and its `daemon.identity` (the old hello's identity
//! fields and the `serverCapabilities` it serves through this layer, never the direct worker
//! link's). `session_binding` has no counterpart: a wire2 session keeps its id across rebuilds.
//!
//! The runtime daemon's own calls and queries, which the CLI's and ACP's wire2 clients use too,
//! carry the rest of what the TUI sends; their result is the old response's `data`, verbatim
//! (the runtime answers these in the old daemon's shapes) except where noted:
//!
//! | command | wire2 | arguments, and the result |
//! | --- | --- | --- |
//! | `create` | call `session.create` (`pa_types::wire2::SessionCreate`) | `path` (the resumed `sessionPath`), `cwd`, `sessionDir`, `launchEnv`, `ephemeral` (`noSession`) and `options`: the create config's `provider`, `model`, `thinking`, `systemPrompt`, `appendSystemPrompt`, `skills`, `promptTemplates`, `autonomous`, `executionMode`, `apiKey`, `credential`, `profile` and `models`, its `telemetryDisabled` (or the command's), and `parent` (`parentSessionPath`, `rlmDepth`); `{session, path, cwd}` becomes the old `{activeSessionId, sessionId, sessionFile}` beside them |
//! | `list` | query `sessions.list` | `open` (all but `all`) |
//! | `kill` | call `session.stop` | `session` |
//! | `rename`, `set_session_name` | call `session.rename` | `session`, `name` |
//! | `rename_saved_session`, `delete_saved_session` | call `session.rename`, `session.delete` | `session`: the saved transcript's file stem (the runtime keeps a session as `<id>.jsonl`) |
//! | `set_model` | call `session.set_model` | `session`, `provider`, `modelId` |
//! | `set_thinking_level` | call `session.set_effort` | `session`, `level` |
//! | `set_session_entry_label` | call `session.label` | `session`, `entry`, `label` |
//! | `export_jsonl` | call `session.export` | `session`, `out` |
//! | `abort`, `abort_and_send_queued` | call `session.abort`, `session.abort_and_send_queued` | `session` |
//! | `execute_bash`, `abort_bash` | call `session.bash`, `session.abort_bash` | `session`, `command`, `excludeFromContext`, `transient`, `runId` |
//! | `cancel_rlm_child` | call `session.cancel_child` | `session`, `child` |
//! | `get_connection_state`, `get_rlm_children`, `get_session_stats`, `get_context_tree`, `get_context_breakdown`, `get_session_tree`, `get_system_prompt` | query `session.model`, `session.children`, `session.usage`, `session.context_tree`, `session.breakdown`, `session.tree`, `session.system_prompt` | `session` |
//! | `heartbeats_list` | query `schedules` | `session` when it names one; `{jobs}` becomes the old `{heartbeats: [{job}]}` of its heartbeat jobs |
//! | `detach` of no session | none | answered by the layer, as the old daemon answered it |
//!
//! A create's other members are left out, because the runtime daemon needs none of them:
//!
//! - `script` and `childScript`, the faux-provider verification seam the product never sets: the
//!   runtime daemon reads its scripts from its own environment (`PRIME_AGENT_FAUX_SCRIPT`,
//!   `PRIME_AGENT_FAUX_CHILD_SCRIPT`), which the launch that starts it gives it.
//! - `lifecycle`: only a `noSession` create is client-owned, and an `ephemeral` session ends with
//!   the client that created it.
//! - `env`, the Herdr pane identity: the runtime daemon reports to no Herdr pane (its `HERDR_*`
//!   variables reach the session in its `launchEnv`).
//! - `name` and `runtimeMetadata`: no TUI create sets them.
//!
//! # Not carried
//!
//! [`outgoing`] names every command, without a wildcard; these come back
//! [`Outgoing::Unmapped`] with their reason, and the transport refuses them:
//!
//! - `get_direct_worker_transport`: the old wire's direct worker link (wire2 has one connection;
//!   the facade never asks for it on wire2).
//! - The frames between the old daemon's own processes (`worker_*`, `get_worker_peer_transport`,
//!   `provider_admission_*`, `ack_result`, `retry_worker`).
//! - What the CLI's and ACP's own wire2 clients send and the TUI does not (`send_message`, the
//!   `cron_*` commands, `wait_for_idle`, `compact`, the ACP session commands, ...).
//! - The old-wire commands no client of this build sends (`reattach`, `restore_*`, `get_queue`,
//!   `new_session`, ...).
//!
//! The guard (`wire2_compat/guard_tests.rs`) finds every command the TUI's own code builds and
//! fails when one comes back unmapped.

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonEventMeta, DaemonOutbound, DaemonProtocolInfo, DaemonResponse,
    DaemonSessionClosedReason, PromptInput, StreamingBehavior, DAEMON_PROTOCOL_NAME,
    DAEMON_PROTOCOL_VERSION,
};
use pa_types::event_log::{AgentInput, Event, Priority, Topic, TopicFilter};
use pa_types::wire2::{ClientFrame, Outcome, SessionCreate, SessionParent, Since};
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
    /// A session's attach: the call, after the transport follows the session's topics
    /// ([`session_subscription`]) and before it delivers what they carried meanwhile.
    Attach { session: String, call: ClientFrame },
    /// A session's detach: the call, and the transport stops following the session's topics.
    Detach { session: String, call: ClientFrame },
    /// `prompt_and_wait`: the input published on `session`, answered once the task its reply
    /// names is done (`agent/<id>/done`).
    PublishAndWait {
        session: String,
        publish: ClientFrame,
    },
    /// A command the layer answers itself, as the old daemon did.
    Local(Box<DaemonOutbound>),
    /// A command wire2 does not carry, and why (see the module docs): the transport refuses it.
    Unmapped {
        command: Box<DaemonCommand>,
        reason: &'static str,
    },
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
        ["system", "closing"] => {
            let mut frame = payload
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("system/closing: the payload is not an object"))?;
            frame.insert("type".to_string(), Value::from("daemon_closing"));
            serde_json::from_value(Value::Object(frame)).context("system/closing: the payload")?
        }
        ["system", "history_import"] => history_import_from(payload)?,
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

/// The old `history_import` frame a `system/history_import` payload, or the
/// `system.history_import` query's result, stands for.
fn history_import_from(payload: &Value) -> Result<DaemonOutbound> {
    let mut frame = payload
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("system/history_import: the payload is not an object"))?;
    frame.insert("type".to_string(), Value::from("history_import"));
    serde_json::from_value(Value::Object(frame)).context("system/history_import: the payload")
}

/// The history import a transport follows for the connection's life: its subscription id, topic
/// and the query for the one already running.
const HISTORY_IMPORT_FEED: (&str, &str, &str) = (
    "compat:history_import",
    "system/history_import",
    "system.history_import",
);

/// The frames a transport sends once connected (request `request_id`) so the TUI hears the boot's
/// history import as on the old wire: the subscription to `system/history_import` from now, then
/// the query for one already running, whose reply [`history_import_reply_frame`] reads.
///
/// # Errors
///
/// The topic does not parse (it always does).
#[doc(hidden)]
pub fn history_import_request(request_id: String) -> Result<Vec<ClientFrame>> {
    Ok(match follow(request_id, HISTORY_IMPORT_FEED)? {
        Outgoing::Follow { subscribe, query } => vec![subscribe, query],
        other => return Err(anyhow!("a feed is followed, not {other:?}")),
    })
}

/// The old `history_import` frame, as JSON, a client that connects mid-import receives after
/// its hello, from the reply to [`history_import_request`]'s query; `None` while no import runs.
///
/// # Errors
///
/// The query failed, or its result is not a `history_import` frame without `type`.
#[doc(hidden)]
pub fn history_import_reply_frame(outcome: Outcome) -> Result<Option<Value>> {
    match outcome {
        Outcome::Ok {
            result: Value::Null,
        } => Ok(None),
        Outcome::Ok { result } => Ok(Some(serde_json::to_value(history_import_from(&result)?)?)),
        Outcome::Error { message, .. } => Err(anyhow!("system.history_import: {message}")),
    }
}

/// The feeds the agents view follows: each one's subscription id, topic and query.
const ROSTER_FEED: (&str, &str, &str) = ("compat:roster", "runtime/roster", "runtime.roster");
const TRAFFIC_FEED: (&str, &str, &str) = ("compat:traffic", "runtime/traffic", "runtime.traffic");

/// `command` as wire2 carries it: an input command becomes a publish, a session read a query, a
/// setting switch a call, a feed's subscribe and unsubscribe its subscription's, the
/// saved-session list a stream, an attach or a detach the call the transport frames with the
/// session's subscription, `prompt_and_wait` a publish answered at its task's `done`, each request
/// with `request_id` as its frame id; a detach of no session is answered here, and every command
/// wire2 does not carry comes back unmapped with its reason (the match names every command).
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
    match &command {
        DaemonCommand::Prompt {
            active_session_id,
            message,
            input,
            ..
        } => input_publish(
            request_id,
            active_session_id,
            message,
            input,
            input.streaming_behavior.map(|behavior| match behavior {
                StreamingBehavior::Steer => Priority::Steer,
                StreamingBehavior::FollowUp => Priority::FollowUp,
            }),
        ),
        DaemonCommand::Steer {
            active_session_id,
            message,
            input,
            ..
        } => input_publish(
            request_id,
            active_session_id,
            message,
            input,
            Some(Priority::Steer),
        ),
        DaemonCommand::FollowUp {
            active_session_id,
            message,
            input,
            ..
        } => input_publish(
            request_id,
            active_session_id,
            message,
            input,
            Some(Priority::FollowUp),
        ),
        DaemonCommand::PromptAndWait {
            active_session_id,
            message,
            input,
            ..
        } => match input_publish(
            request_id,
            active_session_id,
            message,
            input,
            Some(Priority::FollowUp),
        )? {
            Outgoing::Publish(publish) => Ok(Outgoing::PublishAndWait {
                session: active_session_id.clone(),
                publish,
            }),
            refused => Ok(refused),
        },
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
            Ok(Outgoing::Attach {
                session: active_session_id.clone(),
                call: ClientFrame::Call {
                    id: request_id,
                    method: "session.attach".to_string(),
                    args: Value::Object(args),
                },
            })
        }
        DaemonCommand::Detach {
            active_session_id: Some(active_session_id),
            ..
        } => match session_call(request_id, "session.detach", active_session_id, &command)? {
            Outgoing::Call(call) => Ok(Outgoing::Detach {
                session: active_session_id.clone(),
                call,
            }),
            other => Ok(other),
        },
        DaemonCommand::AnswerQuestion {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.answer_question",
            active_session_id,
            &command,
        ),
        DaemonCommand::GetState {
            active_session_id, ..
        } => query("session.snapshot", active_session_id, "state", Vec::new()),
        DaemonCommand::GetMessages {
            active_session_id,
            page,
            capabilities,
            rest,
            ..
        } => query(
            "transcript.page",
            active_session_id,
            "messages",
            vec![
                ("page", member(page.as_ref())),
                ("historyLeafId", rest.get("historyLeafId").cloned()),
                ("capabilities", member(capabilities.as_ref())),
            ],
        ),
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
            Ok(Outgoing::Stream {
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
            })
        }
        DaemonCommand::CycleModel {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.cycle_model",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetScopedModels {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_scoped_models",
            active_session_id,
            &command,
        ),
        DaemonCommand::CycleThinkingLevel {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.cycle_effort",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetServiceTier {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_service_tier",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetTransport {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_transport",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetSteeringMode {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_steering_mode",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetFollowUpMode {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_follow_up_mode",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetAutoCompaction {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_auto_compaction",
            active_session_id,
            &command,
        ),
        DaemonCommand::SetAutoRetry {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_auto_retry",
            active_session_id,
            &command,
        ),
        DaemonCommand::Reload {
            active_session_id, ..
        } => session_call(request_id, "session.reload", active_session_id, &command),
        DaemonCommand::MarkAnthropicWarningShown {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.mark_anthropic_warning_shown",
            active_session_id,
            &command,
        ),
        DaemonCommand::GetCommands {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.commands",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::GetMcpConnections {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.mcp_connections",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::GetModelCatalog {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.model_catalog",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::GetUserMessagesForForking {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.fork_messages",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::GetLastAssistantText {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.last_assistant_text",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::NavigateTree {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.navigate_tree",
            active_session_id,
            &command,
        ),
        DaemonCommand::AbortBranchSummary {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.abort_branch_summary",
            active_session_id,
            &command,
        ),
        DaemonCommand::ExportHtml {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.export_html",
            active_session_id,
            &command,
        ),
        DaemonCommand::Fork {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.fork_entry",
            active_session_id,
            &command,
        ),
        DaemonCommand::ImportJsonl {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.import_jsonl",
            active_session_id,
            &command,
        ),
        DaemonCommand::MutateQueuedMessage {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.mutate_queued_message",
            active_session_id,
            &command,
        ),
        DaemonCommand::AbortCompaction {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.abort_compaction",
            active_session_id,
            &command,
        ),
        DaemonCommand::AbortRetry {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.abort_retry",
            active_session_id,
            &command,
        ),
        DaemonCommand::StartSideQuestion {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.start_side_question",
            active_session_id,
            &command,
        ),
        DaemonCommand::AbortSideQuestion {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.abort_side_question",
            active_session_id,
            &command,
        ),
        DaemonCommand::RecordBetterOutcome {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.record_better_outcome",
            active_session_id,
            &command,
        ),
        DaemonCommand::GetUsage { .. } => command_query(request_id, "usage", None, &command),
        DaemonCommand::ListKernelBash {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.kernel_bash",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::TailKernelBash {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.kernel_bash_tail",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::KillKernelBash {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.kill_kernel_bash",
            active_session_id,
            &command,
        ),
        DaemonCommand::FactoryActivity {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.factory_activity",
            active_session_id,
            &command,
        ),
        DaemonCommand::ListPersistentShells { .. } => {
            command_query(request_id, "persistent_shells", None, &command)
        }
        DaemonCommand::TailPersistentShell { .. } => {
            command_query(request_id, "persistent_shells.tail", None, &command)
        }
        DaemonCommand::KillPersistentShell { .. } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "persistent_shells.kill".to_string(),
            args: command_args(&command, None)?,
        })),
        DaemonCommand::AgentMessagesStatus {
            active_session_id, ..
        } => command_query(
            request_id,
            "agent_messages",
            active_session_id.as_deref(),
            &command,
        ),
        DaemonCommand::AgentMessagesPause {
            active_session_id, ..
        } => ingest_call(
            request_id,
            "agent_messages.pause",
            active_session_id.as_deref(),
            &command,
        ),
        DaemonCommand::AgentMessagesResume {
            active_session_id, ..
        } => ingest_call(
            request_id,
            "agent_messages.resume",
            active_session_id.as_deref(),
            &command,
        ),
        DaemonCommand::AgentMessagesClear {
            active_session_id, ..
        } => session_call(
            request_id,
            "agent_messages.clear",
            active_session_id,
            &command,
        ),
        DaemonCommand::GetRlmMaxDepthStatus {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.rlm_max_depth",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::SetRlmMaxDepth {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_rlm_max_depth",
            active_session_id,
            &command,
        ),
        DaemonCommand::DeleteRlmSubagent {
            active_session_id,
            child_id,
            ..
        } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "session.delete_child".to_string(),
            args: serde_json::json!({ "session": active_session_id, "child": child_id }),
        })),
        DaemonCommand::HeartbeatGet {
            active_session_id, ..
        } => command_query(
            request_id,
            "session.heartbeat",
            Some(active_session_id),
            &command,
        ),
        DaemonCommand::HeartbeatSet {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.set_heartbeat",
            active_session_id,
            &command,
        ),
        DaemonCommand::HeartbeatUpdate {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.update_heartbeat",
            active_session_id,
            &command,
        ),
        DaemonCommand::HeartbeatManage {
            active_session_id, ..
        } => session_call(
            request_id,
            "session.manage_heartbeat",
            active_session_id,
            &command,
        ),
        DaemonCommand::Shutdown { .. } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "daemon.shutdown".to_string(),
            args: command_args(&command, None)?,
        })),
        DaemonCommand::Restart { .. } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "daemon.restart".to_string(),
            args: command_args(&command, None)?,
        })),
        DaemonCommand::PrepareUpdateRestart { .. } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "daemon.prepare_update_restart".to_string(),
            args: command_args(&command, None)?,
        })),
        DaemonCommand::UpdateRestoreStatus { .. } => {
            command_query(request_id, "daemon.update_restore_status", None, &command)
        }
        DaemonCommand::CommitUpdateRestart { .. } => Ok(Outgoing::Call(ClientFrame::Call {
            id: request_id,
            method: "daemon.commit_update_restart".to_string(),
            args: command_args(&command, None)?,
        })),
        DaemonCommand::RosterSubscribe { .. } => follow(request_id, ROSTER_FEED),
        DaemonCommand::AgentTrafficSubscribe { .. } => follow(request_id, TRAFFIC_FEED),
        DaemonCommand::RosterUnsubscribe { .. } => {
            Ok(unfollow(request_id, ROSTER_FEED, "roster_unsubscribe"))
        }
        DaemonCommand::AgentTrafficUnsubscribe { .. } => Ok(unfollow(
            request_id,
            TRAFFIC_FEED,
            "agent_traffic_unsubscribe",
        )),
        DaemonCommand::Detach {
            active_session_id: None,
            ..
        } => Ok(Outgoing::Local(Box::new(answered(request_id, "detach")))),
        DaemonCommand::Create {
            session_path,
            no_session,
            config,
            telemetry_disabled,
            launch_env,
            ..
        } => {
            let config = config.clone().unwrap_or(Value::Null);
            let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);
            let mut options: Map<String, Value> = CREATE_OPTIONS
                .iter()
                .filter_map(|key| Some(((*key).to_string(), config.get(*key)?.clone())))
                .collect();
            if *telemetry_disabled == Some(true)
                || config.get("telemetryDisabled") == Some(&Value::Bool(true))
            {
                options.insert("telemetryDisabled".to_string(), Value::Bool(true));
            }
            if let Some(path) = text("parentSessionPath") {
                let rlm_depth = match config.get("rlmDepth") {
                    None | Some(Value::Null) => 0,
                    Some(depth) => depth
                        .as_u64()
                        .and_then(|depth| u32::try_from(depth).ok())
                        .context("the create's rlmDepth must be a whole number")?,
                };
                options.insert(
                    "parent".to_string(),
                    serde_json::to_value(SessionParent { path, rlm_depth })?,
                );
            }
            let create = SessionCreate {
                path: session_path.clone(),
                cwd: text("cwd"),
                ephemeral: *no_session == Some(true),
                session_dir: text("sessionDir"),
                launch_env: launch_env.clone(),
                options: Some(Value::Object(options)),
                ..SessionCreate::default()
            };
            Ok(call(
                request_id,
                "session.create",
                serde_json::to_value(create)?,
            ))
        }
        DaemonCommand::List { all, .. } => Ok(query_frame(
            request_id,
            "sessions.list",
            serde_json::json!({ "open": *all != Some(true) }),
        )),
        DaemonCommand::Kill {
            active_session_id, ..
        } => Ok(call(
            request_id,
            "session.stop",
            serde_json::json!({ "session": active_session_id }),
        )),
        DaemonCommand::Rename {
            active_session_id,
            name,
            ..
        }
        | DaemonCommand::SetSessionName {
            active_session_id,
            name,
            ..
        } => Ok(call(
            request_id,
            "session.rename",
            serde_json::json!({ "session": active_session_id, "name": name }),
        )),
        DaemonCommand::RenameSavedSession {
            session_path, name, ..
        } => Ok(call(
            request_id,
            "session.rename",
            serde_json::json!({ "session": saved_session_id(session_path)?, "name": name }),
        )),
        DaemonCommand::DeleteSavedSession { session_path, .. } => Ok(call(
            request_id,
            "session.delete",
            serde_json::json!({ "session": saved_session_id(session_path)? }),
        )),
        DaemonCommand::SetModel {
            active_session_id,
            provider,
            model_id,
            ..
        } => Ok(call(
            request_id,
            "session.set_model",
            serde_json::json!({ "session": active_session_id, "provider": provider,
                                "modelId": model_id }),
        )),
        DaemonCommand::SetThinkingLevel {
            active_session_id,
            level,
            ..
        } => Ok(call(
            request_id,
            "session.set_effort",
            serde_json::json!({ "session": active_session_id, "level": level }),
        )),
        DaemonCommand::SetSessionEntryLabel {
            active_session_id,
            entry_id,
            label,
            ..
        } => Ok(call(
            request_id,
            "session.label",
            serde_json::json!({ "session": active_session_id, "entry": entry_id,
                                "label": label }),
        )),
        DaemonCommand::ExportJsonl {
            active_session_id,
            output_path,
            ..
        } => Ok(call(
            request_id,
            "session.export",
            serde_json::json!({ "session": active_session_id, "out": output_path }),
        )),
        DaemonCommand::Abort {
            active_session_id, ..
        } => Ok(call(
            request_id,
            "session.abort",
            serde_json::json!({ "session": active_session_id }),
        )),
        DaemonCommand::AbortAndSendQueued {
            active_session_id, ..
        } => Ok(call(
            request_id,
            "session.abort_and_send_queued",
            serde_json::json!({ "session": active_session_id }),
        )),
        DaemonCommand::ExecuteBash {
            active_session_id,
            command: line,
            exclude_from_context,
            transient,
            run_id,
            ..
        } => {
            let mut args = serde_json::json!({ "session": active_session_id, "command": line });
            for (key, value) in [
                ("excludeFromContext", exclude_from_context.map(Value::Bool)),
                ("transient", transient.map(Value::Bool)),
                ("runId", run_id.clone().map(Value::from)),
            ] {
                if let Some(value) = value {
                    args[key] = value;
                }
            }
            Ok(call(request_id, "session.bash", args))
        }
        DaemonCommand::AbortBash {
            active_session_id, ..
        } => Ok(call(
            request_id,
            "session.abort_bash",
            serde_json::json!({ "session": active_session_id }),
        )),
        DaemonCommand::CancelRlmChild {
            active_session_id,
            child_id,
            ..
        } => Ok(call(
            request_id,
            "session.cancel_child",
            serde_json::json!({ "session": active_session_id, "child": child_id }),
        )),
        DaemonCommand::GetConnectionState {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.model",
            active_session_id,
        )),
        DaemonCommand::GetRlmChildren {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.children",
            active_session_id,
        )),
        DaemonCommand::GetSessionStats {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.usage",
            active_session_id,
        )),
        DaemonCommand::GetContextTree {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.context_tree",
            active_session_id,
        )),
        DaemonCommand::GetContextBreakdown {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.breakdown",
            active_session_id,
        )),
        DaemonCommand::GetSessionTree {
            active_session_id, ..
        } => Ok(session_query(request_id, "session.tree", active_session_id)),
        DaemonCommand::GetSystemPrompt {
            active_session_id, ..
        } => Ok(session_query(
            request_id,
            "session.system_prompt",
            active_session_id,
        )),
        DaemonCommand::HeartbeatsList {
            active_session_id, ..
        } => {
            let mut args = serde_json::json!({});
            if let Some(session) = active_session_id {
                args["session"] = Value::from(session.as_str());
            }
            Ok(query_frame(request_id, "schedules", args))
        }
        // The direct worker link: wire2 has one connection.
        DaemonCommand::GetDirectWorkerTransport { .. } => Ok(unmapped(command, DIRECT_LINK)),
        // The old daemon's own processes' frames.
        DaemonCommand::GetWorkerPeerTransport { .. }
        | DaemonCommand::WorkerIdlePassivation { .. }
        | DaemonCommand::WorkerRosterDelta { .. }
        | DaemonCommand::ProviderAdmissionAcquire { .. }
        | DaemonCommand::ProviderAdmissionRelease { .. }
        | DaemonCommand::WorkerAgentTraffic { .. }
        | DaemonCommand::WorkerRegister { .. }
        | DaemonCommand::AckResult { .. }
        | DaemonCommand::RetryWorker { .. } => Ok(unmapped(command, WORKER_FRAME)),
        // What the CLI's and ACP's own wire2 clients send (`pa-cli` `runtime_daemon_client`,
        // `pa-runtime` `acp::wire2_link`); the TUI sends none of them.
        DaemonCommand::ListAgentPeers { .. }
        | DaemonCommand::EventBus { .. }
        | DaemonCommand::CompleteOwnedSession { .. }
        | DaemonCommand::CancelPromptAdmission { .. }
        | DaemonCommand::SendMessage { .. }
        | DaemonCommand::WaitForIdle { .. }
        | DaemonCommand::WaitForHeadlessCompletion { .. }
        | DaemonCommand::ReplaceAcpMcpServers { .. }
        | DaemonCommand::GetAvailableModels { .. }
        | DaemonCommand::AbortAndClearQueue { .. }
        | DaemonCommand::AcquireSessionInputPause { .. }
        | DaemonCommand::ReleaseSessionInputPause { .. }
        | DaemonCommand::CronList { .. }
        | DaemonCommand::CronAdd { .. }
        | DaemonCommand::CronCancel { .. }
        | DaemonCommand::Compact { .. }
        | DaemonCommand::GetSessionContext { .. } => Ok(unmapped(command, OTHER_CLIENTS)),
        // Old-wire commands no client of this build sends.
        DaemonCommand::Reattach { .. }
        | DaemonCommand::PromoteOwnedSession { .. }
        | DaemonCommand::RestoreNextTurn { .. }
        | DaemonCommand::RestoreActions { .. }
        | DaemonCommand::AppendCustomMessage { .. }
        | DaemonCommand::ResumeQueue { .. }
        | DaemonCommand::GetSessionHeader { .. }
        | DaemonCommand::GetSessionMessages { .. }
        | DaemonCommand::GetResourceSnapshot { .. }
        | DaemonCommand::SetMcpStaticToken { .. }
        | DaemonCommand::RemoveMcpConnection { .. }
        | DaemonCommand::GetQueue { .. }
        | DaemonCommand::ClearQueue { .. }
        | DaemonCommand::Refine { .. }
        | DaemonCommand::ExecuteBashAndWait { .. }
        | DaemonCommand::NewSession { .. }
        | DaemonCommand::SwitchSession { .. }
        | DaemonCommand::GetToolDefinition { .. } => Ok(unmapped(command, NOT_SENT)),
    }
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

/// The call `method` that an agent-message pause or resume becomes: on its session, or on every
/// running one when it names none.
fn ingest_call(
    request_id: String,
    method: &str,
    session: Option<&str>,
    command: &DaemonCommand,
) -> Result<Outgoing> {
    Ok(Outgoing::Call(ClientFrame::Call {
        id: request_id,
        method: method.to_string(),
        args: command_args(command, session)?,
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
const ANSWERED_WITHOUT_DATA: [&str; 16] = [
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
    "shutdown",
    "restart",
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
        response: Box::new(answered(request_id, command)),
    }
}

/// The old daemon's plain success for `command` (request `request_id`), without `data`.
fn answered(request_id: String, command: &str) -> DaemonOutbound {
    DaemonOutbound::Response {
        response: DaemonResponse {
            id: Some(request_id),
            command: command.to_string(),
            success: true,
            data: None,
            error: None,
            error_info: None,
        },
    }
}

/// Why a command is not carried (the module docs' last table).
const DIRECT_LINK: &str = "the direct worker link is the old daemon's: wire2 has one connection";
const WORKER_FRAME: &str = "a frame between the old daemon's own processes";
const OTHER_CLIENTS: &str =
    "the CLI's and ACP's own wire2 clients send it; the terminal UI does not";
const NOT_SENT: &str = "no client of this build sends it";
const INPUT_PARTS: &str =
    "the runtime daemon's input carries a prompt's text and images, not its content blocks, \
     custom message or prefix messages";

/// `command`, refused on wire2 for `reason`.
fn unmapped(command: DaemonCommand, reason: &'static str) -> Outgoing {
    Outgoing::Unmapped {
        command: Box::new(command),
        reason,
    }
}

/// The create config's members the runtime daemon's `session.create` takes as `options` under
/// their own names (the rest of the config is mapped or left out as the module docs say).
const CREATE_OPTIONS: [&str; 13] = [
    "provider",
    "model",
    "thinking",
    "systemPrompt",
    "appendSystemPrompt",
    "skills",
    "promptTemplates",
    "autonomous",
    "executionMode",
    "apiKey",
    "credential",
    "profile",
    "models",
];

/// The input command on session `session` as the runtime daemon's `agent/<id>/input` event: its
/// text, its images, and its place in the session's lane.
fn input_publish(
    request_id: String,
    session: &str,
    message: &str,
    input: &PromptInput,
    priority: Option<Priority>,
) -> Result<Outgoing> {
    if input.content.is_some() || input.custom_message.is_some() || input.prefix_messages.is_some()
    {
        return Ok(Outgoing::Unmapped {
            command: Box::new(DaemonCommand::Prompt {
                id: None,
                active_session_id: session.to_string(),
                message: message.to_string(),
                input: input.clone(),
                rest: Map::new(),
            }),
            reason: INPUT_PARTS,
        });
    }
    let topic = Topic::new(format!("agent/{session}/input"))
        .with_context(|| format!("session {session:?} has no input topic"))?;
    let images = match &input.images {
        Some(images) => serde_json::from_value(images.clone()).context("the prompt's images")?,
        None => Vec::new(),
    };
    let payload = AgentInput {
        text: message.to_string(),
        images,
        priority,
    };
    Ok(Outgoing::Publish(ClientFrame::Publish {
        id: request_id,
        topic,
        payload: serde_json::to_value(payload)?,
    }))
}

/// The call `method` with `args`.
fn call(request_id: String, method: &str, args: Value) -> Outgoing {
    Outgoing::Call(ClientFrame::Call {
        id: request_id,
        method: method.to_string(),
        args,
    })
}

/// The query `name` with `args`.
fn query_frame(request_id: String, name: &str, args: Value) -> Outgoing {
    Outgoing::Query(ClientFrame::Query {
        id: request_id,
        name: name.to_string(),
        args,
    })
}

/// The query `name` on session `session` alone.
fn session_query(request_id: String, name: &str, session: &str) -> Outgoing {
    query_frame(request_id, name, serde_json::json!({ "session": session }))
}

/// The id of the saved session at `path`: the runtime daemon keeps a session as `<id>.jsonl` in
/// its session dir and names a saved one by its id.
fn saved_session_id(path: &str) -> Result<String> {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{path:?} names no saved session"))
}

/// The subscription a transport follows session `session`'s topics with (`agent/<id>/#`), from
/// after `since` in the log: its live stream, and the stored topics (its faults, its close) a
/// reconnecting client replays from where it left off.
///
/// # Errors
///
/// The session id is not a topic level.
pub(crate) fn session_subscription(session: &str, since: Since) -> Result<ClientFrame> {
    Ok(ClientFrame::Subscribe {
        sub_id: session_subscription_id(session),
        filter: TopicFilter::new(format!("agent/{session}/#"))
            .with_context(|| format!("session {session:?} has no topics"))?,
        since,
    })
}

/// The id of [`session_subscription`]'s subscription.
pub(crate) fn session_subscription_id(session: &str) -> String {
    format!("compat:agent:{session}")
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
        Outcome::Ok { result } => (true, Some(old_data(command, result)), None),
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

/// The old response's `data` for a mapped `command`'s reply `result`: verbatim, except where the
/// runtime daemon's own call answers in its own terms (the module docs' phase-7 table).
fn old_data(command: &str, mut result: Value) -> Value {
    match command {
        // `{session, path, cwd}`: the old create's ids (one id on wire2) and transcript.
        "create" => {
            if let Some(fields) = result.as_object_mut() {
                let session = fields.get("session").cloned().unwrap_or(Value::Null);
                let path = fields.get("path").cloned().unwrap_or(Value::Null);
                fields.insert("activeSessionId".to_string(), session.clone());
                fields.insert("sessionId".to_string(), session);
                fields.insert("sessionFile".to_string(), path);
            }
            result
        }
        // `{jobs}`: the old catalog's rows are the heartbeat jobs, each as `{job}`.
        "heartbeats_list" => {
            let heartbeats: Vec<Value> = result["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|job| matches!(job["source"].as_str(), Some("heartbeat" | "rlm_heartbeat")))
                .map(|job| serde_json::json!({ "job": job }))
                .collect();
            serde_json::json!({ "heartbeats": heartbeats })
        }
        _ => result,
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
        Outgoing::Attach { call, .. } | Outgoing::Detach { call, .. } => vec![call],
        Outgoing::PublishAndWait { publish, .. } => vec![publish],
        Outgoing::Unfollow { unsubscribe, .. } => vec![unsubscribe],
        Outgoing::Local(_) | Outgoing::Unmapped { .. } => Vec::new(),
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
mod guard_tests;
#[cfg(test)]
mod tests;
