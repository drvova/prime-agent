//! Live token-stream coalescing: the emit path parks `message_update` frames in a
//! single-slot coalescer, throttled on both edges: an update that arrives when nothing went out
//! within the interval broadcasts at once (the leading edge: the first token, the first token
//! after a pause), and anything parked behind it waits for the flusher, which drains the
//! trailing edge one interval after the last broadcast. Every other frame flushes the parked
//! update first, so wire order stays identical. The deltas are ADDITIVE, so the parked run merges
//! delta text; block-end events flush it.
//!
//! Each flushed update also carries the live `contextUsage` estimate (the last context reading,
//! or the request's reported prompt, plus the streamed output so far), computed once per flush,
//! never per token.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use serde_json::{json, Map};

use crate::engine::AssistantSnapshot;
use crate::protocol::{create_daemon_event_meta, DaemonOutbound};
use crate::worker::OutboundFrame;

/// The starting gap between two broadcast updates: the leading edge goes out at once, the
/// trailing edge one interval after the previous broadcast, so the interval bounds both the
/// broadcast rate and the staleness of a parked update. [`FlushPacer`] moves it between
/// [`MIN_FLUSH_INTERVAL`] and [`MAX_FLUSH_INTERVAL`].
pub(crate) const UPDATE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// The shortest interval (about 60 Hz): subscribers keeping up get smooth streaming.
const MIN_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
/// The longest interval (10 Hz): a lagging subscriber gets fewer, larger updates.
const MAX_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// A subscriber this many frames behind is lagging.
const LAGGING_BACKLOG: usize = 64;

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

/// The worker's adaptive `message_update` flush interval: after every broadcast it reads how
/// far behind the slowest subscriber is and whether a subscriber dropped frames, and moves the
/// interval between 16 and 100 ms. Only the trailing edge follows it; the leading edge
/// (the first token, the first after a pause) always goes out at once.
pub(crate) struct FlushPacer {
    state: Mutex<PacerState>,
}

struct PacerState {
    controller: pa_types::control::Controller,
    /// The drop count at the last observation.
    seen_drops: u64,
    started: Instant,
}

impl FlushPacer {
    pub(crate) fn new() -> Self {
        let started = Instant::now();
        let config = pa_types::control::ControllerConfig {
            initial_rate_per_min: u32::try_from(60_000 / UPDATE_FLUSH_INTERVAL.as_millis())
                .unwrap_or(FLUSH_CONTROLLER_CONFIG.initial_rate_per_min),
            ..FLUSH_CONTROLLER_CONFIG
        };
        FlushPacer {
            state: Mutex::new(PacerState {
                controller: pa_types::control::Controller::new(config, 0),
                seen_drops: 0,
                started,
            }),
        }
    }

    /// The interval to apply now.
    pub(crate) fn interval(&self) -> Duration {
        let rate = self.state.lock().unwrap().controller.target().rate_per_min;
        Duration::from_millis(60_000 / u64::from(rate.max(1)))
            .clamp(MIN_FLUSH_INTERVAL, MAX_FLUSH_INTERVAL)
    }

    /// Fold in one broadcast's view of the subscribers: their backlog and the total frames
    /// dropped so far.
    pub(crate) fn observe(&self, backlog: usize, dropped: u64) {
        use pa_types::control::{LimitReport, Observation, Outcome};
        let mut state = self.state.lock().unwrap();
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

/// The session identity a flushed frame is stamped with (the frame is
/// built at flush time, not at park time).
#[derive(Clone)]
struct SessionIdentity {
    active_session_id: String,
    generation: String,
}

/// The parked `message_update`: the newest snapshot plus the merged
/// additive delta run of the superseded frames.
struct PendingUpdate {
    message: AssistantSnapshot,
    /// The stream event kind of the parked run (`text_delta`, ...).
    kind: String,
    /// The concatenated delta text of every merged frame of the same kind.
    delta: String,
    /// The broadcast sequence the newest parked frame consumed.
    sequence: u64,
}

/// Single-slot coalescer for one turn's `message_update` frames. Shared
/// between the turn's emit path (park/direct-send) and the flusher task;
/// every broadcast happens under `inner`, so frame order is total.
pub(crate) struct TurnStreamCoalescer {
    inner: Mutex<CoalescerInner>,
    session: SessionIdentity,
}

struct CoalescerInner {
    /// The latest parked update. Replaced by newer snapshots of the same
    /// stream-event kind (their delta text merges into `delta`).
    pending: Option<PendingUpdate>,
    /// The turn ended: park no more, flush nothing, stop the flusher.
    closed: bool,
    /// When the last parked update was broadcast (leading edge, trailing edge, or flushed ahead
    /// of a direct frame); `None` before the turn's first.
    last_flush: Option<Instant>,
    /// The session's latest context reading (the turn's start, then every settled row): the
    /// anchor the flushed updates' streaming estimate grows from. `None` without a known window.
    context: Option<pa_types::usage::ContextUsage>,
}

impl TurnStreamCoalescer {
    pub(crate) fn new(active_session_id: String, generation: String) -> Self {
        TurnStreamCoalescer {
            inner: Mutex::new(CoalescerInner {
                pending: None,
                closed: false,
                last_flush: None,
                context: None,
            }),
            session: SessionIdentity {
                active_session_id,
                generation,
            },
        }
    }

    /// Park one streamed update: the newest snapshot wins; the delta merges into the
    /// parked run (same kind) or starts a fresh one. When nothing was broadcast within the
    /// interval the run goes out now (the leading edge) instead of waiting for the flusher.
    /// `false` when the turn already ended.
    pub(crate) fn park_update(
        &self,
        message: AssistantSnapshot,
        kind: &str,
        delta: &str,
        sequence: u64,
        events: &crate::worker::EventPump,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
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
        if inner
            .last_flush
            .is_none_or(|at| at.elapsed() >= events.flush.interval())
        {
            self.flush_locked(&mut inner, events);
        }
        true
    }

    /// Broadcast every `payloads` frame directly, after flushing any parked
    /// update first. All sends happen under the coalescer lock, so the flusher
    /// can never interleave between the parked update and its settling frame.
    pub(crate) fn send_direct(&self, payloads: &[Vec<u8>], events: &crate::worker::EventPump) {
        let mut inner = self.inner.lock().unwrap();
        self.flush_locked(&mut inner, events);
        for payload in payloads {
            events.send(OutboundFrame::session_event(payload.clone()));
        }
    }

    /// Flusher tick: broadcast the parked update (the trailing edge) once a full interval
    /// passed since the last broadcast. Returns `false` once the turn closed and the flusher
    /// should stop.
    pub(crate) fn flush_pending(&self, events: &crate::worker::EventPump) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return false;
        }
        if inner
            .last_flush
            .is_none_or(|at| at.elapsed() >= events.flush.interval())
        {
            self.flush_locked(&mut inner, events);
        }
        true
    }

    /// When the flusher should next look at the slot: one interval after the last broadcast
    /// (the earliest the trailing edge may go out), or one interval from now when that moment
    /// already passed or nothing was broadcast yet (a fresh park then takes the leading edge
    /// itself). `None` once the turn closed.
    pub(crate) fn next_flush_at(&self, events: &crate::worker::EventPump) -> Option<Instant> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        let now = Instant::now();
        let interval = events.flush.interval();
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
    pub(crate) fn set_context(&self, reading: Option<pa_types::usage::ContextUsage>) {
        self.inner.lock().unwrap().context = reading;
    }

    /// End of turn: nothing parked after this point is broadcast (an aborted
    /// turn's stale partial must not appear after its settle events), and
    /// anything still parked is dropped.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.pending = None;
    }

    /// Serialize and broadcast the parked update (the caller holds `inner`):
    /// the wire conversion runs here, once per flush.
    fn flush_locked(&self, inner: &mut CoalescerInner, events: &crate::worker::EventPump) {
        let Some(pending) = inner.pending.take() else {
            return;
        };
        inner.last_flush = Some(Instant::now());
        let Some(message) = pending.message.into_wire() else {
            return;
        };
        let context =
            crate::session_stats::streaming_context_usage(inner.context.as_ref(), &message);
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
        let meta = create_daemon_event_meta(
            &self.session.active_session_id,
            pending.sequence,
            None,
            Some(&self.session.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: self.session.active_session_id.clone(),
            event,
            meta: Some(meta),
            rest: Map::default(),
        };
        let payload = serde_json::to_vec(&outbound).unwrap_or_default();
        events.send(OutboundFrame::session_event(payload));
        events.flush.observe(events.backlog(), events.dropped());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_payload(event: &Value) -> Vec<u8> {
        serde_json::to_vec(&json!({ "event": event })).unwrap_or_default()
    }

    fn subscribe() -> (
        crate::worker::EventPump,
        broadcast::Receiver<Arc<OutboundFrame>>,
    ) {
        let pump = crate::worker::EventPump::new();
        let rx = pump.subscribe();
        (pump, rx)
    }

    fn session_event_of(frame: &std::sync::Arc<OutboundFrame>) -> Value {
        serde_json::from_slice::<Value>(&frame.payload).expect("session event payload")
    }

    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    /// The leading edge: a lone update with nothing broadcast within the interval goes out at
    /// once. The clock is paused and never advanced, so a tick-driven flush cannot be what sent it.
    #[tokio::test(start_paused = true)]
    async fn a_lone_update_broadcasts_without_waiting_for_the_tick() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "first" })),
            "text_delta",
            "first",
            1,
            &pump,
        ));
        let sent = session_event_of(&rx.try_recv().expect("the leading edge broadcast"));
        assert_eq!(sent["event"]["message"]["text"], "first");
        assert_eq!(sent["meta"]["sequence"], 1);
        // An update inside the interval parks for the trailing edge; the flusher drains it only
        // once a full interval passed since the leading broadcast.
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "first second" })),
            "text_delta",
            " second",
            2,
            &pump,
        ));
        assert!(rx.try_recv().is_err(), "the follow-up parks");
        assert!(coalescer.flush_pending(&pump));
        assert!(
            rx.try_recv().is_err(),
            "the trailing edge waits out the interval"
        );
        tokio::time::advance(UPDATE_FLUSH_INTERVAL).await;
        assert!(coalescer.flush_pending(&pump));
        let trailing = session_event_of(&rx.try_recv().expect("the trailing edge broadcast"));
        assert_eq!(trailing["event"]["message"]["text"], "first second");
        assert_eq!(
            trailing["event"]["assistantMessageEvent"]["delta"],
            " second"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_flush_interval_shortens_while_subscribers_keep_up_and_backs_off_on_lag_or_drops() {
        let pacer = FlushPacer::new();
        let mut intervals = vec![pacer.interval()];
        for (backlog, dropped) in [(0, 0), (0, 0), (80, 0), (0, 0), (0, 5), (0, 5)] {
            tokio::time::advance(Duration::from_secs(1)).await;
            pacer.observe(backlog, dropped);
            intervals.push(pacer.interval());
        }
        assert_eq!(
            intervals,
            [50, 45, 41, 83, 71, 100, 83].map(Duration::from_millis)
        );
    }

    /// A subscriber a ring-segment behind stretches the trailing edge past the starting 50 ms;
    /// the leading edge still goes out at once.
    #[tokio::test(start_paused = true)]
    async fn a_lagging_subscriber_stretches_the_trailing_edge() {
        let (pump, mut rx) = subscribe();
        // The slow subscriber: frames it has not read yet.
        for _ in 0..LAGGING_BACKLOG {
            pump.send(OutboundFrame::session_event(Vec::new()));
        }
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "lead" })),
            "text_delta",
            "lead",
            1,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "lead more" })),
            "text_delta",
            " more",
            2,
            &pump,
        ));
        let mut trailing_at = Vec::new();
        for _ in 0..2 {
            tokio::time::advance(UPDATE_FLUSH_INTERVAL).await;
            assert!(coalescer.flush_pending(&pump));
            trailing_at.push(pump.backlog());
        }
        // The leading edge joined the backlog at once; the trailing edge only after 100 ms.
        assert_eq!(trailing_at, [LAGGING_BACKLOG + 1, LAGGING_BACKLOG + 2]);
        drop(rx.try_recv());
    }

    #[test]
    fn direct_sends_flush_the_parked_update_first() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        // The first update takes the leading edge; the second parks behind it.
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "part" })),
            "text_delta",
            "part",
            1,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "partial" })),
            "text_delta",
            "ial",
            2,
            &pump,
        ));
        // The parked update waits for the flusher until a direct frame
        // arrives: the direct send must broadcast the parked snapshot first.
        coalescer.send_direct(&[event_payload(&json!({ "type": "message_end" }))], &pump);
        let leading = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(leading["event"]["message"]["text"], "part");
        let parked = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(parked["event"]["type"], "message_update");
        assert_eq!(parked["event"]["message"]["text"], "partial");
        let end = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(end["event"]["type"], "message_end");
        assert!(rx.try_recv().is_err(), "no further frames");
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_snapshot_merges_the_superseded_deltas() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "The" })),
            "text_delta",
            "The ",
            1,
            &pump,
        ));
        let leading = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(leading["event"]["message"]["text"], "The");
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "The Thames" })),
            "text_delta",
            "Thames ",
            2,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "The Thames flows" })),
            "text_delta",
            "flows",
            3,
            &pump,
        ));
        tokio::time::advance(UPDATE_FLUSH_INTERVAL).await;
        assert!(coalescer.flush_pending(&pump));
        let flushed = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(flushed["event"]["message"]["text"], "The Thames flows");
        assert_eq!(
            flushed["event"]["assistantMessageEvent"]["delta"],
            "Thames flows"
        );
        // The flushed frame carries the newest frame's broadcast sequence.
        assert_eq!(flushed["meta"]["sequence"], 3);
        assert!(
            rx.try_recv().is_err(),
            "the superseded snapshots are merged"
        );
    }

    #[test]
    fn a_kind_switch_replaces_the_run() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "think" })),
            "thinking_delta",
            "think",
            1,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "thinkmore" })),
            "thinking_delta",
            "more",
            2,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "thinkmoreanswer" })),
            "text_delta",
            "answer",
            3,
            &pump,
        ));
        coalescer.send_direct(&[event_payload(&json!({ "type": "turn_end" }))], &pump);
        let leading = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(leading["event"]["assistantMessageEvent"]["delta"], "think");
        let flushed = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(
            flushed["event"]["assistantMessageEvent"]["type"],
            "text_delta"
        );
        assert_eq!(flushed["event"]["assistantMessageEvent"]["delta"], "answer");
        let direct = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(direct["event"]["type"], "turn_end");
        assert!(
            rx.try_recv().is_err(),
            "no frames beyond the leading update, the flushed run and the direct one"
        );
    }

    #[test]
    fn close_stops_parking_and_flushing() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "lead" })),
            "text_delta",
            "lead",
            1,
            &pump,
        ));
        assert!(coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "leadpartial" })),
            "text_delta",
            "partial",
            2,
            &pump,
        ));
        coalescer.close();
        assert!(!coalescer.park_update(
            AssistantSnapshot::Wire(json!({ "text": "stale" })),
            "text_delta",
            "s",
            3,
            &pump,
        ));
        assert!(!coalescer.flush_pending(&pump), "the flusher stops");
        assert!(
            coalescer.next_flush_at(&pump).is_none(),
            "a closed turn arms no tick"
        );
        let leading = session_event_of(&rx.try_recv().unwrap());
        assert_eq!(leading["event"]["message"]["text"], "lead");
        assert!(
            rx.try_recv().is_err(),
            "the parked partial is dropped and a closed turn parks nothing"
        );
    }

    #[test]
    fn flush_without_parked_frames_is_a_kept_alive_noop() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.flush_pending(&pump));
        assert!(rx.try_recv().is_err());
    }
}
