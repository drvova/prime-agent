//! The `/usage` page's wire vocabulary: the provider quota windows (with their reset times) and
//! the local spend history the daemon's `get_usage` request serves and the TUI renders. Every
//! number here came from a provider response or the local session files; nothing is estimated.

use serde::{Deserialize, Serialize};

/// Which half of the page one `get_usage` request asks for. The halves travel separately so a
/// slow provider endpoint never holds back the local spend history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSection {
    /// The provider quota windows ([`QuotaReport`]).
    Quotas,
    /// The local spend history ([`SpendHistory`]).
    Spend,
}

/// The `get_usage` `quotas` response: one card per provider the daemon holds a credential for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaReport {
    pub providers: Vec<ProviderQuota>,
    /// The daemon's live request pacing per provider and model (the adaptive
    /// admission controller's state); empty without a daemon or before any
    /// request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pacing: Vec<ProviderPacing>,
}

/// One provider/model's request pacing as the admission controller holds it
/// now: read-only, learned from responses, never configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPacing {
    pub provider: String,
    pub model: String,
    /// Requests allowed in flight now.
    pub concurrency: u32,
    /// Requests per minute allowed now.
    pub rate_per_min: u32,
    /// The concurrency the provider last pushed back at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency_ceiling: Option<u32>,
    /// The reported (or pushed-back) request rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_ceiling: Option<u32>,
    pub in_flight: u32,
    /// Why the controller last changed the pacing.
    pub reason: crate::control::AdjustReason,
}

/// Where a card's numbers came from; the page labels every card with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// The provider's own usage endpoint.
    Provider,
    /// The rate-limit error a model response carried (status and `retryAfterMs`).
    ObservedResponse,
}

/// One provider's quota card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderQuota {
    /// The provider id (`anthropic`, `openai-codex`, `github-copilot`, ...).
    pub provider: String,
    /// The plan the provider reports (`pro`, `plus`, `individual`), when it reports one. Never an
    /// email or account id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub source: QuotaSource,
    /// When the numbers were fetched (or the response was observed), epoch ms.
    pub fetched_at_ms: i64,
    pub state: QuotaState,
}

/// A card's body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum QuotaState {
    /// The provider reported these limit windows (possibly none: no limits apply).
    Windows { windows: Vec<QuotaWindow> },
    /// A model response was rate limited (HTTP 429): the provider asked for this wait.
    RateLimited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at_ms: Option<i64>,
    },
    /// The fetch failed; the card shows the reason instead of numbers.
    Failed { error: String },
}

/// The unit a window's `used` / `limit` count in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaUnit {
    /// `used` is a percentage of a limit the provider does not disclose (`limit` is 100).
    Percent,
    /// Counted requests (Copilot premium requests).
    Requests,
}

/// One limit window: how much of it is used and when it resets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    /// The window's name (`5-hour`, `Weekly`, `Weekly (Opus)`, `Premium requests`).
    pub label: String,
    pub used: f64,
    /// `None` when the window has no cap (an unlimited plan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    pub unit: QuotaUnit,
    /// When the window resets, epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

impl QuotaWindow {
    /// The used fraction (`0..`, above 1 past the cap); `None` without a positive limit.
    #[must_use]
    pub fn used_fraction(&self) -> Option<f64> {
        self.limit
            .filter(|limit| *limit > 0.0)
            .map(|limit| self.used / limit)
    }
}

/// Token and cost totals over some span of assistant calls (a missing field reads as zero).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpendTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// The provider-billed cost the session files recorded, in USD.
    pub cost: f64,
    /// Billed model calls.
    pub requests: u64,
}

impl SpendTotals {
    /// Every token the calls billed (input, output, cache reads and writes).
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

/// One local calendar day of spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySpend {
    /// The local date, `YYYY-MM-DD`.
    pub date: String,
    pub totals: SpendTotals,
}

/// One provider/model's spend over the page's spans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpend {
    pub provider: String,
    pub model: String,
    pub today: SpendTotals,
    pub last_7_days: SpendTotals,
    pub last_30_days: SpendTotals,
    pub all_time: SpendTotals,
}

/// The latest rate-limited model response seen for one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedRateLimit {
    pub provider: String,
    pub observed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

/// The `get_usage` `spend` response: the local session files' billed calls, aggregated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendHistory {
    pub today: SpendTotals,
    pub last_7_days: SpendTotals,
    pub last_30_days: SpendTotals,
    pub all_time: SpendTotals,
    /// The last 30 local days, oldest first, today last (days without calls included).
    pub daily: Vec<DailySpend>,
    /// Per provider/model, by all-time cost, highest first.
    pub models: Vec<ModelSpend>,
    /// The latest rate-limited response per provider over the last 7 days.
    pub rate_limits: Vec<ObservedRateLimit>,
    /// Session files the scan read.
    pub sessions: u64,
}
