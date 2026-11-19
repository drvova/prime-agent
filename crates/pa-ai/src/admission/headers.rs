//! The provider rate-limit header families, read into the controller's [`LimitReport`]:
//! OpenAI-style `x-ratelimit-{limit,remaining,reset}-{requests,tokens}`, Anthropic
//! `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-{limit,remaining,reset}`,
//! and `retry-after` / `retry-after-ms`.

use std::collections::{BTreeMap, HashMap};

use pa_types::control::{LimitReport, LimitWindow};

use crate::utils_inner::stream_failure::{header_value, parse_reset_ms, parse_retry_after_ms};

/// Read every family a response carried. Absent headers stay `None`; a provider that sends
/// none yields the empty report, and the controller then adapts from outcomes alone.
#[must_use]
pub fn parse_limit_report(headers: &BTreeMap<String, String>) -> LimitReport {
    let headers: HashMap<String, String> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    let requests = [
        window(&headers, "x-ratelimit-{}-requests"),
        window(&headers, "anthropic-ratelimit-requests-{}"),
    ]
    .into_iter()
    .find(LimitWindow::is_reported)
    .unwrap_or_default();
    // Anthropic reports a combined token window and split input/output windows: the tightest
    // one paces.
    let tokens = [
        window(&headers, "x-ratelimit-{}-tokens"),
        window(&headers, "anthropic-ratelimit-tokens-{}"),
        window(&headers, "anthropic-ratelimit-input-tokens-{}"),
        window(&headers, "anthropic-ratelimit-output-tokens-{}"),
    ]
    .into_iter()
    .filter(LimitWindow::is_reported)
    .min_by_key(|window| match (window.limit, window.remaining) {
        (Some(limit), Some(remaining)) if limit > 0 => remaining.saturating_mul(10_000) / limit,
        _ => u64::MAX,
    })
    .unwrap_or_default();
    LimitReport {
        requests,
        tokens,
        retry_after_ms: parse_retry_after_ms(&headers),
    }
}

/// One window of a family: `pattern`'s `{}` takes `limit`, `remaining`, and `reset`.
fn window(headers: &HashMap<String, String>, pattern: &str) -> LimitWindow {
    let value = |part: &str| header_value(headers, &pattern.replace("{}", part));
    let number = |part: &str| value(part).and_then(|raw| raw.trim().parse::<u64>().ok());
    LimitWindow {
        limit: number("limit"),
        remaining: number("remaining"),
        reset_ms: value("reset").and_then(|raw| parse_reset_ms(&raw)),
    }
}
