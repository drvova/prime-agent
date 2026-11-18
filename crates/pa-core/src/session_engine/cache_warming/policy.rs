//! The cache-warming economics: what a waiting session's next round is
//! likely to be, what a refresh costs against the write it saves, and when
//! the warmer stops. Pure functions over prices, token counts and clocks so
//! the rule is testable without a provider.
//!
//! The rule is adapted from Nodecode's `cache-warming.lisp` (MIT): refresh at
//! nine tenths of the entry's lifetime, only when the expected saving
//! (the write the next round avoids, weighted by the chance that round comes
//! before the entry expires, less the refresh's own cache read and output
//! token) is at least five cents, and stop after an age cap.

use std::time::Duration;

use pa_agent::types::{CacheRetention, Usage, UsageCost};
use serde_json::Value;

use crate::settings::{CacheRetentionSetting, CacheWarmingSetting};

/// The env override for the prompt-cache lifetime (a wire identifier the
/// providers read themselves; kept under its historical name).
pub(crate) const CACHE_RETENTION_ENV: &str = "PI_CACHE_RETENTION";

/// How far into the cache entry's lifetime a refresh is sent.
const REFRESH_FRACTION: f64 = 0.9;
/// The least time a refresh leaves before the entry would expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(10);
/// Dollars a refresh must be expected to save to be sent.
pub(crate) const MINIMUM_SAVING_USD: f64 = 0.05;
/// Refreshes one armed round may send, whatever its phase allows by age.
pub(crate) const MAX_REFRESHES: u32 = 12;
/// Anthropic's default cache-entry lifetime.
const SHORT_TTL: Duration = Duration::from_secs(300);
/// Anthropic's extended (`ttl: "1h"`) cache-entry lifetime.
const LONG_TTL: Duration = Duration::from_secs(3600);
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// The cache retention a request asks the provider for: the
/// `PI_CACHE_RETENTION` env override wins (the provider reads it itself, so
/// the request leaves the option unset), then the `cacheRetention` setting,
/// whose default is the provider's short lifetime.
pub(crate) fn request_cache_retention(
    env_override: Option<&str>,
    setting: CacheRetentionSetting,
) -> Option<CacheRetention> {
    if env_override.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    Some(match setting {
        CacheRetentionSetting::Short => CacheRetention::Short,
        CacheRetentionSetting::Long => CacheRetention::Long,
    })
}

/// What a sent request's payload says about repeating it as a refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WarmShape {
    /// The shortest lifetime among the request's cache markers.
    pub ttl: Duration,
}

/// Read a request payload's cache markers (Anthropic `cache_control`,
/// Bedrock `cachePoint`). `None` when the request placed none, or when it
/// thinks on a token budget: such a request cannot be repeated with one
/// output token (the budget must stay under `max_tokens`, and the provider
/// would raise `max_tokens` by the whole budget). Markers only ever sit on
/// the system blocks, the last tool and the last message, so only those are
/// scanned, never the whole history.
pub(crate) fn warm_shape(payload: &Value) -> Option<WarmShape> {
    let budget_thinking = [
        &payload["thinking"],
        &payload["additionalModelRequestFields"]["thinking"],
    ]
    .into_iter()
    .any(|thinking| thinking["type"] == "enabled");
    if budget_thinking {
        return None;
    }
    let mut ttls = Vec::new();
    let scanned = [
        &payload["system"],
        &payload["tools"],
        payload["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .unwrap_or(&Value::Null),
    ];
    for value in scanned {
        collect_marker_ttls(value, &mut ttls);
    }
    ttls.into_iter().min().map(|ttl| WarmShape { ttl })
}

fn collect_marker_ttls(value: &Value, ttls: &mut Vec<Duration>) {
    match value {
        Value::Object(map) => {
            for key in ["cache_control", "cachePoint"] {
                if let Some(marker) = map.get(key) {
                    ttls.push(if marker["ttl"] == "1h" {
                        LONG_TTL
                    } else {
                        SHORT_TTL
                    });
                }
            }
            for child in map.values() {
                collect_marker_ttls(child, ttls);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_marker_ttls(item, ttls);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// How long after the start of the request that last read the entry (a read
/// refreshes the entry's lifetime from the start of the request) the next
/// refresh is due.
pub(crate) fn refresh_delay(ttl: Duration, since_last_read_start: Duration) -> Duration {
    let due = ttl
        .mul_f64(REFRESH_FRACTION)
        .min(ttl.saturating_sub(REFRESH_MARGIN));
    due.saturating_sub(since_last_read_start)
}

/// Where a session that armed a warmer stands when the refresh comes due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarmPhase {
    /// The round ended in tool calls and no later round started: a tool is
    /// still running, and its result goes out as the next round.
    ToolRunning,
    /// The turn settled, but a background `bash()` job or a subagent of
    /// this session is still running; its completion wakes the session.
    AwaitingWork,
    /// A subagent's turn settled and it waits on its parent's next message.
    AwaitingParent,
    /// Nothing is running: the next round waits on a person.
    Idle,
}

impl WarmPhase {
    /// The chance that the next round comes before the refreshed entry
    /// expires. Nodecode uses 1.0 while a tool runs and 0.15 when idle;
    /// work in flight wakes the session as surely as a running tool, and a
    /// subagent waiting on its parent is usually re-tasked or collected.
    fn continuation(self) -> f64 {
        match self {
            WarmPhase::ToolRunning | WarmPhase::AwaitingWork => 1.0,
            WarmPhase::AwaitingParent => 0.5,
            WarmPhase::Idle => 0.15,
        }
    }

    /// How long after the round that armed it a warmer keeps refreshing.
    fn max_age(self) -> Duration {
        match self {
            WarmPhase::ToolRunning | WarmPhase::AwaitingWork => Duration::from_hours(1),
            WarmPhase::AwaitingParent | WarmPhase::Idle => Duration::from_mins(30),
        }
    }
}

/// Per-million-token prices for one model, as the cache is billed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CachePrices {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

impl CachePrices {
    /// Prices for a cache entry living `ttl`: the catalog's read and write
    /// prices, with Anthropic's multipliers where the catalog leaves them out
    /// and the one-hour write rate for a one-hour entry.
    pub(crate) fn for_ttl(cost: &UsageCost, ttl: Duration) -> Self {
        let duration = if ttl >= LONG_TTL { "1h" } else { "5m" };
        let (default_read, default_write) = pa_ai::get_anthropic_cache_costs(cost.input, duration);
        let cache_write = if duration == "1h" || cost.cache_write <= 0.0 {
            default_write
        } else {
            cost.cache_write
        };
        Self {
            input: cost.input,
            output: cost.output,
            cache_read: if cost.cache_read > 0.0 {
                cost.cache_read
            } else {
                default_read
            },
            cache_write,
        }
    }

    /// The dollars one request saved through the cache: its cache reads
    /// against the same tokens uncached, less what its cache writes cost
    /// above uncached input.
    // Token counts sit far below f64's 2^53 exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn request_savings(&self, usage: &Usage) -> f64 {
        (usage.cache_read as f64 * (self.input - self.cache_read)
            - usage.cache_write as f64 * (self.cache_write - self.input))
            / TOKENS_PER_MILLION
    }

    /// The dollars one refresh of a `prompt_tokens` prompt is expected to
    /// save in `phase`: the write the next round avoids, weighted by the
    /// chance it comes in time, less the refresh's own read and one output
    /// token.
    // Token counts sit far below f64's 2^53 exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn expected_saving(&self, prompt_tokens: u64, phase: WarmPhase) -> f64 {
        let tokens = prompt_tokens as f64;
        let avoided_write = (self.cache_write - self.cache_read).max(0.0) * tokens;
        let refresh = self.cache_read * tokens + self.output;
        (phase.continuation() * avoided_write - refresh) / TOKENS_PER_MILLION
    }
}

/// Why an armed warmer stops instead of refreshing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// `cacheWarming: off`.
    Disabled,
    /// `cacheWarming: active` never warms an idle session.
    IdleNotWarmed,
    /// The phase's age cap or the refresh cap was reached.
    Capped,
    /// The expected saving is under [`MINIMUM_SAVING_USD`].
    Unprofitable,
}

/// What a due warmer does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WarmVerdict {
    Refresh { expected_saving_usd: f64 },
    Stop(StopReason),
}

/// The state a due warmer is judged on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DueWarmer {
    pub mode: CacheWarmingSetting,
    pub phase: WarmPhase,
    /// Since the real round that armed the warmer.
    pub age: Duration,
    pub refreshes: u32,
    pub prompt_tokens: u64,
    pub prices: CachePrices,
}

pub(crate) fn verdict(due: &DueWarmer) -> WarmVerdict {
    match (due.mode, due.phase) {
        (CacheWarmingSetting::Off, _) => return WarmVerdict::Stop(StopReason::Disabled),
        (CacheWarmingSetting::Active, WarmPhase::Idle) => {
            return WarmVerdict::Stop(StopReason::IdleNotWarmed);
        }
        (
            CacheWarmingSetting::Active,
            WarmPhase::ToolRunning | WarmPhase::AwaitingWork | WarmPhase::AwaitingParent,
        )
        | (CacheWarmingSetting::Idle, _) => {}
    }
    if due.age > due.phase.max_age() || due.refreshes >= MAX_REFRESHES {
        return WarmVerdict::Stop(StopReason::Capped);
    }
    let expected_saving_usd = due.prices.expected_saving(due.prompt_tokens, due.phase);
    if expected_saving_usd < MINIMUM_SAVING_USD {
        return WarmVerdict::Stop(StopReason::Unprofitable);
    }
    WarmVerdict::Refresh {
        expected_saving_usd,
    }
}
