//! The timer source: fires `timer/<name>` every interval, on a cron
//! expression, or once at a time, with Owner trust and the payload
//! `{ "scheduledFor": <unix ms>, "firedAt": <unix ms> }`.
//!
//! Options are exactly one of `every` (`30s`, `10m`, `1h30m`, `2d`), `cron`
//! (five fields or `@hourly`-style, read in UTC) and `at` (RFC 3339), plus an
//! optional `tz` that is only `UTC`: zones are not supported, so cron fields
//! are always UTC.
//!
//! **Catch-up.** A start resumes from the `scheduledFor` of the last fire the
//! runtime holds ([`StartContext::last_fired_ms`]). Fires missed while the
//! timer was stopped (or while the machine slept) are not replayed: when one
//! or more were missed, the timer fires once at once, with `scheduledFor`
//! the latest missed time, and then continues on schedule. An `every` timer
//! that has never fired first fires one interval after its start; a cron
//! timer that has never fired waits for its next match; an `at` timer that
//! has never fired fires at its time, or at once when that time has passed,
//! and never again.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pa_types::event_log::Trust;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{source_event, source_topic, spawn, Source, SourceHandle, StartContext};

mod schedule;

use schedule::{Cron, Schedule};

/// The longest a [`SystemClock`] sleeps before it reads the time again, so
/// a wall clock that jumped (or a machine that slept) is noticed.
const SYSTEM_CLOCK_RECHECK: Duration = Duration::from_mins(1);

/// The time a timer reads and waits on.
///
/// `now_ms` is the current time in Unix milliseconds. `sleep_until` returns
/// once `now_ms` has reached `unix_ms` (at once when it already has); a
/// timer drops the future when it is stopped, so an implementation must not
/// rely on running to completion.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;

    fn sleep_until(&self, unix_ms: u64) -> impl Future<Output = ()> + Send;
}

/// The system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    async fn sleep_until(&self, unix_ms: u64) {
        loop {
            let now = self.now_ms();
            if now >= unix_ms {
                return;
            }
            tokio::time::sleep(Duration::from_millis(unix_ms - now).min(SYSTEM_CLOCK_RECHECK))
                .await;
        }
    }
}

/// A timer's options: when it fires.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "TimerFields")]
pub struct TimerOptions {
    schedule: Schedule,
}

/// The options as written, before exactly one schedule is picked out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimerFields {
    every: Option<String>,
    cron: Option<String>,
    at: Option<String>,
    tz: Option<String>,
}

impl TryFrom<TimerFields> for TimerOptions {
    type Error = String;

    fn try_from(fields: TimerFields) -> Result<Self, Self::Error> {
        if let Some(zone) = fields.tz.filter(|zone| zone != "UTC") {
            return Err(format!(
                "a timer's `tz` is only \"UTC\" (time zones are not supported), got {zone:?}"
            ));
        }
        let schedule = match (fields.every, fields.cron, fields.at) {
            (Some(every), None, None) => Schedule::Every {
                interval_ms: schedule::parse_every(&every)?,
            },
            (None, Some(cron), None) => Schedule::Cron(Cron::parse(&cron)?),
            (None, None, Some(at)) => Schedule::At {
                unix_ms: schedule::parse_at(&at)?,
            },
            (None, None, None)
            | (Some(_), Some(_), _)
            | (Some(_), None, Some(_))
            | (None, Some(_), Some(_)) => {
                return Err("a timer has exactly one of `every`, `cron` and `at`".to_string());
            }
        };
        Ok(TimerOptions { schedule })
    }
}

/// Starts timers on a [`Clock`].
#[derive(Debug, Clone, Default)]
pub struct TimerSource<C = SystemClock> {
    clock: Arc<C>,
}

impl TimerSource<SystemClock> {
    /// Timers on the system clock.
    #[must_use]
    pub fn new() -> Self {
        TimerSource::default()
    }
}

impl<C: Clock> TimerSource<C> {
    /// Timers on `clock`.
    #[must_use]
    pub fn with_clock(clock: Arc<C>) -> Self {
        TimerSource { clock }
    }
}

impl<C: Clock> Source for TimerSource<C> {
    type Options = TimerOptions;

    fn options_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "timer",
            "description": "Fires timer/<name> on a schedule, with Owner trust. Exactly one of every, cron and at.",
            "type": "object",
            "properties": {
                "every": {
                    "type": "string",
                    "description": "An interval of <count><unit> parts, units s, m, h, d: 30s, 10m, 1h30m. At least 1s.",
                    "pattern": "^([0-9]+[smhd])+$"
                },
                "cron": {
                    "type": "string",
                    "description": "Five fields (minute hour day-of-month month day-of-week) in UTC, or @hourly, @daily, @weekly, @monthly, @yearly."
                },
                "at": {
                    "type": "string",
                    "format": "date-time",
                    "description": "One RFC 3339 time to fire at, once."
                },
                "tz": {
                    "const": "UTC",
                    "description": "Only UTC: time zones are not supported."
                }
            },
            "oneOf": [
                { "required": ["every"] },
                { "required": ["cron"] },
                { "required": ["at"] }
            ],
            "additionalProperties": false
        })
    }

    fn start(
        &self,
        name: &str,
        options: TimerOptions,
        context: StartContext,
    ) -> anyhow::Result<SourceHandle> {
        let topic = source_topic(name, format!("timer/{name}"))?;
        let schedule = options.schedule;
        let clock = Arc::clone(&self.clock);
        let mut next = match (context.last_fired_ms, &schedule) {
            (Some(last_fired), _) => schedule.next_after(last_fired),
            (None, Schedule::At { unix_ms }) => Some(*unix_ms),
            (None, Schedule::Every { .. } | Schedule::Cron(_)) => {
                schedule.next_after(clock.now_ms())
            }
        };
        let name = name.to_string();
        spawn(None, move |cancel| async move {
            while let Some(due) = next {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    () = clock.sleep_until(due) => {}
                }
                let fired_at = clock.now_ms();
                let scheduled_for = schedule.latest_due(due, fired_at);
                let payload = json!({ "scheduledFor": scheduled_for, "firedAt": fired_at });
                context.publisher.publish(source_event(
                    topic.clone(),
                    &name,
                    payload,
                    Trust::Owner,
                ))?;
                next = schedule.next_after(scheduled_for);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
