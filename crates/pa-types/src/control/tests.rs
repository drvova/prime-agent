//! The controller's decision sequences, replayed with explicit clocks and
//! compared whole.
use super::*;

fn config() -> ControllerConfig {
    ControllerConfig {
        initial_concurrency: 2,
        min_concurrency: 1,
        max_concurrency: 8,
        initial_rate_per_min: 60,
        min_rate_per_min: 1,
        max_rate_per_min: 1_000,
        rate_step_per_min: 10,
        decrease_keep_pct: 50,
        latency_keep_pct: 75,
        latency_rise_pct: 200,
        latency_rise_samples: 3,
        headroom_pct: 20,
        decrease_spacing_ms: 1_000,
        ceiling_ttl_ms: 60_000,
    }
}

fn observation(outcome: Outcome, latency_ms: Option<u64>) -> Observation {
    Observation {
        outcome,
        latency_ms,
        limits: LimitReport::default(),
    }
}

fn target(concurrency: u32, rate_per_min: u32, reason: AdjustReason) -> Target {
    Target {
        concurrency,
        rate_per_min,
        blocked_until_ms: None,
        reason,
    }
}

/// Headroom (successes at steady latency, no pushback) climbs one step per
/// window of successes at the current concurrency.
#[test]
fn headroom_scales_up_one_step_per_window_of_successes() {
    let mut controller = Controller::new(config(), 0);
    let success = observation(Outcome::Success, Some(100));
    let sequence: Vec<Target> = (1..=5)
        .map(|now| controller.observe(&success, now))
        .collect();
    assert_eq!(
        sequence,
        vec![
            target(2, 60, AdjustReason::Initial),
            target(3, 70, AdjustReason::Headroom),
            target(3, 70, AdjustReason::Headroom),
            target(3, 70, AdjustReason::Headroom),
            target(4, 80, AdjustReason::Headroom),
        ]
    );
}

/// A throttle halves concurrency and rate once per burst, honours the
/// server-requested wait, then recovers additively up to the ceiling it
/// learned and no further.
#[test]
fn a_throttle_cuts_multiplicatively_then_recovers_additively_to_the_ceiling() {
    let mut controller = Controller::restore(
        config(),
        LearnedState {
            concurrency: 4,
            rate_per_min: 100,
            concurrency_ceiling: None,
            rate_ceiling: None,
            ceiling_set_at_ms: None,
            reason: AdjustReason::Initial,
            updated_at_ms: 0,
        },
        0,
    );
    let throttled = Observation {
        outcome: Outcome::Throttled,
        latency_ms: None,
        limits: LimitReport {
            retry_after_ms: Some(2_000),
            ..LimitReport::default()
        },
    };
    let success = observation(Outcome::Success, Some(100));
    let mut sequence = vec![
        controller.observe(&throttled, 1_000),
        // The same burst: no second cut.
        controller.observe(&throttled, 1_500),
    ];
    for now in 4_000..4_007 {
        sequence.push(controller.observe(&success, now));
    }
    let blocked = Some(3_500);
    let at = |concurrency, rate_per_min, reason| Target {
        blocked_until_ms: blocked,
        ..target(concurrency, rate_per_min, reason)
    };
    assert_eq!(
        sequence,
        vec![
            Target {
                blocked_until_ms: Some(3_000),
                ..target(2, 50, AdjustReason::Throttled)
            },
            // The second rejection of the burst only extends the wait.
            at(2, 50, AdjustReason::Throttled),
            at(2, 50, AdjustReason::Throttled),
            at(3, 60, AdjustReason::Headroom),
            at(3, 60, AdjustReason::Headroom),
            at(3, 60, AdjustReason::Headroom),
            at(4, 70, AdjustReason::Headroom),
            at(4, 70, AdjustReason::Headroom),
            at(4, 70, AdjustReason::Headroom),
        ]
    );
    // Capped at the learned ceiling: further windows add rate below its
    // ceiling (100) but never concurrency above 4.
    for now in 4_007..4_100 {
        controller.observe(&success, now);
    }
    assert_eq!(controller.target(), at(4, 100, AdjustReason::Headroom));
}

/// A sustained rise of the latency sample over its baseline (the early sign
/// of throttling) lowers concurrency before any 429; a single slow sample
/// does not.
#[test]
fn a_sustained_latency_rise_lowers_concurrency() {
    let mut controller = Controller::restore(
        config(),
        LearnedState {
            concurrency: 8,
            rate_per_min: 100,
            concurrency_ceiling: None,
            rate_ceiling: None,
            ceiling_set_at_ms: None,
            reason: AdjustReason::Initial,
            updated_at_ms: 0,
        },
        0,
    );
    let samples = [100, 100, 500, 100, 500, 500, 500];
    let sequence: Vec<Target> = samples
        .iter()
        .enumerate()
        .map(|(now, sample)| {
            controller.observe(&observation(Outcome::Success, Some(*sample)), now as u64)
        })
        .collect();
    let restored = target(8, 100, AdjustReason::Restored);
    assert_eq!(
        sequence,
        vec![
            restored,
            restored,
            restored,
            restored,
            restored,
            restored,
            target(6, 100, AdjustReason::LatencyRise),
        ]
    );
}

/// A reported request limit sets the rate; a raised limit (a plan change)
/// is followed up; an aged-out pushback ceiling is dropped so the
/// controller probes again.
#[test]
fn reported_limits_are_followed_and_ceilings_decay() {
    let mut controller = Controller::new(config(), 0);
    let reporting = |limit| Observation {
        outcome: Outcome::Failed,
        latency_ms: None,
        limits: LimitReport {
            requests: LimitWindow {
                limit: Some(limit),
                remaining: Some(limit),
                reset_ms: Some(1_000),
            },
            ..LimitReport::default()
        },
    };
    let sequence = vec![
        controller.observe(&reporting(500), 1),
        controller.observe(&reporting(900), 2),
        controller.observe(&observation(Outcome::Overloaded, None), 3),
        controller.observe(&observation(Outcome::Failed, None), 70_000),
    ];
    assert_eq!(
        sequence,
        vec![
            target(2, 500, AdjustReason::LimitReported),
            target(2, 900, AdjustReason::LimitReported),
            target(1, 450, AdjustReason::Overloaded),
            target(1, 450, AdjustReason::Decay),
        ]
    );
    assert_eq!(
        controller.learned(),
        LearnedState {
            concurrency: 1,
            rate_per_min: 450,
            concurrency_ceiling: None,
            rate_ceiling: None,
            ceiling_set_at_ms: None,
            reason: AdjustReason::Decay,
            updated_at_ms: 70_000,
        }
    );
}

/// Deadline order is priority with aging: foreground first under
/// contention, but a background waiter that waited out its offset wins.
#[test]
fn service_order_prefers_foreground_and_ages_background() {
    let mut waiters = [
        (
            "background-old",
            service_deadline_ms(Priority::Background, 0),
        ),
        ("helper", service_deadline_ms(Priority::Helper, 14_000)),
        (
            "foreground",
            service_deadline_ms(Priority::Foreground, 16_000),
        ),
        (
            "background-new",
            service_deadline_ms(Priority::Background, 16_000),
        ),
    ];
    waiters.sort_by_key(|(_, deadline)| *deadline);
    assert_eq!(
        waiters.map(|(name, _)| name),
        ["background-old", "foreground", "helper", "background-new"]
    );
}

/// The bucket paces at its rate, pauses on an exhausted reported window,
/// and resumes after the reset.
#[test]
fn the_bucket_paces_and_pauses_until_a_reported_reset() {
    // 10 per minute: one token every 6s, a burst of one.
    let mut bucket = RateBucket::new(10, 0);
    let mut takes = vec![bucket.try_take(0), bucket.try_take(0)];
    bucket.sync(0, Some(5_000), 10);
    takes.extend([
        bucket.try_take(10),
        bucket.try_take(5_010),
        bucket.try_take(11_010),
    ]);
    assert_eq!(
        takes,
        vec![Ok(()), Err(6_000), Err(11_000), Err(6_000), Ok(())]
    );
}
