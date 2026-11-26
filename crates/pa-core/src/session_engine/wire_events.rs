//! The session-event wire builder: the `session_event` payloads a daemon sends its clients for
//! one session's turns, built from the agent loop's events. Two hosts send them, so the shapes
//! and the coalescing live here once:
//!
//! - the wire forms of the loop's messages, stream deltas and tool results
//!   ([`session_wire_value`], [`stream_event_value`], [`tool_result_wire_value`]);
//! - how a streamed assistant update travels ([`update_route`]): a reply's `start` opens it as a
//!   `message_start` carrying `{"type": "start"}`, a block end settles the run and goes out
//!   direct, and every delta parks in the turn's [`UpdateCoalescer`];
//! - the coalescer itself: a single slot throttled on both edges. An update that arrives when
//!   nothing went out within the interval goes out at once (the leading edge: the first token,
//!   the first after a pause), and anything parked behind it waits for the host's flusher, which
//!   drains the trailing edge one interval after the last send. Every other frame flushes the
//!   parked update first ([`UpdateCoalescer::send_direct`]), so wire order is unchanged. The
//!   deltas are additive, so a parked run of one kind merges its delta text;
//! - the live `contextUsage` estimate each flushed update carries ([`streaming_context_usage`]),
//!   computed once per flush, never per token, and the adaptive flush interval
//!   ([`FlushPacer`]).
//!
//! The print modes' json stream is a different surface (`session_events::agent_event_json`: one
//! line per loop event, no coalescing) and does not use this module.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pa_types::context_breakdown::RequestCache;
use pa_types::usage::{estimate_tokens, ContextUsage};
use serde_json::{json, Value};
use tokio::time::Instant;

use super::provider_adapter::json_round_trip;

/// A streamed assistant message: already in wire form, or the loop's typed partial, converted
/// only when a frame is built (a parked update superseded before its flush is never converted).
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantSnapshot {
    Wire(Value),
    Loop(Arc<pa_agent::types::AgentMessage>),
}

impl AssistantSnapshot {
    /// The message's session wire form; `None` when the loop message does not convert.
    #[must_use]
    pub fn into_wire(self) -> Option<Value> {
        match self {
            Self::Wire(value) => Some(value),
            Self::Loop(message) => session_wire_value(&message),
        }
    }
}

/// Serialize a loop message through the session wire shape (adds `role`).
#[must_use]
pub fn session_wire_value(agent_message: &pa_agent::types::AgentMessage) -> Option<Value> {
    use pa_agent::types::Message as LoopMessage;
    let session_message = match agent_message {
        pa_agent::types::AgentMessage::Standard(LoopMessage::User(user)) => {
            pa_types::session::AgentMessage::User(json_round_trip(user)?)
        }
        pa_agent::types::AgentMessage::Standard(LoopMessage::Assistant(assistant)) => {
            pa_types::session::AgentMessage::Assistant(json_round_trip(assistant)?)
        }
        pa_agent::types::AgentMessage::Standard(LoopMessage::ToolResult(tool_result)) => {
            pa_types::session::AgentMessage::ToolResult(json_round_trip(tool_result)?)
        }
        // A custom row: the payload is the session-shape custom message and
        // the wire form is the tagged session message (payload plus role).
        pa_agent::types::AgentMessage::Custom(custom) => {
            let mut value = custom.payload.clone();
            let object = value.as_object_mut()?;
            object
                .entry("role".to_string())
                .or_insert_with(|| Value::String(custom.role.clone()));
            return Some(value);
        }
    };
    serde_json::to_value(&session_message).ok()
}

/// Wire form of one provider stream event: the event `type` plus the `delta` when the event
/// carries one. The reply's `start` is kept (it opens the `message_start`); the terminal
/// `done`/`error` map to `None`.
#[must_use]
pub fn stream_event_value(event: &pa_agent::stream::AssistantMessageEvent) -> Option<Value> {
    use pa_agent::stream::AssistantMessageEvent;
    let (kind, delta) = match event {
        AssistantMessageEvent::Start { .. } => ("start", None),
        AssistantMessageEvent::TextStart { .. } => ("text_start", None),
        AssistantMessageEvent::TextDelta { delta, .. } => ("text_delta", Some(delta.as_str())),
        AssistantMessageEvent::TextEnd { .. } => ("text_end", None),
        AssistantMessageEvent::ThinkingStart { .. } => ("thinking_start", None),
        AssistantMessageEvent::ThinkingDelta { delta, .. } => {
            ("thinking_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ThinkingEnd { .. } => ("thinking_end", None),
        AssistantMessageEvent::ToolCallStart { .. } => ("toolcall_start", None),
        AssistantMessageEvent::ToolCallDelta { delta, .. } => {
            ("toolcall_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ToolCallEnd { .. } => ("toolcall_end", None),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => return None,
    };
    match delta {
        Some(delta) => Some(json!({ "type": kind, "delta": delta })),
        None => Some(json!({ "type": kind })),
    }
}

/// Wire form of one tool result (the tool-execution events' `result` / `partialResult`).
#[must_use]
pub fn tool_result_wire_value(result: &pa_agent::types::AgentToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|block| serde_json::to_value(block).unwrap_or(Value::Null))
        .collect();
    json!({ "content": content, "details": result.details })
}

/// How one streamed assistant update travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRoute {
    /// Out at once, flushing the parked run first: the reply's `start` (a `message_start`) or a
    /// block end that settles the parked run (a `message_update`). Build it with
    /// [`direct_update_event`].
    Direct,
    /// Into the turn's [`UpdateCoalescer`]: a delta (or a block start) of `kind`, with its
    /// additive `delta` text.
    Park { kind: String, delta: String },
}

/// The route of an update whose stream event is `stream_event` (its [`stream_event_value`]).
#[must_use]
pub fn update_route(stream_event: Option<&Value>) -> UpdateRoute {
    let kind = stream_event
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str);
    if matches!(
        kind,
        Some("start" | "text_end" | "thinking_end" | "toolcall_end")
    ) {
        return UpdateRoute::Direct;
    }
    UpdateRoute::Park {
        kind: kind.unwrap_or_default().to_string(),
        delta: stream_event
            .and_then(|event| event.get("delta"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// The event of a [`UpdateRoute::Direct`] update: a `message_start` for the reply's `start`,
/// else a `message_update`, carrying the stream event as `assistantMessageEvent`.
#[must_use]
pub fn direct_update_event(message: Value, stream_event: Option<Value>) -> Value {
    let starts_message = stream_event
        .as_ref()
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
        == Some("start");
    let mut event = json!({
        "type": if starts_message { "message_start" } else { "message_update" },
    });
    event["message"] = message;
    if let Some(stream_event) = stream_event {
        event["assistantMessageEvent"] = stream_event;
    }
    event
}

/// The live context estimate while an assistant reply streams: the prompt the provider already
/// reported for this request (Anthropic's `message_start` usage), else the last reading (the
/// settled context plus the estimated rows appended since), plus the streamed output so far by
/// the chars/4 heuristic. A provider's mid-stream output count is not trusted (Anthropic reports
/// a placeholder until `message_delta`). A reported prompt also yields this request's cache
/// split. `None` when neither anchor is known (right after a compaction, before the first reply).
#[must_use]
pub fn streaming_context_usage(
    base: Option<&ContextUsage>,
    partial: &Value,
) -> Option<ContextUsage> {
    let base = base?;
    let reported = partial
        .get("usage")
        .and_then(|usage| serde_json::from_value::<RequestCache>(usage.clone()).ok())
        .filter(|request| request.input + request.cache_read + request.cache_write > 0);
    let prompt = match reported {
        Some(request) => request.input + request.cache_read + request.cache_write,
        None => base.tokens?,
    };
    Some(ContextUsage {
        estimated: true,
        last_request: reported.or(base.last_request),
        ..ContextUsage::of(prompt + estimate_tokens(partial), base.context_window)
    })
}

/// The starting gap between two sent updates: the leading edge goes out at once, the trailing
/// edge one interval after the previous send, so the interval bounds both the send rate and the
/// staleness of a parked update. [`FlushPacer`] moves it between 16 and 100 ms.
pub const UPDATE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// The shortest interval (about 60 Hz): subscribers keeping up get smooth streaming.
const MIN_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
/// The longest interval (10 Hz): a lagging subscriber gets fewer, larger updates.
const MAX_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// A subscriber this many frames behind is lagging.
pub const LAGGING_BACKLOG: usize = 64;

/// The flush pacing's tuning on `pa_types::control`'s controller: the controlled rate is
/// flushes a minute (60 000 / interval). Clean flushes add 120 a minute each, a lagging
/// subscriber halves the rate, a dropped frame halves it too, and a cut ages out after 30 s.
const FLUSH_CONTROLLER_CONFIG: pa_types::control::ControllerConfig =
    pa_types::control::ControllerConfig {
        initial_concurrency: 1,
        min_concurrency: 1,
        max_concurrency: 1,
        initial_rate_per_min: 1_200,
        min_rate_per_min: 600,
        max_rate_per_min: 3_750,
        rate_step_per_min: 120,
        decrease_keep_pct: 50,
        latency_keep_pct: 100,
        latency_rise_pct: 300,
        latency_rise_samples: 3,
        headroom_pct: 0,
        decrease_spacing_ms: 500,
        ceiling_ttl_ms: 30_000,
    };

/// The adaptive `message_update` flush interval: after every send the host reports how far
/// behind the slowest subscriber is and whether a subscriber dropped frames, and the interval
/// moves between 16 and 100 ms. Only the trailing edge follows it; the leading edge (the first
/// token, the first after a pause) always goes out at once.
pub struct FlushPacer {
    state: Mutex<PacerState>,
}

struct PacerState {
    controller: pa_types::control::Controller,
    /// The drop count at the last observation.
    seen_drops: u64,
    started: Instant,
}

impl Default for FlushPacer {
    fn default() -> Self {
        let config = pa_types::control::ControllerConfig {
            initial_rate_per_min: u32::try_from(60_000 / UPDATE_FLUSH_INTERVAL.as_millis())
                .unwrap_or(FLUSH_CONTROLLER_CONFIG.initial_rate_per_min),
            ..FLUSH_CONTROLLER_CONFIG
        };
        FlushPacer {
            state: Mutex::new(PacerState {
                controller: pa_types::control::Controller::new(config, 0),
                seen_drops: 0,
                started: Instant::now(),
            }),
        }
    }
}

impl FlushPacer {
    /// The interval to apply now.
    #[must_use]
    pub fn interval(&self) -> Duration {
        let rate = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .controller
            .target()
            .rate_per_min;
        Duration::from_millis(60_000 / u64::from(rate.max(1)))
            .clamp(MIN_FLUSH_INTERVAL, MAX_FLUSH_INTERVAL)
    }

    /// Fold in one send's view of the subscribers: their backlog and the total frames dropped
    /// so far.
    pub fn observe(&self, backlog: usize, dropped: u64) {
        use pa_types::control::{LimitReport, Observation, Outcome};
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let outcome = if dropped > state.seen_drops {
            Outcome::Throttled
        } else if backlog >= LAGGING_BACKLOG {
            Outcome::Overloaded
        } else {
            Outcome::Success
        };
        state.seen_drops = dropped;
        let now_ms = u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _target = state.controller.observe(
            &Observation {
                outcome,
                latency_ms: None,
                limits: LimitReport::default(),
            },
            now_ms,
        );
    }
}

/// The parked `message_update`: the newest snapshot plus the merged additive delta run of the
/// superseded updates.
struct PendingUpdate {
    message: AssistantSnapshot,
    /// The stream event kind of the parked run (`text_delta`, ...).
    kind: String,
    /// The concatenated delta text of every merged update of the same kind.
    delta: String,
    /// The event sequence the newest parked update consumed.
    sequence: u64,
}

/// Single-slot coalescer for one turn's `message_update` events, shared between the turn's emit
/// path (park, direct send) and the host's flusher. Every send happens under its lock, so the
/// order of what it sends is total. What a host does with a flushed update - frame it with its
/// `sequence` and send it - is the `send` callback each flushing call takes, run under that
/// lock.
#[derive(Default)]
pub struct UpdateCoalescer {
    inner: Mutex<CoalescerInner>,
}

#[derive(Default)]
struct CoalescerInner {
    /// The latest parked update. Replaced by newer snapshots of the same stream-event kind
    /// (their delta text merges into `delta`).
    pending: Option<PendingUpdate>,
    /// The turn ended: park no more, flush nothing, stop the flusher.
    closed: bool,
    /// When the last parked update was sent (leading edge, trailing edge, or flushed ahead of a
    /// direct frame); `None` before the turn's first.
    last_flush: Option<Instant>,
    /// The session's latest context reading (the turn's start, then every settled row): the
    /// anchor the flushed updates' streaming estimate grows from. `None` without a known window.
    context: Option<ContextUsage>,
}

impl UpdateCoalescer {
    fn lock(&self) -> std::sync::MutexGuard<'_, CoalescerInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Park one streamed update: the newest snapshot wins; the delta merges into the parked run
    /// (same kind) or starts a fresh one. When nothing was sent within `interval` the run goes
    /// out now through `send` (the leading edge) instead of waiting for the flusher. `false`
    /// when the turn already ended.
    pub fn park_update(
        &self,
        message: AssistantSnapshot,
        kind: &str,
        delta: &str,
        sequence: u64,
        interval: Duration,
        send: impl FnOnce(Value, u64),
    ) -> bool {
        let mut inner = self.lock();
        if inner.closed {
            return false;
        }
        match &mut inner.pending {
            // Same kind: the newer snapshot wins and its delta text joins
            // the run (deltas are additive for delta-mapping consumers).
            Some(pending) if pending.kind == kind => {
                pending.delta.push_str(delta);
                pending.message = message;
                pending.sequence = sequence;
            }
            // Kind switch: the newer snapshot wins whole. A `*_start`
            // event carries no delta, so nothing additive is lost.
            _ => {
                inner.pending = Some(PendingUpdate {
                    message,
                    kind: kind.to_string(),
                    delta: delta.to_string(),
                    sequence,
                });
            }
        }
        if inner.last_flush.is_none_or(|at| at.elapsed() >= interval) {
            flush_locked(&mut inner, send);
        }
        true
    }

    /// Flush any parked update through `send_update`, then run `send_frames`, both under the
    /// lock, so the flusher can never interleave between the parked update and the frame that
    /// settles it.
    pub fn send_direct(&self, send_update: impl FnOnce(Value, u64), send_frames: impl FnOnce()) {
        let mut inner = self.lock();
        flush_locked(&mut inner, send_update);
        send_frames();
    }

    /// Flusher tick: send the parked update (the trailing edge) once a full `interval` passed
    /// since the last send. Returns `false` once the turn closed and the flusher should stop.
    pub fn flush_pending(&self, interval: Duration, send: impl FnOnce(Value, u64)) -> bool {
        let mut inner = self.lock();
        if inner.closed {
            return false;
        }
        if inner.last_flush.is_none_or(|at| at.elapsed() >= interval) {
            flush_locked(&mut inner, send);
        }
        true
    }

    /// When the flusher should next look at the slot: one `interval` after the last send (the
    /// earliest the trailing edge may go out), or one interval from now when that moment already
    /// passed or nothing was sent yet (a fresh park then takes the leading edge itself). `None`
    /// once the turn closed.
    #[must_use]
    pub fn next_flush_at(&self, interval: Duration) -> Option<Instant> {
        let inner = self.lock();
        if inner.closed {
            return None;
        }
        let now = Instant::now();
        Some(
            inner
                .last_flush
                .map(|at| at + interval)
                .filter(|due| *due > now)
                .unwrap_or(now + interval),
        )
    }

    /// Record the session's latest context reading (a settled row's `message_end` carries the
    /// same value): the next streamed reply's estimate grows from it.
    pub fn set_context(&self, reading: Option<ContextUsage>) {
        self.lock().context = reading;
    }

    /// End of turn: nothing parked after this point is sent (an aborted turn's stale partial
    /// must not appear after its settle events), and anything still parked is dropped.
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.pending = None;
    }
}

/// Build and send the parked update (the caller holds the lock): the wire conversion runs here,
/// once per flush.
fn flush_locked(inner: &mut CoalescerInner, send: impl FnOnce(Value, u64)) {
    let Some(pending) = inner.pending.take() else {
        return;
    };
    inner.last_flush = Some(Instant::now());
    let Some(message) = pending.message.into_wire() else {
        return;
    };
    let context = streaming_context_usage(inner.context.as_ref(), &message);
    let mut event = json!({
        "type": "message_update",
        "message": message,
    });
    if let Some(context) = context {
        event["contextUsage"] = json!(context);
    }
    if !pending.kind.is_empty() {
        let mut stream_event = json!({ "type": pending.kind });
        if !pending.delta.is_empty() {
            stream_event["delta"] = json!(pending.delta);
        }
        event["assistantMessageEvent"] = stream_event;
    }
    send(event, pending.sequence);
}

#[cfg(test)]
mod tests;
