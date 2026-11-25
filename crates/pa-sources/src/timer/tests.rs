use std::sync::Arc;

use pa_types::event_log::{NewEvent, Topic, Trust};
use serde_json::json;
use tokio::sync::watch;

use super::schedule::{parse_at, Cron, Schedule};
use super::{Clock, TimerOptions, TimerSource};
use crate::test_support::context;
use crate::{source_event, Source};

const MINUTE: u64 = 60_000;
/// 2026-11-25T09:30:00Z, a Wednesday.
const NOW: u64 = 1_795_599_000_000;

/// A clock the test moves by hand.
struct ManualClock(watch::Sender<u64>);

impl ManualClock {
    fn at(now: u64) -> Arc<Self> {
        Arc::new(ManualClock(watch::Sender::new(now)))
    }

    fn set(&self, now: u64) {
        self.0.send_replace(now);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        *self.0.borrow()
    }

    async fn sleep_until(&self, unix_ms: u64) {
        let mut now = self.0.subscribe();
        // The sender lives as long as the clock, which outlives its timers.
        let _ = now.wait_for(|now| *now >= unix_ms).await;
    }
}

fn options(options: serde_json::Value) -> TimerOptions {
    serde_json::from_value(options).expect("valid timer options")
}

fn fire(scheduled_for: u64, fired_at: u64) -> NewEvent {
    source_event(
        Topic::new("timer/tick").unwrap(),
        "tick",
        json!({ "scheduledFor": scheduled_for, "firedAt": fired_at }),
        Trust::Owner,
    )
}

#[test]
fn options_name_exactly_one_utc_schedule() {
    let refused = |value| serde_json::from_value::<TimerOptions>(value).is_err();
    assert!(refused(json!({})));
    assert!(refused(json!({ "every": "1m", "cron": "@hourly" })));
    assert!(refused(json!({ "every": "1m", "tz": "Europe/Paris" })));
    assert!(refused(json!({ "every": "1m", "jitter": "5s" })));
    assert_eq!(
        options(json!({ "every": "1h30m", "tz": "UTC" })).schedule,
        Schedule::Every {
            interval_ms: 90 * MINUTE
        }
    );
}

#[test]
fn every_refuses_sub_second_and_malformed_intervals() {
    for every in ["0s", "10", "5x", "m", ""] {
        assert!(
            serde_json::from_value::<TimerOptions>(json!({ "every": every })).is_err(),
            "{every:?}"
        );
    }
}

#[test]
fn at_reads_rfc3339_offsets_and_fractions() {
    assert_eq!(parse_at("2026-11-25T09:30:00Z"), Ok(NOW));
    assert_eq!(parse_at("2026-11-25T10:30:00.250+01:00"), Ok(NOW + 250));
    assert!(parse_at("2026-02-29T00:00:00Z").is_err());
    assert!(parse_at("2026-11-25 09:30:00").is_err());
}

#[test]
fn cron_finds_the_next_and_latest_matching_minute() {
    let weekday_mornings = Schedule::Cron(Cron::parse("*/15 9-10 * * 1-5").unwrap());
    // Wednesday 09:30 → 09:45; Friday 10:50 → Monday 09:00.
    assert_eq!(weekday_mornings.next_after(NOW), Some(NOW + 15 * MINUTE));
    let friday_late = NOW + 2 * 24 * 60 * MINUTE + 80 * MINUTE;
    let monday_nine = NOW + 5 * 24 * 60 * MINUTE - 30 * MINUTE;
    assert_eq!(weekday_mornings.next_after(friday_late), Some(monday_nine));
    // Missed from Wednesday 09:45 until Friday 10:50: the latest is Friday 10:45.
    assert_eq!(
        weekday_mornings.latest_due(NOW + 15 * MINUTE, friday_late),
        friday_late - 5 * MINUTE
    );
    assert!(Cron::parse("0 0 30 2 *").is_err());
    assert!(Cron::parse("60 * * * *").is_err());
    assert!(Cron::parse("*/0 * * * *").is_err());
    assert_eq!(
        Cron::parse("@daily").unwrap(),
        Cron::parse("0 0 * * *").unwrap()
    );
}

#[tokio::test]
async fn an_interval_fires_on_schedule_as_the_clock_moves() {
    let clock = ManualClock::at(NOW);
    let (context, mut published) = context(None);
    let timer = TimerSource::with_clock(Arc::clone(&clock))
        .start("tick", options(json!({ "every": "1m" })), context)
        .unwrap();
    clock.set(NOW + MINUTE);
    assert_eq!(published.next().await, fire(NOW + MINUTE, NOW + MINUTE));
    clock.set(NOW + 2 * MINUTE + 5);
    assert_eq!(
        published.next().await,
        fire(NOW + 2 * MINUTE, NOW + 2 * MINUTE + 5)
    );
    timer.stop().await.unwrap();
}

#[tokio::test]
async fn missed_fires_are_caught_up_once_then_the_schedule_continues() {
    let clock = ManualClock::at(NOW);
    // Last fired ten minutes and a half ago: ten fires were missed.
    let (context, mut published) = context(Some(NOW - 10 * MINUTE - MINUTE / 2));
    let timer = TimerSource::with_clock(Arc::clone(&clock))
        .start("tick", options(json!({ "every": "1m" })), context)
        .unwrap();
    assert_eq!(published.next().await, fire(NOW - MINUTE / 2, NOW));
    clock.set(NOW + MINUTE / 2);
    assert_eq!(
        published.next().await,
        fire(NOW + MINUTE / 2, NOW + MINUTE / 2)
    );
    timer.stop().await.unwrap();
    assert_eq!(published.drain(), Vec::new());
}

#[tokio::test]
async fn a_past_one_shot_fires_once_and_not_after_it_fired() {
    let clock = ManualClock::at(NOW);
    let at = json!({ "at": "2026-11-25T09:00:00Z" });
    let (first, mut published) = context(None);
    let timer = TimerSource::with_clock(Arc::clone(&clock))
        .start("tick", options(at.clone()), first)
        .unwrap();
    assert_eq!(published.next().await, fire(NOW - 30 * MINUTE, NOW));
    // Its work ended after the one fire.
    timer.stop().await.unwrap();

    let (again, mut published) = context(Some(NOW - 30 * MINUTE));
    let timer = TimerSource::with_clock(clock)
        .start("tick", options(at), again)
        .unwrap();
    timer.stop().await.unwrap();
    assert_eq!(published.drain(), Vec::new());
}

#[tokio::test]
async fn stop_ends_a_waiting_timer() {
    let (context, mut published) = context(None);
    let timer = TimerSource::new()
        .start("tick", options(json!({ "cron": "@yearly" })), context)
        .unwrap();
    tokio::time::timeout(crate::test_support::PATIENCE, timer.stop())
        .await
        .expect("a prompt stop")
        .unwrap();
    assert_eq!(published.drain(), Vec::new());
}

#[test]
fn a_name_is_one_topic_level() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (context, _published) = context(None);
    let started = TimerSource::new().start("a/b", options(json!({ "every": "1m" })), context);
    assert!(started.is_err());
}
