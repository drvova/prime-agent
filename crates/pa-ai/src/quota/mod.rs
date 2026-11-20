//! Provider quota fetchers for the `/usage` page: each subscription provider's own usage
//! endpoint, read with the stored OAuth credential and parsed into the shared
//! [`pa_types::provider_usage::QuotaWindow`] vocabulary. Read-only: nothing here spends, resets,
//! or refreshes anything. Every request goes through the caller's [`ProviderHttp`] with a bounded
//! timeout, so the daemon runs them off every paint path and tests serve them from loopback.
//!
//! Which response fields are confirmed: the shapes follow the endpoints' observed or documented
//! wire shapes (the fixtures under `fixtures/` mirror them); each parser's doc names the fields
//! it reads and which of them are inferred rather than observed.

mod anthropic;
mod github_copilot;
mod openai_codex;
mod synthetic;

#[cfg(test)]
mod tests;

pub use anthropic::{fetch_anthropic_quota, ANTHROPIC_USAGE_BASE_URL};
pub use github_copilot::{fetch_github_copilot_quota, github_api_base_url};
pub use openai_codex::{fetch_openai_codex_quota, OPENAI_CODEX_USAGE_BASE_URL};
pub use synthetic::{fetch_synthetic_quota, SYNTHETIC_QUOTA_BASE_URL};

use pa_types::provider_usage::QuotaWindow;
use serde_json::Value;

use crate::oauth::{ProviderHttp, ProviderHttpMethod, ProviderHttpRequest};

/// The per-request bound: a hung endpoint fails its own card after this long.
pub const QUOTA_REQUEST_TIMEOUT_MS: u64 = 8_000;

/// One provider's parsed answer: the plan it reports and its limit windows.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedQuota {
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

/// GET `url` with `headers` and parse the JSON body; a non-2xx status or an unparseable body is
/// the card's error text (never the URL or the body: the text is shown as-is).
async fn get_json(
    http: &dyn ProviderHttp,
    url: String,
    headers: Vec<(String, String)>,
) -> Result<Value, String> {
    let response = http
        .request(
            ProviderHttpRequest {
                method: ProviderHttpMethod::Get,
                url,
                headers,
                body: None,
                follow_redirects: true,
            },
            QUOTA_REQUEST_TIMEOUT_MS,
        )
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    match response.status {
        200..=299 => serde_json::from_str(&response.body)
            .map_err(|_| "the usage endpoint returned an unexpected response".to_string()),
        401 | 403 => Err(format!(
            "the provider rejected the sign-in (HTTP {}); run /login",
            response.status
        )),
        429 => Err("the usage endpoint is rate limited (HTTP 429)".to_string()),
        status => Err(format!("the usage endpoint answered HTTP {status}")),
    }
}

/// An RFC 3339 / ISO-8601 timestamp (`2026-11-18T19:00:00.123+00:00`, `...Z`, or a bare
/// `2026-12-01` date read as UTC midnight, the JavaScript `Date.parse` rule) in epoch ms.
fn parse_iso_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = match text.split_once(['T', ' ']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let years = if month <= 2 { year - 1 } else { year };
    let era = years.div_euclid(400);
    let year_of_era = years.rem_euclid(400);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let Some(time) = time else {
        return Some(days * 86_400_000);
    };
    let zone_at = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(zone_at);
    let mut fields = clock.splitn(3, ':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next()?.parse().ok()?;
    let (second, millis) = match fields.next() {
        Some(seconds) => match seconds.split_once('.') {
            Some((whole, fraction)) => {
                let digits: String = fraction
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .take(3)
                    .collect();
                let millis = format!("{digits:0<3}").parse::<i64>().ok()?;
                (whole.parse::<i64>().ok()?, millis)
            }
            None => (seconds.parse::<i64>().ok()?, 0),
        },
        None => (0, 0),
    };
    let offset_ms = match zone {
        "" | "Z" | "z" => 0,
        signed => {
            let sign = if signed.starts_with('-') { -1 } else { 1 };
            let digits: String = signed[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: i64 = digits.get(..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4).map_or(Some(0), |m| m.parse().ok())?;
            sign * (hours * 3_600_000 + minutes * 60_000)
        }
    };
    Some(
        days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis - offset_ms,
    )
}

/// A finite JSON number (or a numeric string, which some endpoints send).
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
    .filter(|number: &f64| number.is_finite())
}
