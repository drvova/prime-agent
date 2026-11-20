//! IO-free adaptive capacity control: a pure controller that turns
//! observations of a shared resource into a capacity target, plus the
//! pure pacing and fairness pieces that wrap it.
//!
//! The controller behaves like a serverless autoscaler: it keeps adjusting
//! concurrency and pacing for as long as the resource is used. Headroom
//! (no throttling, steady latency, a high reported `remaining`) raises the
//! target step by step toward the learned ceiling; throttling (429),
//! overload (503 / 529) and a sustained latency rise cut it
//! multiplicatively; a ceiling learned from a throttle decays after a
//! quiet period so the controller probes again. Its learned state
//! serializes, so a host can persist it and restore it after a restart.
//!
//! # Plugging in a resource
//!
//! Nothing here sleeps, spawns, locks, or reads a clock: every call takes
//! `now_ms` from the caller, so a host can drive the controller from any
//! runtime and tests replay exact decision sequences. A host wraps it:
//!
//! 1. Keep one [`Controller`] per independently limited resource (for
//!    model providers: provider, credential, model scope).
//! 2. Admit work while `in_flight < target.concurrency`, the
//!    [`RateBucket`] grants a token, and `now_ms >= target.blocked_until_ms`.
//!    Order waiters by [`service_deadline_ms`] so no [`Priority`] class
//!    starves.
//! 3. Report each finished unit of work as an [`Observation`]; apply the
//!    returned [`Target`] (resize the pool, re-rate the bucket).
//! 4. Persist [`Controller::learned`] off the critical path; seed the next
//!    process with [`Controller::restore`].
//!
//! The provider request limiter (`pa_ai::admission`) is the first user; a
//! warm pool, a worker pool, or a fan-out gate plug in the same way with
//! their own [`ControllerConfig`] and their own notion of "throttled".
//! The delegation depth bound ([`DepthController`]) is the same idea on a
//! coarser resource: how many levels of subagents a session may spawn.

mod depth;

pub use depth::{
    DelegationOutcome, DepthChange, DepthController, DEPTH_CEILING, DEPTH_FLOOR, DEPTH_INITIAL,
    DEPTH_RAISE_STREAK,
};

use serde::{Deserialize, Serialize};

/// Who is waiting for capacity. Under contention earlier classes are served
/// first; aging ([`service_deadline_ms`]) keeps later classes from starving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// The interactive turn the user is waiting on.
    Foreground,
    /// A subagent's turn.
    Subagent,
    /// Helper calls: titles, compaction summaries, side questions.
    Helper,
    /// Unattended work: scheduled jobs and other background turns.
    Background,
}

impl Priority {
    /// The head start a class gives up to [`Priority::Foreground`]: a waiter
    /// of this class is served ahead of a foreground waiter that arrived
    /// more than this many milliseconds after it.
    #[must_use]
    pub const fn aging_offset_ms(self) -> u64 {
        match self {
            Self::Foreground => 0,
            Self::Subagent => 2_000,
            Self::Helper => 5_000,
            Self::Background => 15_000,
        }
    }
}

/// The service order of a waiter: the smallest deadline is served first.
/// Deadline order is priority with aging: a background waiter that has
/// waited out its offset outranks a foreground waiter that just arrived.
#[must_use]
pub const fn service_deadline_ms(priority: Priority, enqueued_at_ms: u64) -> u64 {
    enqueued_at_ms.saturating_add(priority.aging_offset_ms())
}

/// How one unit of work ended, as far as capacity is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Served normally.
    Success,
    /// Rejected for exceeding a rate or quota (HTTP 429).
    Throttled,
    /// Rejected because the resource is overloaded (HTTP 503 / 529,
    /// `overloaded_error`).
    Overloaded,
    /// Failed for a reason that says nothing about capacity.
    Failed,
}

/// One limit window a resource reported about itself (for HTTP providers:
/// the `x-ratelimit-*` / `anthropic-ratelimit-*` header families).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitWindow {
    /// The window's capacity (per minute for request and token limits).
    pub limit: Option<u64>,
    /// What is left in the window.
    pub remaining: Option<u64>,
    /// Milliseconds until the window is full again.
    pub reset_ms: Option<u64>,
}

impl LimitWindow {
    /// Whether the window reported anything.
    #[must_use]
    pub const fn is_reported(&self) -> bool {
        self.limit.is_some() || self.remaining.is_some() || self.reset_ms.is_some()
    }
}

/// The limits a resource reported with one response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitReport {
    /// The request-count window.
    pub requests: LimitWindow,
    /// The token window (input + output, or the tighter of the split
    /// input/output windows).
    pub tokens: LimitWindow,
    /// A server-requested wait before the next request (`retry-after`).
    pub retry_after_ms: Option<u64>,
}

/// One finished unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub outcome: Outcome,
    /// Time to the first useful byte (for model streams: the first event).
    /// The latency-gradient decrease watches this sample.
    pub latency_ms: Option<u64>,
    pub limits: LimitReport,
}

/// Why the target last changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustReason {
    /// The configured starting point.
    Initial,
    /// Seeded from persisted learned state.
    Restored,
    /// Headroom observed: additive increase.
    Headroom,
    /// The resource reported its own limit.
    LimitReported,
    /// A throttle: multiplicative decrease.
    Throttled,
    /// An overload: multiplicative decrease.
    Overloaded,
    /// A sustained latency rise (the early throttling sign): decrease.
    LatencyRise,
    /// The learned ceiling aged out; probing again.
    Decay,
}

/// The tuning of one controlled resource. All percentages are integers so
/// decisions replay exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerConfig {
    pub initial_concurrency: u32,
    pub min_concurrency: u32,
    pub max_concurrency: u32,
    pub initial_rate_per_min: u32,
    pub min_rate_per_min: u32,
    pub max_rate_per_min: u32,
    /// The additive increase of the rate per headroom step.
    pub rate_step_per_min: u32,
    /// Multiplicative decrease on a throttle or overload (percent kept).
    pub decrease_keep_pct: u32,
    /// Decrease on a sustained latency rise (percent kept).
    pub latency_keep_pct: u32,
    /// A latency sample above `baseline * latency_rise_pct / 100` counts
    /// as a rise.
    pub latency_rise_pct: u32,
    /// Consecutive rising samples that trigger the latency decrease.
    pub latency_rise_samples: u32,
    /// A reported `remaining` at or above this share of `limit` is headroom.
    pub headroom_pct: u32,
    /// Throttles closer together than this cut only once (one burst of
    /// rejections is one signal).
    pub decrease_spacing_ms: u64,
    /// A learned ceiling older than this is dropped so the controller
    /// probes above it again.
    pub ceiling_ttl_ms: u64,
}

/// The capacity a host should apply now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub concurrency: u32,
    pub rate_per_min: u32,
    /// Admit nothing before this instant (a server-requested wait).
    pub blocked_until_ms: Option<u64>,
    pub reason: AdjustReason,
}

/// The persistable part of a controller: what it learned about the
/// resource. Restoring it lets a new process start where the last left off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnedState {
    pub concurrency: u32,
    pub rate_per_min: u32,
    /// The concurrency at which the resource last pushed back.
    pub concurrency_ceiling: Option<u32>,
    /// The rate limit the resource reported, or the rate it last pushed
    /// back at.
    pub rate_ceiling: Option<u32>,
    /// When the ceilings were last set; they age out after
    /// [`ControllerConfig::ceiling_ttl_ms`].
    pub ceiling_set_at_ms: Option<u64>,
    pub reason: AdjustReason,
    pub updated_at_ms: u64,
}

/// The adaptive controller for one resource. See the module docs for how a
/// host drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Controller {
    config: ControllerConfig,
    learned: LearnedState,
    blocked_until_ms: Option<u64>,
    last_decrease_at_ms: Option<u64>,
    successes_since_change: u32,
    latency_baseline_ms: Option<u64>,
    latency_rise_streak: u32,
}

impl Controller {
    /// A controller at the configured starting point.
    #[must_use]
    pub fn new(config: ControllerConfig, now_ms: u64) -> Self {
        Self::from_learned(
            config,
            LearnedState {
                concurrency: config.initial_concurrency,
                rate_per_min: config.initial_rate_per_min,
                concurrency_ceiling: None,
                rate_ceiling: None,
                ceiling_set_at_ms: None,
                reason: AdjustReason::Initial,
                updated_at_ms: now_ms,
            },
        )
    }

    /// A controller seeded from persisted state, clamped to `config`; an
    /// aged-out ceiling is dropped at the next observation.
    #[must_use]
    pub fn restore(config: ControllerConfig, learned: LearnedState, now_ms: u64) -> Self {
        Self::from_learned(
            config,
            LearnedState {
                concurrency: learned
                    .concurrency
                    .clamp(config.min_concurrency, config.max_concurrency),
                rate_per_min: learned
                    .rate_per_min
                    .clamp(config.min_rate_per_min, config.max_rate_per_min),
                reason: AdjustReason::Restored,
                updated_at_ms: now_ms,
                ..learned
            },
        )
    }

    fn from_learned(config: ControllerConfig, learned: LearnedState) -> Self {
        Self {
            config,
            learned,
            blocked_until_ms: None,
            last_decrease_at_ms: None,
            successes_since_change: 0,
            latency_baseline_ms: None,
            latency_rise_streak: 0,
        }
    }

    /// The target to apply now.
    #[must_use]
    pub fn target(&self) -> Target {
        Target {
            concurrency: self.learned.concurrency,
            rate_per_min: self.learned.rate_per_min,
            blocked_until_ms: self.blocked_until_ms,
            reason: self.learned.reason,
        }
    }

    /// What the controller learned, for persistence.
    #[must_use]
    pub fn learned(&self) -> LearnedState {
        self.learned
    }

    /// Fold one observation in and return the new target.
    pub fn observe(&mut self, observation: &Observation, now_ms: u64) -> Target {
        self.age_ceilings(now_ms);
        let limits = &observation.limits;
        if let Some(wait) = limits.retry_after_ms {
            let until = now_ms.saturating_add(wait);
            self.blocked_until_ms = Some(self.blocked_until_ms.map_or(until, |at| at.max(until)));
        }
        if let Some(limit) = limits.requests.limit {
            self.follow_reported_limit(limit, now_ms);
        }
        match observation.outcome {
            Outcome::Throttled => self.decrease(AdjustReason::Throttled, now_ms),
            Outcome::Overloaded => self.decrease(AdjustReason::Overloaded, now_ms),
            Outcome::Success => self.on_success(observation, now_ms),
            Outcome::Failed => {}
        }
        self.target()
    }

    /// The resource stated its request limit: the rate ceiling follows it
    /// up or down (a plan change). The rate moves to it, unless the
    /// resource pushed back recently: then it only clamps, and recovery
    /// climbs back additively.
    fn follow_reported_limit(&mut self, limit: u64, now_ms: u64) {
        let limit = u32::try_from(limit)
            .unwrap_or(u32::MAX)
            .clamp(self.config.min_rate_per_min, self.config.max_rate_per_min);
        let rate = if self.learned.ceiling_set_at_ms.is_some() {
            self.learned.rate_per_min.min(limit)
        } else {
            limit
        };
        if self.learned.rate_ceiling == Some(limit) && self.learned.rate_per_min == rate {
            return;
        }
        self.learned.rate_ceiling = Some(limit);
        self.learned.rate_per_min = rate;
        self.learned.reason = AdjustReason::LimitReported;
        self.learned.updated_at_ms = now_ms;
    }

    fn decrease(&mut self, reason: AdjustReason, now_ms: u64) {
        self.successes_since_change = 0;
        self.latency_rise_streak = 0;
        if self
            .last_decrease_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < self.config.decrease_spacing_ms)
        {
            return;
        }
        self.last_decrease_at_ms = Some(now_ms);
        let keep = self.config.decrease_keep_pct;
        let learned = &mut self.learned;
        learned.concurrency_ceiling = Some(learned.concurrency);
        learned.rate_ceiling = Some(
            learned
                .rate_ceiling
                .map_or(learned.rate_per_min, |ceiling| {
                    ceiling.min(learned.rate_per_min)
                }),
        );
        learned.ceiling_set_at_ms = Some(now_ms);
        learned.concurrency = scale(learned.concurrency, keep).max(self.config.min_concurrency);
        learned.rate_per_min = scale(learned.rate_per_min, keep).max(self.config.min_rate_per_min);
        learned.reason = reason;
        learned.updated_at_ms = now_ms;
    }

    fn on_success(&mut self, observation: &Observation, now_ms: u64) {
        if let Some(sample) = observation.latency_ms {
            if self.latency_rose(sample) {
                self.latency_rise_streak = 0;
                self.successes_since_change = 0;
                let learned = &mut self.learned;
                let lowered = scale(learned.concurrency, self.config.latency_keep_pct)
                    .max(self.config.min_concurrency);
                if lowered < learned.concurrency {
                    learned.concurrency_ceiling = Some(learned.concurrency);
                    learned.ceiling_set_at_ms = Some(now_ms);
                    learned.concurrency = lowered;
                    learned.reason = AdjustReason::LatencyRise;
                    learned.updated_at_ms = now_ms;
                }
                return;
            }
        }
        if !has_headroom(&observation.limits, self.config.headroom_pct) {
            return;
        }
        self.successes_since_change += 1;
        // One full window of successes at the current concurrency earns a step.
        if self.successes_since_change < self.learned.concurrency {
            return;
        }
        self.successes_since_change = 0;
        let learned = &mut self.learned;
        let concurrency_cap = learned
            .concurrency_ceiling
            .map_or(self.config.max_concurrency, |ceiling| {
                ceiling.min(self.config.max_concurrency)
            });
        let rate_cap = learned
            .rate_ceiling
            .map_or(self.config.max_rate_per_min, |ceiling| {
                ceiling.min(self.config.max_rate_per_min)
            });
        let concurrency = (learned.concurrency + 1)
            .min(concurrency_cap)
            .max(learned.concurrency);
        let rate = learned
            .rate_per_min
            .saturating_add(self.config.rate_step_per_min)
            .min(rate_cap)
            .max(learned.rate_per_min);
        if concurrency != learned.concurrency || rate != learned.rate_per_min {
            learned.concurrency = concurrency;
            learned.rate_per_min = rate;
            learned.reason = AdjustReason::Headroom;
            learned.updated_at_ms = now_ms;
        }
    }

    /// Track the latency baseline (an EWMA over non-rising samples) and say
    /// whether this sample completes a sustained rise.
    fn latency_rose(&mut self, sample: u64) -> bool {
        let Some(baseline) = self.latency_baseline_ms else {
            self.latency_baseline_ms = Some(sample);
            return false;
        };
        if sample.saturating_mul(100)
            > baseline.saturating_mul(u64::from(self.config.latency_rise_pct))
        {
            self.latency_rise_streak += 1;
            return self.latency_rise_streak >= self.config.latency_rise_samples;
        }
        self.latency_rise_streak = 0;
        self.latency_baseline_ms = Some((baseline.saturating_mul(7) + sample) / 8);
        false
    }

    /// Drop ceilings learned longer ago than the TTL: limits change, so the
    /// controller probes above them again. A reported request limit is
    /// re-learned from the next response that carries it.
    fn age_ceilings(&mut self, now_ms: u64) {
        let Some(set_at) = self.learned.ceiling_set_at_ms else {
            return;
        };
        if now_ms.saturating_sub(set_at) < self.config.ceiling_ttl_ms {
            return;
        }
        let learned = &mut self.learned;
        learned.concurrency_ceiling = None;
        learned.rate_ceiling = None;
        learned.ceiling_set_at_ms = None;
        learned.reason = AdjustReason::Decay;
        learned.updated_at_ms = now_ms;
    }
}

fn scale(value: u32, keep_pct: u32) -> u32 {
    u32::try_from(u64::from(value) * u64::from(keep_pct) / 100).unwrap_or(u32::MAX)
}

/// Whether a response left room: no reported window is below `headroom_pct`.
fn has_headroom(limits: &LimitReport, headroom_pct: u32) -> bool {
    [limits.requests, limits.tokens]
        .iter()
        .all(|window| match (window.limit, window.remaining) {
            (Some(limit), Some(remaining)) if limit > 0 => {
                remaining.saturating_mul(100) >= limit.saturating_mul(u64::from(headroom_pct))
            }
            _ => true,
        })
}

/// A token bucket for request pacing, refilled continuously at the target
/// rate and synced to a reported `remaining` / `reset`. Pure: every call
/// takes `now_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateBucket {
    /// Tokens in thousandths, so slow rates refill smoothly.
    milli_tokens: u64,
    rate_per_min: u32,
    refilled_at_ms: u64,
    /// No refill before this instant (a reported window that ran out).
    paused_until_ms: Option<u64>,
}

impl RateBucket {
    /// A full bucket at `rate_per_min`.
    #[must_use]
    pub fn new(rate_per_min: u32, now_ms: u64) -> Self {
        Self {
            milli_tokens: Self::capacity_milli(rate_per_min),
            rate_per_min,
            refilled_at_ms: now_ms,
            paused_until_ms: None,
        }
    }

    /// Burst capacity: a tenth of a minute's budget, at least one request.
    fn capacity_milli(rate_per_min: u32) -> u64 {
        u64::from((rate_per_min / 10).max(1)) * 1000
    }

    fn refill(&mut self, now_ms: u64) {
        let from = self
            .paused_until_ms
            .map_or(self.refilled_at_ms, |until| until.max(self.refilled_at_ms));
        if now_ms > from {
            let earned = (now_ms - from).saturating_mul(u64::from(self.rate_per_min)) / 60;
            self.milli_tokens = self
                .milli_tokens
                .saturating_add(earned)
                .min(Self::capacity_milli(self.rate_per_min));
            self.paused_until_ms = None;
        }
        self.refilled_at_ms = now_ms.max(self.refilled_at_ms);
    }

    /// Move to a new rate (the controller's target), keeping earned tokens.
    pub fn set_rate(&mut self, rate_per_min: u32, now_ms: u64) {
        self.refill(now_ms);
        self.rate_per_min = rate_per_min;
        self.milli_tokens = self.milli_tokens.min(Self::capacity_milli(rate_per_min));
    }

    /// Sync to a reported window: never hold more than `available` tokens
    /// (what the resource says is left, minus work already admitted), and
    /// when the window ran out, refill nothing until it resets.
    pub fn sync(&mut self, available: u64, reset_ms: Option<u64>, now_ms: u64) {
        self.refill(now_ms);
        self.milli_tokens = self.milli_tokens.min(available.saturating_mul(1000));
        if available == 0 {
            if let Some(reset) = reset_ms {
                self.paused_until_ms = Some(now_ms.saturating_add(reset));
            }
        }
    }

    /// Take one token, or say how long until one is available.
    ///
    /// # Errors
    ///
    /// Returns the wait in milliseconds when the bucket is empty.
    pub fn try_take(&mut self, now_ms: u64) -> Result<(), u64> {
        self.refill(now_ms);
        if self.milli_tokens >= 1000 {
            self.milli_tokens -= 1000;
            return Ok(());
        }
        let missing = 1000 - self.milli_tokens;
        let rate = u64::from(self.rate_per_min.max(1));
        let refill_wait = (missing * 60).div_ceil(rate);
        let paused = self
            .paused_until_ms
            .map_or(0, |until| until.saturating_sub(now_ms));
        Err(paused + refill_wait)
    }
}

#[cfg(test)]
mod tests;
