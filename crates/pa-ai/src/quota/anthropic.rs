//! Claude subscription quota: `GET {base}/api/oauth/usage` with the OAuth access token.
//!
//! Fields read (observed wire shape): `five_hour` / `seven_day` / `seven_day_opus` /
//! `seven_day_sonnet` as `{ utilization: 0..100, resets_at: ISO-8601 }` (each nullable: `null`
//! means the window is not running), and `limits[]` rows `{ kind: "weekly_scoped", percent,
//! resets_at, scope.model.display_name }` for the model-scoped weekly caps. Inferred: that
//! `utilization` is a percentage of an undisclosed cap (the page shows it as `N / 100 %`).

use pa_types::provider_usage::{QuotaUnit, QuotaWindow};
use serde_json::Value;

use super::{get_json, number, parse_iso_ms, FetchedQuota};
use crate::oauth::ProviderHttp;
use crate::providers::anthropic::CLAUDE_CODE_VERSION;

/// The production API origin.
pub const ANTHROPIC_USAGE_BASE_URL: &str = "https://api.anthropic.com";

/// The OAuth account routes' beta flag.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The fixed windows: payload field and page label.
const BUCKETS: [(&str, &str); 4] = [
    ("five_hour", "5-hour"),
    ("seven_day", "Weekly"),
    ("seven_day_opus", "Weekly (Opus)"),
    ("seven_day_sonnet", "Weekly (Sonnet)"),
];

/// Fetch and parse the Claude subscription windows.
///
/// # Errors
///
/// The card's error text: a transport failure, a rejected sign-in, or an unexpected body.
#[tracing::instrument(name = "quota.anthropic", skip_all)]
pub async fn fetch_anthropic_quota(
    http: &dyn ProviderHttp,
    base_url: &str,
    access_token: &str,
) -> Result<FetchedQuota, String> {
    let payload = get_json(
        http,
        format!("{}/api/oauth/usage", base_url.trim_end_matches('/')),
        vec![
            ("Authorization".into(), format!("Bearer {access_token}")),
            ("anthropic-beta".into(), OAUTH_BETA.into()),
            ("Accept".into(), "application/json".into()),
            (
                "User-Agent".into(),
                format!("claude-cli/{CLAUDE_CODE_VERSION}"),
            ),
        ],
    )
    .await?;
    parse_anthropic_usage(&payload)
}

fn parse_anthropic_usage(payload: &Value) -> Result<FetchedQuota, String> {
    if !payload.is_object() {
        return Err("the usage endpoint returned an unexpected response".to_string());
    }
    let mut windows: Vec<QuotaWindow> = BUCKETS
        .iter()
        .filter_map(|(field, label)| {
            let bucket = payload.get(*field)?;
            percent_window(
                label,
                number(bucket.get("utilization")),
                bucket.get("resets_at"),
            )
        })
        .collect();
    for row in payload
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if row.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
            continue;
        }
        let Some(model) = row
            .pointer("/scope/model/display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let label = format!("Weekly ({model})");
        if windows.iter().any(|window| window.label == label) {
            continue;
        }
        if let Some(window) =
            percent_window(&label, number(row.get("percent")), row.get("resets_at"))
        {
            windows.push(window);
        }
    }
    Ok(FetchedQuota {
        plan: None,
        windows,
    })
}

/// A percent window; `None` without a utilization (a `null` or idle bucket).
fn percent_window(
    label: &str,
    used: Option<f64>,
    resets_at: Option<&Value>,
) -> Option<QuotaWindow> {
    Some(QuotaWindow {
        label: label.to_string(),
        used: used?.clamp(0.0, 100.0),
        limit: Some(100.0),
        unit: QuotaUnit::Percent,
        resets_at_ms: resets_at.and_then(Value::as_str).and_then(parse_iso_ms),
    })
}
