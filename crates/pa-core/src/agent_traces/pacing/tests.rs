use pa_types::control::{AdjustReason, Target};

use super::*;

fn answered(status: u16, retry_after: Option<&str>) -> TraceHttpResponse {
    TraceHttpResponse {
        status,
        body: String::new(),
        retry_after: retry_after.map(str::to_string),
    }
}

#[tokio::test(start_paused = true)]
async fn the_controller_widens_on_success_and_cuts_on_a_throttle_or_overload() {
    let gate = TraceRequestGate::new();
    let results = [
        Ok(answered(200, None)),
        Ok(answered(201, None)),
        Ok(answered(429, Some("30"))),
        Err(TraceHttpError::Transport("reset".into())),
        Ok(answered(503, None)),
    ];
    let mut targets = Vec::new();
    for result in &results {
        let permit = gate.before_request(None, None).await.expect("a permit");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        permit.finish(result);
        let Target {
            concurrency,
            rate_per_min,
            blocked_until_ms,
            reason,
        } = gate.target();
        targets.push((
            concurrency,
            rate_per_min,
            blocked_until_ms.is_some(),
            reason,
        ));
    }
    assert_eq!(
        targets,
        [
            (2, 5, false, AdjustReason::Initial),
            // Two successes at width two earn a step (the rate is at the platform's limit).
            (3, 5, false, AdjustReason::Headroom),
            // A 429 halves both and holds new requests for its Retry-After.
            (1, 2, true, AdjustReason::Throttled),
            // A transport failure says nothing about capacity.
            (1, 2, true, AdjustReason::Throttled),
            (1, 1, true, AdjustReason::Overloaded),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancel_ends_a_wait_without_a_permit() {
    let gate = TraceRequestGate::new();
    let held = gate
        .before_request(None, None)
        .await
        .expect("the first slot");
    let cancel = TraceUploadCancel::new();
    cancel.cancel();
    let waited = gate.before_request(Some(&cancel), None).await;
    assert_eq!(
        (waited.err(), gate.in_flight()),
        (Some(TraceHttpError::Cancelled), 1)
    );
    drop(held);
}

/// A runtime whose clock is behind the clock's first read (here: another paused runtime ten
/// minutes ahead read first) still reads a clock that moves, so a spent token refills there.
/// Red only when nothing else in the process read the clock first: run it on its own.
#[test]
fn the_clock_moves_on_a_runtime_behind_its_first_read() {
    let paused = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    };
    let behind = paused();
    paused().block_on(async {
        tokio::time::advance(std::time::Duration::from_mins(10)).await;
        tokio_clock_ms()
    });
    let moved = behind.block_on(async {
        let before = tokio_clock_ms();
        tokio::time::advance(std::time::Duration::from_secs(12)).await;
        tokio_clock_ms() - before
    });
    assert_eq!(moved, 12_000);
}
