use std::cell::RefCell;

use serde_json::{json, Value};

use super::*;

/// What a coalescer sent, in order: each update's event and sequence.
#[derive(Default)]
struct Sent(RefCell<Vec<(Value, u64)>>);

impl Sent {
    fn send(&self) -> impl FnOnce(Value, u64) + '_ {
        |event, sequence| self.0.borrow_mut().push((event, sequence))
    }

    fn take(&self) -> Vec<(Value, u64)> {
        self.0.take()
    }
}

fn snapshot(text: &str) -> AssistantSnapshot {
    AssistantSnapshot::Wire(json!({ "text": text }))
}

fn update(text: &str, kind: &str, delta: Option<&str>) -> Value {
    let mut stream_event = json!({ "type": kind });
    if let Some(delta) = delta {
        stream_event["delta"] = json!(delta);
    }
    json!({
        "type": "message_update",
        "message": { "text": text },
        "assistantMessageEvent": stream_event,
    })
}

/// The coalescing contract on one turn: the leading edge goes out at once, the updates behind it
/// merge their deltas (a newer snapshot wins and carries the newest sequence) until a full
/// interval passed, a kind switch starts a fresh run, a direct send flushes the parked run ahead
/// of its frame, and a closed turn sends nothing more. The clock is paused, so only `advance`
/// moves it.
#[tokio::test(start_paused = true)]
async fn the_coalescer_sends_the_leading_edge_and_merges_the_rest() {
    let sent = Sent::default();
    let coalescer = UpdateCoalescer::default();
    let interval = UPDATE_FLUSH_INTERVAL;
    assert!(coalescer.park_update(
        snapshot("The"),
        "text_delta",
        "The ",
        1,
        interval,
        sent.send()
    ));
    assert_eq!(
        sent.take(),
        [(update("The", "text_delta", Some("The ")), 1)]
    );
    assert!(coalescer.park_update(
        snapshot("The Thames"),
        "text_delta",
        "Thames ",
        2,
        interval,
        sent.send(),
    ));
    assert!(coalescer.park_update(
        snapshot("The Thames flows"),
        "text_delta",
        "flows",
        3,
        interval,
        sent.send(),
    ));
    assert!(coalescer.flush_pending(interval, sent.send()));
    assert_eq!(sent.take(), [], "the trailing edge waits out the interval");
    tokio::time::advance(interval).await;
    assert!(coalescer.flush_pending(interval, sent.send()));
    assert_eq!(
        sent.take(),
        [(
            update("The Thames flows", "text_delta", Some("Thames flows")),
            3
        )]
    );
    // A kind switch replaces the run whole; the direct frame goes out after it.
    assert!(coalescer.park_update(
        snapshot("think"),
        "thinking_delta",
        "think",
        4,
        interval,
        sent.send(),
    ));
    assert!(coalescer.park_update(
        snapshot("answer"),
        "text_start",
        "",
        5,
        interval,
        sent.send()
    ));
    let direct = RefCell::new(false);
    coalescer.send_direct(sent.send(), || *direct.borrow_mut() = true);
    assert!(*direct.borrow());
    assert_eq!(sent.take(), [(update("answer", "text_start", None), 5)]);
    // A closed turn parks nothing and drops what was parked.
    assert!(coalescer.park_update(snapshot("a"), "text_delta", "a", 6, interval, sent.send()));
    coalescer.close();
    assert!(!coalescer.park_update(snapshot("b"), "text_delta", "b", 7, interval, sent.send()));
    assert!(!coalescer.flush_pending(interval, sent.send()));
    assert_eq!(coalescer.next_flush_at(interval), None);
    assert_eq!(sent.take(), []);
}

/// A flushed update carries the live context estimate grown from the last reading.
#[test]
fn a_flushed_update_carries_the_streaming_context_estimate() {
    let sent = Sent::default();
    let coalescer = UpdateCoalescer::default();
    coalescer.set_context(Some(ContextUsage::of(1_000, 10_000)));
    let message = json!({ "text": "abcdefgh" });
    assert!(coalescer.park_update(
        AssistantSnapshot::Wire(message.clone()),
        "text_delta",
        "abcdefgh",
        1,
        UPDATE_FLUSH_INTERVAL,
        sent.send(),
    ));
    let context = streaming_context_usage(Some(&ContextUsage::of(1_000, 10_000)), &message)
        .expect("an anchored estimate");
    let mut expected = update("abcdefgh", "text_delta", Some("abcdefgh"));
    expected["contextUsage"] = json!(context);
    assert_eq!(sent.take(), [(expected, 1)]);
}

#[test]
fn updates_route_by_their_stream_event() {
    let routes: Vec<UpdateRoute> = [
        Some(json!({ "type": "start" })),
        Some(json!({ "type": "text_delta", "delta": "hi" })),
        Some(json!({ "type": "thinking_end" })),
        None,
    ]
    .iter()
    .map(|event| update_route(event.as_ref()))
    .collect();
    assert_eq!(
        routes,
        [
            UpdateRoute::Direct,
            UpdateRoute::Park {
                kind: "text_delta".to_string(),
                delta: "hi".to_string(),
            },
            UpdateRoute::Direct,
            UpdateRoute::Park {
                kind: String::new(),
                delta: String::new(),
            },
        ]
    );
    assert_eq!(
        direct_update_event(
            json!({ "role": "assistant" }),
            Some(json!({ "type": "start" }))
        ),
        json!({
            "type": "message_start",
            "message": { "role": "assistant" },
            "assistantMessageEvent": { "type": "start" },
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_flush_interval_shortens_while_subscribers_keep_up_and_backs_off_on_lag_or_drops() {
    let pacer = FlushPacer::default();
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
