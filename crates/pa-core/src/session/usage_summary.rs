//! A session's own token and cost summary, as the session list, the RLM
//! ledger's deletion snapshots and the usage rollups publish it.

use serde::{Deserialize, Serialize};

/// The token/cost summary the rows publish: `inputTokens` folds cache reads
/// and writes into the input total; `cost` is the recorded total (a plan's
/// calls count at their API-equivalent value), and `billing` splits the
/// session's own calls by billing mode.
///
/// `Eq` is manual: `cost` is the f64 under `JsNumber`, and JSON numbers are
/// always finite (a NaN never round-trips `serde_json`), so equality is
/// total.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<pa_types::billing::BillingSplit>,
}

impl Eq for SessionUsageSummary {}

/// One model's share of a session's own usage, with the cache split the
/// summary folds away (the deleted-child capture's per-model snapshot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageSummary {
    pub provider: String,
    pub id: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}
