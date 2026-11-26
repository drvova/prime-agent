//! A session's stream, `agent/<id>/stream` (live only): everything the
//! session tells its clients as it runs, in one order, in the shape its
//! builder chose ([`DriverEvents`]).
//!
//! - **The json stream's shapes** (`DriverEvents::Stream` and `Quiet`, the
//!   print modes): one payload per agent event
//!   (`pa_core::session_engine::session_events::agent_event_json`) and each
//!   driver event as its arm built it, verbatim.
//! - **The session-event shapes** (`DriverEvents::SessionEvents`, the
//!   runtime daemon's sessions): the payloads the old daemon's worker sent
//!   as `session_event.event`, built from the agent's events with
//!   `pa_core::session_engine::wire_events`, each carrying the session's own
//!   event counter as `meta.sequence`. A reply opens with a `message_start`
//!   carrying `{"type": "start"}`; its deltas coalesce in one
//!   [`UpdateCoalescer`] per run (`agent_start` to `agent_end`), each
//!   consuming a number as it parks, so a superseded one leaves a gap; a
//!   block end, and every other frame, flushes the parked update first. A
//!   settled assistant row carries the session's billing stamp, a tool
//!   result goes out as a `message_start`/`message_end` pair, and every
//!   settled conversation row's `message_end` carries the session's context
//!   reading, which the next reply's streaming estimate grows from. The
//!   counter spans the session's lifetimes; `session.snapshot` reports it as
//!   `lastEventSequence`.
//!
//! Under supervision a crash is mirrored onto the stream as the old
//! supervisor sends it, without a sequence: `session_crashed`, then for a
//! run the crash cut off (its `agent_start` went out, its `agent_end` never
//! will) a `turn_end` with the crash's reason and an `agent_end`; then
//! `session_recovered` or `session_failed`.

use std::sync::{Arc, Mutex, Weak};

use pa_agent::agent::Agent;
use pa_agent::types::{AgentEvent, AgentMessage, Message};
use pa_core::session::crash::SessionCrash;
use pa_core::session_engine::billing::SessionBilling;
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::headless::HARNESS_DIGEST_CUSTOM_TYPE;
use pa_core::session_engine::session_events::agent_event_json;
use pa_core::session_engine::wire_events::{
    direct_update_event, session_wire_value, settled_context_usage, stream_event_value,
    tool_result_wire_value, update_route, AssistantSnapshot, UpdateCoalescer, UpdateRoute,
    UPDATE_FLUSH_INTERVAL,
};
use pa_types::daemon::{SessionCrashInfo, SessionFaultEvent};
use pa_types::event_log::{NewEvent, Topic, Trust};
use pa_types::usage::ContextUsage;
use serde_json::{json, Value};

use super::{DriverEvents, Outbox};
use crate::lock;
use crate::trust::TaskTrust;

/// One session's stream.
pub(crate) struct SessionStream {
    outbox: Outbox,
    session: String,
    /// `agent/<id>/stream`; `None` for an id that is no topic level (such a
    /// session is refused before it opens, so nothing is lost).
    topic: Option<Topic>,
    /// The running task's trust, which the session's own events carry.
    trust: TaskTrust,
    state: Mutex<StreamState>,
}

#[derive(Default)]
struct StreamState {
    /// The session-event shapes, once a build chose them.
    sequenced: bool,
    /// The last `meta.sequence` the session consumed.
    sequence: u64,
    /// The open run's coalescer, from its `agent_start` to its `agent_end`.
    run: Option<Arc<UpdateCoalescer>>,
    /// The session's latest context reading.
    context: Option<ContextUsage>,
    /// The lifetime's billing stamp, which settled assistant rows carry.
    billing: Option<SessionBilling>,
}

impl SessionStream {
    pub(crate) fn new(outbox: Outbox, session: &str, trust: TaskTrust) -> Self {
        SessionStream {
            outbox,
            session: session.to_string(),
            topic: Topic::new(format!("agent/{session}/stream")).ok(),
            trust,
            state: Mutex::new(StreamState::default()),
        }
    }

    /// Takes in a lifetime's built engine: the shape its builder chose, its
    /// billing stamp, and the context reading its transcript starts from. A
    /// run the last lifetime left open is not continued.
    pub(crate) async fn adopt(&self, events: DriverEvents, engine: &SessionEngine) {
        let state = engine.session.agent().state().await;
        let context = settled_context_usage(&state.messages, state.model.context_window);
        let mut stream = lock(&self.state);
        if let Some(run) = stream.run.take() {
            run.close();
        }
        stream.sequenced = match events {
            DriverEvents::SessionEvents => true,
            DriverEvents::Stream | DriverEvents::Quiet => false,
        };
        stream.billing = Some(engine.session.billing());
        stream.context = context;
    }

    /// The last `meta.sequence` the session's stream consumed (0 before the
    /// first, and in the json stream's shapes).
    pub(crate) fn last_sequence(&self) -> u64 {
        lock(&self.state).sequence
    }

    /// Publishes one of the session's own frames (a driver event, a child's
    /// update, a summary delta) with the running task's trust.
    pub(crate) fn publish(&self, payload: &Value) {
        self.publish_as(payload, self.trust.current());
    }

    /// [`Self::publish`] with `trust` (a client's `!` command is the
    /// owner's).
    pub(crate) fn publish_as(&self, payload: &Value, trust: Trust) {
        let mut state = lock(&self.state);
        if state.sequenced {
            self.direct(&mut state, vec![payload.clone()], trust);
        } else {
            self.send(payload.clone(), trust);
        }
    }

    /// A fault of the session's, unsequenced, as the supervisor sends it.
    pub(crate) fn fault(&self, fault: &SessionFaultEvent) {
        self.send(
            serde_json::to_value(fault).unwrap_or_default(),
            Trust::Owner,
        );
    }

    /// The session crashed: `session_crashed`, then the end of a run the
    /// crash cut off. Its parked update is dropped.
    pub(crate) fn crashed(&self, crash: &SessionCrash) {
        let run = lock(&self.state).run.take();
        self.fault(&SessionFaultEvent::SessionCrashed(SessionCrashInfo {
            kind: crash.kind.clone(),
            message: crash.message.clone(),
            location: crash.location.clone(),
            thread: crash.thread.clone(),
            at: crash.at.clone(),
            state: None,
        }));
        if let Some(run) = run {
            run.close();
            self.send(
                json!({ "type": "turn_end", "error": crash.reason() }),
                Trust::Owner,
            );
            self.send(json!({ "type": "agent_end" }), Trust::Owner);
        }
    }

    /// Publishes what one agent event says, in the stream's shape. `agent`
    /// is the agent emitting it: a settled row's context reading reads its
    /// messages.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per agent event, each its old frames, read in the loop's order"
    )]
    pub(crate) async fn agent_event(self: &Arc<Self>, event: &AgentEvent, agent: &Weak<Agent>) {
        let (sequenced, billing) = {
            let state = lock(&self.state);
            (state.sequenced, state.billing.clone())
        };
        if !sequenced {
            if let Some(payload) = agent_event_json(event) {
                self.send(payload, self.trust.current());
            }
            return;
        }
        let trust = self.trust.current();
        let frames = match event {
            AgentEvent::AgentStart => {
                self.open_run();
                vec![json!({ "type": "agent_start" })]
            }
            AgentEvent::AgentEnd { messages } => {
                let messages: Vec<Value> = messages.iter().filter_map(session_wire_value).collect();
                let mut state = lock(&self.state);
                self.direct(
                    &mut state,
                    vec![json!({ "type": "agent_end", "messages": messages })],
                    trust,
                );
                if let Some(run) = state.run.take() {
                    run.close();
                }
                return;
            }
            AgentEvent::TurnStart => vec![json!({ "type": "turn_start" })],
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => {
                let Some(message) = stamped_wire_value(billing.as_ref(), message).await else {
                    return;
                };
                let tool_results: Vec<Value> = tool_results
                    .iter()
                    .filter_map(|result| session_wire_value(&AgentMessage::from(result.clone())))
                    .collect();
                vec![json!({ "type": "turn_end", "message": message, "toolResults": tool_results })]
            }
            AgentEvent::MessageStart { message } => {
                if !is_assistant(message) {
                    return;
                }
                let Some(value) = session_wire_value(message) else {
                    return;
                };
                vec![direct_update_event(value, Some(json!({ "type": "start" })))]
            }
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                if !is_assistant(message) {
                    return;
                }
                let stream_event = stream_event_value(assistant_message_event);
                match update_route(stream_event.as_ref()) {
                    UpdateRoute::Park { kind, delta } => {
                        self.park(AssistantSnapshot::Loop(Arc::clone(message)), &kind, &delta);
                        return;
                    }
                    UpdateRoute::Direct => {
                        match AssistantSnapshot::Loop(Arc::clone(message)).into_wire() {
                            Some(value) => vec![direct_update_event(value, stream_event)],
                            None => return,
                        }
                    }
                }
            }
            AgentEvent::MessageEnd { message } => {
                return self.settled(message, billing.as_ref(), agent, trust).await;
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => vec![json!({
                "type": "tool_execution_start",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "args": args,
            })],
            AgentEvent::ToolExecutionProgress { .. } => return,
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => vec![json!({
                "type": "tool_execution_update",
                "toolCallId": tool_call_id,
                "partialResult": tool_result_wire_value(partial_result),
            })],
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => vec![json!({
                "type": "tool_execution_end",
                "toolCallId": tool_call_id,
                "result": tool_result_wire_value(result),
                "isError": is_error,
            })],
        };
        self.direct(&mut lock(&self.state), frames, trust);
    }

    /// A settled row: an assistant row's `message_end` (with its billing
    /// stamp); a tool result's, a user row's and a custom row's
    /// `message_start`/`message_end` pair (the harness digest, which each
    /// lifetime renders afresh, goes out with its run's `agent_end` only).
    /// A conversation row's `message_end` carries the session's context
    /// reading after it.
    async fn settled(
        &self,
        message: &AgentMessage,
        billing: Option<&SessionBilling>,
        agent: &Weak<Agent>,
        trust: Trust,
    ) {
        let (value, pair, reads_context) = match message {
            AgentMessage::Standard(Message::Assistant(_)) => {
                (stamped_wire_value(billing, message).await, false, true)
            }
            AgentMessage::Standard(Message::ToolResult(_) | Message::User(_)) => {
                (session_wire_value(message), true, true)
            }
            AgentMessage::Custom(custom) => {
                if custom.payload.get("customType").and_then(Value::as_str)
                    == Some(HARNESS_DIGEST_CUSTOM_TYPE)
                {
                    return;
                }
                (session_wire_value(message), true, false)
            }
        };
        let Some(value) = value else {
            return;
        };
        let context = match agent.upgrade().filter(|_| reads_context) {
            Some(agent) => {
                let state = agent.state().await;
                Some(settled_context_usage(
                    &state.messages,
                    state.model.context_window,
                ))
            }
            None => None,
        };
        let mut end = json!({ "type": "message_end", "message": value });
        let mut state = lock(&self.state);
        if let Some(context) = context {
            if let Some(reading) = &context {
                end["contextUsage"] = json!(reading);
            }
            if let Some(run) = &state.run {
                run.set_context(context);
            }
            state.context = context;
        }
        let frames = if pair {
            vec![
                json!({ "type": "message_start", "message": end["message"] }),
                end,
            ]
        } else {
            vec![end]
        };
        self.direct(&mut state, frames, trust);
    }

    /// Opens a run: its coalescer, and the flusher that sends the trailing
    /// edge of what parks in it.
    fn open_run(self: &Arc<Self>) {
        let run = Arc::new(UpdateCoalescer::default());
        {
            let mut state = lock(&self.state);
            run.set_context(state.context);
            if let Some(previous) = state.run.replace(Arc::clone(&run)) {
                previous.close();
            }
        }
        let stream = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(due) = run.next_flush_at(UPDATE_FLUSH_INTERVAL) {
                tokio::time::sleep_until(due).await;
                let Some(stream) = stream.upgrade() else {
                    break;
                };
                let open = run.flush_pending(UPDATE_FLUSH_INTERVAL, |event, sequence| {
                    stream.send(sequenced(event, sequence), stream.trust.current());
                });
                if !open {
                    break;
                }
            }
        });
    }

    /// Parks one streamed update in the open run's coalescer; it consumes
    /// its number now. One outside a run opens none and goes nowhere.
    fn park(&self, message: AssistantSnapshot, kind: &str, delta: &str) {
        let mut state = lock(&self.state);
        let Some(run) = state.run.clone() else {
            return;
        };
        state.sequence += 1;
        let sequence = state.sequence;
        run.park_update(
            message,
            kind,
            delta,
            sequence,
            UPDATE_FLUSH_INTERVAL,
            |event, sequence| self.send(sequenced(event, sequence), self.trust.current()),
        );
    }

    /// Sends `frames`, each numbered, after the open run's parked update,
    /// all under the stream's lock: nothing comes between them.
    fn direct(&self, state: &mut StreamState, frames: Vec<Value>, trust: Trust) {
        let frames: Vec<Value> = frames
            .into_iter()
            .map(|frame| {
                state.sequence += 1;
                sequenced(frame, state.sequence)
            })
            .collect();
        let send_frames = || {
            for frame in frames {
                self.send(frame, trust);
            }
        };
        match &state.run {
            Some(run) => run.send_direct(
                |event, sequence| self.send(sequenced(event, sequence), self.trust.current()),
                send_frames,
            ),
            None => send_frames(),
        }
    }

    fn send(&self, payload: Value, trust: Trust) {
        let Some(topic) = &self.topic else {
            return;
        };
        self.outbox.send(NewEvent {
            topic: topic.clone(),
            source: self.session.clone(),
            session: Some(self.session.clone()),
            payload,
            trust,
            cause: None,
            reply: None,
        });
    }
}

/// `frame` with `sequence` as its `meta.sequence`.
fn sequenced(mut frame: Value, sequence: u64) -> Value {
    if let Some(object) = frame.as_object_mut() {
        object.insert("meta".to_string(), json!({ "sequence": sequence }));
    }
    frame
}

fn is_assistant(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Standard(Message::Assistant(_)))
}

/// The session wire form of a settled loop message; an assistant message
/// carries the session's billing stamp, like the core's own record of it.
async fn stamped_wire_value(
    billing: Option<&SessionBilling>,
    message: &AgentMessage,
) -> Option<Value> {
    let mut value = session_wire_value(message)?;
    if let (Some(billing), AgentMessage::Standard(Message::Assistant(assistant))) =
        (billing, message)
    {
        let assistant = pa_core::session_engine::provider_adapter::json_round_trip::<
            _,
            pa_types::ai::AssistantMessage,
        >(assistant)?;
        if let Some(mode) = billing.mode_of(&assistant).await {
            value["usage"]["billing"] = json!(mode);
        }
    }
    Some(value)
}
