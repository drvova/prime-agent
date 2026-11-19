//! How a model call is paid for: the billing mode stamped on each assistant message's usage, and
//! the per-mode spend split the session summaries and the `/usage` page carry. A mode says what a
//! recorded `cost` means: under [`BillingMode::Plan`] the cost is the API-equivalent value of the
//! call, never a charge.

use serde::{Deserialize, Serialize};

/// Where a pay-as-you-go cost came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// The catalog's per-token rates applied to the call's token counts.
    Catalog,
    /// The provider priced the call itself (`OpenRouter`'s `usage.cost`).
    ProviderReported,
}

/// How one model call is paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum BillingMode {
    /// Metered per call: the recorded cost is what the account is charged.
    PayAsYouGo { source: CostSource },
    /// Covered by a subscription plan (Claude, `ChatGPT`, Copilot, a coding plan): the recorded
    /// cost is the API-equivalent value, not a charge. `provider_quota` is true where the
    /// provider serves usage windows for the plan (the `/usage` quota cards).
    Plan { provider_quota: bool },
    /// A fixed fee with no metered use.
    FlatRate,
    /// A local or private endpoint: nothing is billed.
    Free,
    /// No price is known for the call; this is not "$0".
    Unknown,
}

/// The lenient reader of the optional `billing` usage field: a mode this build does not know
/// (written by a newer one) reads as unclassified instead of failing the whole message.
///
/// # Errors
///
/// Never fails on the value's content; only a broken JSON stream errors.
pub fn deserialize_lenient_billing<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BillingMode>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// The calls and recorded cost of one billing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModeSpend {
    pub calls: u64,
    /// The recorded cost in USD: a charge for pay-as-you-go, the API-equivalent value for a plan.
    pub cost: f64,
}

/// Recorded spend split by billing mode. `unclassified` holds calls recorded before modes were
/// stamped (and rows that carry no mode): their cost reads as it always did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BillingSplit {
    pub pay_as_you_go: ModeSpend,
    pub plan: ModeSpend,
    pub flat_rate: ModeSpend,
    pub free: ModeSpend,
    pub unknown: ModeSpend,
    pub unclassified: ModeSpend,
}

impl BillingSplit {
    /// Count one call of `mode` (`None`: unclassified) costing `cost`.
    pub fn add(&mut self, mode: Option<&BillingMode>, cost: f64) {
        let spend = match mode {
            Some(BillingMode::PayAsYouGo { .. }) => &mut self.pay_as_you_go,
            Some(BillingMode::Plan { .. }) => &mut self.plan,
            Some(BillingMode::FlatRate) => &mut self.flat_rate,
            Some(BillingMode::Free) => &mut self.free,
            Some(BillingMode::Unknown) => &mut self.unknown,
            None => &mut self.unclassified,
        };
        spend.calls += 1;
        spend.cost += cost;
    }

    /// Fold another split into this one.
    pub fn merge(&mut self, other: &BillingSplit) {
        for (spend, other) in [
            (&mut self.pay_as_you_go, other.pay_as_you_go),
            (&mut self.plan, other.plan),
            (&mut self.flat_rate, other.flat_rate),
            (&mut self.free, other.free),
            (&mut self.unknown, other.unknown),
            (&mut self.unclassified, other.unclassified),
        ] {
            spend.calls += other.calls;
            spend.cost += other.cost;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_mode_reads_as_unclassified() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "deserialize_lenient_billing")]
            billing: Option<BillingMode>,
        }
        let read = |json: &str| serde_json::from_str::<Holder>(json).unwrap().billing;
        assert_eq!(
            (
                read(r#"{"billing":{"mode":"plan","providerQuota":true}}"#),
                read(r#"{"billing":{"mode":"barter"}}"#),
                read("{}"),
            ),
            (
                Some(BillingMode::Plan {
                    provider_quota: true
                }),
                None,
                None
            )
        );
    }
}
