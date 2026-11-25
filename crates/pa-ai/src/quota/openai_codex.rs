//! `ChatGPT` (Codex) subscription quota: `GET {base}/wham/usage` with the Codex OAuth bearer and
//! the `ChatGPT-Account-Id` header.
//!
//! Fields read (observed wire shape): `plan_type`, `rate_limit.primary_window` /
//! `rate_limit.secondary_window` as `{ used_percent, limit_window_seconds, reset_after_seconds,
//! reset_at }`. `reset_at` is epoch seconds (a value past 10^12 is already milliseconds);
//! without it the reset is `now + reset_after_seconds`. Inferred: the window names, derived from
//! `limit_window_seconds` (18 000 s reads `5-hour`, 604 800 s `Weekly`).

use pa_types::provider_usage::{QuotaUnit, QuotaWindow};
use serde_json::Value;

use super::{get_json, number, FetchedQuota};
use crate::oauth::ProviderHttp;
use crate::providers::openai_codex_responses::request::{
    extract_account_id, DEFAULT_CODEX_BASE_URL,
};

/// The production `ChatGPT` backend origin (the account routes live under it, never under a
/// streaming proxy's base URL).
pub const OPENAI_CODEX_USAGE_BASE_URL: &str = DEFAULT_CODEX_BASE_URL;

/// Fetch and parse the Codex plan windows. `now_ms` anchors a relative `reset_after_seconds`.
/// The account id comes from the token's own claim, the way the request path reads it.
///
/// # Errors
///
/// The card's error text: a token without an account claim, a transport failure, a rejected
/// sign-in, or an unexpected body.
#[tracing::instrument(name = "quota.openai_codex", skip_all)]
pub async fn fetch_openai_codex_quota(
    http: &dyn ProviderHttp,
    base_url: &str,
    access_token: &str,
    now_ms: i64,
) -> Result<FetchedQuota, String> {
    let account_id = extract_account_id(access_token)
        .map_err(|_| "the stored ChatGPT sign-in has no account id; run /login".to_string())?;
    let payload = get_json(
        http,
        format!("{}/wham/usage", base_url.trim_end_matches('/')),
        vec![
            ("Authorization".into(), format!("Bearer {access_token}")),
            ("ChatGPT-Account-Id".into(), account_id),
            ("Accept".into(), "application/json".into()),
        ],
    )
    .await?;
    parse_codex_usage(&payload, now_ms)
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "epoch times in ms fit i64 for any date the endpoint can report"
)]
fn parse_codex_usage(payload: &Value, now_ms: i64) -> Result<FetchedQuota, String> {
    if !payload.is_object() {
        return Err("the usage endpoint returned an unexpected response".to_string());
    }
    let windows = [
        ("primary_window", "Primary"),
        ("secondary_window", "Secondary"),
    ]
    .iter()
    .filter_map(|(field, fallback)| {
        let window = payload.get("rate_limit")?.get(*field)?;
        let used = number(window.get("used_percent"))?;
        let label = number(window.get("limit_window_seconds"))
            .map_or_else(|| (*fallback).to_string(), window_label);
        let resets_at_ms = number(window.get("reset_at"))
            .map(|reset_at| {
                if reset_at > 1e12 {
                    reset_at as i64
                } else {
                    (reset_at * 1000.0) as i64
                }
            })
            .or_else(|| {
                number(window.get("reset_after_seconds"))
                    .map(|seconds| now_ms + (seconds * 1000.0) as i64)
            });
        Some(QuotaWindow {
            label,
            used: used.clamp(0.0, 100.0),
            limit: Some(100.0),
            unit: QuotaUnit::Percent,
            resets_at_ms,
        })
    })
    .collect();
    Ok(FetchedQuota {
        plan: payload
            .get("plan_type")
            .and_then(Value::as_str)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string),
        windows,
    })
}

/// `5-hour`, `Daily`, `Weekly`, or `N-hour` / `N-day` for a window length in seconds.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the hour count is rounded and floored at one before the cast"
)]
fn window_label(seconds: f64) -> String {
    let hours = (seconds / 3600.0).round().max(1.0) as u64;
    match hours {
        24 => "Daily".to_string(),
        168 => "Weekly".to_string(),
        hours if hours % 24 == 0 => format!("{}-day", hours / 24),
        hours => format!("{hours}-hour"),
    }
}
