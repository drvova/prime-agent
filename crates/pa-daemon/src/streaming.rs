//! The worker's live token stream: each turn's `message_update` events coalesce in pa-core's
//! [`UpdateCoalescer`] (the single slot throttled on both edges, its merging and its live
//! `contextUsage` estimate: `pa_core::session_engine::wire_events`), and this wrapper frames what
//! it flushes as the session's `session_event` with the sequence the update consumed, sends it on
//! the worker's event pump, and feeds the pump's backlog and drops back to the [`FlushPacer`].

use serde_json::{Map, Value};
use tokio::time::Instant;

pub(crate) use pa_core::session_engine::wire_events::FlushPacer;
use pa_core::session_engine::wire_events::{AssistantSnapshot, UpdateCoalescer};

use crate::protocol::{create_daemon_event_meta, DaemonOutbound};
use crate::worker::OutboundFrame;

/// The session identity a flushed frame is stamped with (the frame is
/// built at flush time, not at park time).
#[derive(Clone)]
struct SessionIdentity {
    active_session_id: String,
    generation: String,
}

/// One turn's coalescer on the worker's event pump: every send happens under the coalescer's
/// lock, so frame order is total.
pub(crate) struct TurnStreamCoalescer {
    coalescer: UpdateCoalescer,
    session: SessionIdentity,
}

impl TurnStreamCoalescer {
    pub(crate) fn new(active_session_id: String, generation: String) -> Self {
        TurnStreamCoalescer {
            coalescer: UpdateCoalescer::default(),
            session: SessionIdentity {
                active_session_id,
                generation,
            },
        }
    }

    /// Park one streamed update ([`UpdateCoalescer::park_update`]). `false` when the turn
    /// already ended.
    pub(crate) fn park_update(
        &self,
        message: AssistantSnapshot,
        kind: &str,
        delta: &str,
        sequence: u64,
        events: &crate::worker::EventPump,
    ) -> bool {
        self.coalescer.park_update(
            message,
            kind,
            delta,
            sequence,
            events.flush.interval(),
            |event, sequence| self.send_update(event, sequence, events),
        )
    }

    /// Broadcast every `payloads` frame directly, after flushing any parked
    /// update first. All sends happen under the coalescer lock, so the flusher
    /// can never interleave between the parked update and its settling frame.
    pub(crate) fn send_direct(&self, payloads: &[Vec<u8>], events: &crate::worker::EventPump) {
        self.coalescer.send_direct(
            |event, sequence| self.send_update(event, sequence, events),
            || {
                for payload in payloads {
                    events.send(OutboundFrame::session_event(payload.clone()));
                }
            },
        );
    }

    /// Flusher tick ([`UpdateCoalescer::flush_pending`]): `false` once the turn closed and the
    /// flusher should stop.
    pub(crate) fn flush_pending(&self, events: &crate::worker::EventPump) -> bool {
        self.coalescer
            .flush_pending(events.flush.interval(), |event, sequence| {
                self.send_update(event, sequence, events);
            })
    }

    /// When the flusher should next look at the slot ([`UpdateCoalescer::next_flush_at`]).
    pub(crate) fn next_flush_at(&self, events: &crate::worker::EventPump) -> Option<Instant> {
        self.coalescer.next_flush_at(events.flush.interval())
    }

    /// Record the session's latest context reading ([`UpdateCoalescer::set_context`]).
    pub(crate) fn set_context(&self, reading: Option<pa_types::usage::ContextUsage>) {
        self.coalescer.set_context(reading);
    }

    /// End of turn ([`UpdateCoalescer::close`]).
    pub(crate) fn close(&self) {
        self.coalescer.close();
    }

    /// Frame and broadcast one flushed update (under the coalescer's lock), then let the pacer
    /// see the subscribers.
    fn send_update(&self, event: Value, sequence: u64, events: &crate::worker::EventPump) {
        let meta = create_daemon_event_meta(
            &self.session.active_session_id,
            sequence,
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
    use pa_core::session_engine::wire_events::{LAGGING_BACKLOG, UPDATE_FLUSH_INTERVAL};
    use serde_json::json;

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

    #[test]
    fn flush_without_parked_frames_is_a_kept_alive_noop() {
        let (pump, mut rx) = subscribe();
        let coalescer = TurnStreamCoalescer::new("s1".to_string(), "g1".to_string());
        assert!(coalescer.flush_pending(&pump));
        assert!(rx.try_recv().is_err());
    }
}
