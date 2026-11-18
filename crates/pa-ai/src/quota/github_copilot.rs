//! GitHub Copilot quota: `GET {api}/copilot_internal/user` with the GitHub OAuth token (the
//! stored credential's refresh token — the short-lived Copilot API token does not open it).
//!
//! Fields read (observed wire shape): `copilot_plan`, `quota_reset_date` (a date or timestamp
//! string, read the way JavaScript `Date.parse` reads it), and `quota_snapshots
//! .{premium_interactions,chat,completions}` as `{ entitlement, remaining, unlimited }`. The used
//! count is `entitlement - remaining`. Unlimited chat and completions buckets are omitted (no cap
//! to show); an unlimited premium bucket shows its use without a limit.

use pa_types::provider_usage::{QuotaUnit, QuotaWindow};
use serde_json::Value;

use super::{get_json, number, parse_iso_ms, FetchedQuota};
use crate::oauth::{ProviderHttp, COPILOT_CLIENT_HEADERS};

/// The snapshot buckets: payload field and page label.
const BUCKETS: [(&str, &str); 3] = [
    ("premium_interactions", "Premium requests"),
    ("chat", "Chat"),
    ("completions", "Completions"),
];

/// The GitHub REST origin for a Copilot sign-in: `https://api.github.com`, or
/// `https://api.<domain>` for a GitHub Enterprise domain.
#[must_use]
pub fn github_api_base_url(enterprise_domain: Option<&str>) -> String {
    match enterprise_domain
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
    {
        Some(domain) => {
            let host = domain
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            format!("https://api.{}", host.trim_start_matches("api."))
        }
        None => "https://api.github.com".to_string(),
    }
}

/// Fetch and parse the Copilot quota snapshot.
///
/// # Errors
///
/// The card's error text: a transport failure, a rejected sign-in, or an unexpected body.
#[tracing::instrument(name = "quota.github_copilot", skip_all)]
pub async fn fetch_github_copilot_quota(
    http: &dyn ProviderHttp,
    api_base_url: &str,
    github_token: &str,
) -> Result<FetchedQuota, String> {
    let mut headers: Vec<(String, String)> = COPILOT_CLIENT_HEADERS
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    headers.push(("Authorization".into(), format!("Bearer {github_token}")));
    headers.push(("Accept".into(), "application/json".into()));
    let payload = get_json(
        http,
        format!(
            "{}/copilot_internal/user",
            api_base_url.trim_end_matches('/')
        ),
        headers,
    )
    .await?;
    parse_copilot_usage(&payload)
}

fn parse_copilot_usage(payload: &Value) -> Result<FetchedQuota, String> {
    let Some(snapshots) = payload.get("quota_snapshots") else {
        return Err("the usage endpoint returned an unexpected response".to_string());
    };
    let resets_at_ms = payload
        .get("quota_reset_date")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);
    let windows = BUCKETS
        .iter()
        .filter_map(|(field, label)| {
            let bucket = snapshots.get(*field)?;
            let unlimited = bucket.get("unlimited").and_then(Value::as_bool) == Some(true);
            let entitlement = number(bucket.get("entitlement"))?;
            let remaining = number(bucket.get("remaining"))?;
            if unlimited && *field != "premium_interactions" {
                return None;
            }
            Some(QuotaWindow {
                label: (*label).to_string(),
                used: (entitlement - remaining).max(0.0),
                limit: (!unlimited).then_some(entitlement),
                unit: QuotaUnit::Requests,
                resets_at_ms,
            })
        })
        .collect();
    Ok(FetchedQuota {
        plan: payload
            .get("copilot_plan")
            .and_then(Value::as_str)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string),
        windows,
    })
}
