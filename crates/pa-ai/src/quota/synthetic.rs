//! Synthetic subscription quota: `GET {base}/v2/quotas` with the API key as a bearer token
//! (documented at dev.synthetic.new/docs/synthetic/quotas; the call does not count against the
//! subscription).
//!
//! Fields read (the documented response): `subscription` as `{ limit, requests, renewsAt }`, the
//! request cap, the requests used, and the ISO-8601 renewal time. An account without a
//! subscription object reports no window.

use pa_types::provider_usage::{QuotaUnit, QuotaWindow};
use serde_json::Value;

use super::{get_json, number, parse_iso_ms, FetchedQuota};
use crate::oauth::ProviderHttp;

/// The production API origin.
pub const SYNTHETIC_QUOTA_BASE_URL: &str = "https://api.synthetic.new";

/// Fetch and parse the Synthetic subscription window.
///
/// # Errors
///
/// The card's error text: a transport failure, a rejected key, or an unexpected body.
#[tracing::instrument(name = "quota.synthetic", skip_all)]
pub async fn fetch_synthetic_quota(
    http: &dyn ProviderHttp,
    base_url: &str,
    api_key: &str,
) -> Result<FetchedQuota, String> {
    let payload = get_json(
        http,
        format!("{}/v2/quotas", base_url.trim_end_matches('/')),
        vec![
            ("Authorization".into(), format!("Bearer {api_key}")),
            ("Accept".into(), "application/json".into()),
        ],
    )
    .await?;
    parse_synthetic_quotas(&payload)
}

fn parse_synthetic_quotas(payload: &Value) -> Result<FetchedQuota, String> {
    if !payload.is_object() {
        return Err("the usage endpoint returned an unexpected response".to_string());
    }
    let windows = payload
        .get("subscription")
        .and_then(|subscription| {
            Some(QuotaWindow {
                label: "Subscription".to_string(),
                used: number(subscription.get("requests"))?.max(0.0),
                limit: number(subscription.get("limit")),
                unit: QuotaUnit::Requests,
                resets_at_ms: subscription
                    .get("renewsAt")
                    .and_then(Value::as_str)
                    .and_then(parse_iso_ms),
            })
        })
        .into_iter()
        .collect();
    Ok(FetchedQuota {
        plan: None,
        windows,
    })
}
