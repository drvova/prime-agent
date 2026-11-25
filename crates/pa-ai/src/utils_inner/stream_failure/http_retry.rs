//! The HTTP `retry-after` parsing family: the header lookup, the
//! seconds-vs-HTTP-date forms, and the civil-date math behind the date form.
use super::now_ms;

pub(crate) fn header_value<S: std::hash::BuildHasher>(
    headers: &std::collections::HashMap<String, String, S>,
    name: &str,
) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Parse Retry-After / Retry-After-Ms headers into a millisecond wait.
#[must_use]
pub fn parse_retry_after_ms<S: std::hash::BuildHasher>(
    headers: &std::collections::HashMap<String, String, S>,
) -> Option<u64> {
    if let Some(value) = header_value(headers, "retry-after-ms") {
        if let Ok(ms) = value.parse::<f64>() {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the retry-after-ms header is finite and non-negative here; the wait is u64 ms"
            )]
            if ms.is_finite() && ms >= 0.0 {
                return Some(ms as u64);
            }
        }
    }
    let raw = header_value(headers, "retry-after")?;
    if let Ok(seconds) = raw.parse::<f64>() {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the retry-after header is a finite, non-negative second count here; the wait is u64 ms"
        )]
        if seconds.is_finite() && seconds >= 0.0 {
            return Some((seconds * 1000.0) as u64);
        }
    }
    // HTTP-date form: compute the delta against now.
    match parse_http_date(&raw) {
        Some(date_ms) => {
            #[expect(
                clippy::cast_possible_wrap,
                reason = "epoch milliseconds fit i64 for about 292 million years"
            )]
            let now = now_ms() as i64;
            #[expect(
                clippy::cast_sign_loss,
                reason = "the max(0) floor keeps the delta non-negative for the u64 wait"
            )]
            let wait = (date_ms - now).max(0) as u64;
            Some(wait)
        }
        None => None,
    }
}

/// The rate-limit reset headers a 429 without `Retry-After` falls back to.
const RATE_LIMIT_RESET_HEADERS: [&str; 3] = [
    "x-ratelimit-reset",
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
];

/// Parse the common `x-ratelimit-reset*` headers into a millisecond wait: the latest reset wins
/// (every exhausted limit must reset before a retry can pass). A value is a duration
/// (`1m30s`, `250ms`, `6m0s`), plain seconds, or an epoch timestamp (values at or above 10^12 are
/// epoch milliseconds, at or above 10^9 epoch seconds; the wait is the delta to now).
#[must_use]
pub fn parse_rate_limit_reset_ms<S: std::hash::BuildHasher>(
    headers: &std::collections::HashMap<String, String, S>,
) -> Option<u64> {
    RATE_LIMIT_RESET_HEADERS
        .iter()
        .filter_map(|name| header_value(headers, name))
        .filter_map(|raw| parse_reset_ms(&raw))
        .max()
}

/// One rate-limit reset value as a millisecond wait: a duration (`1m30s`, `250ms`, `6m0s`),
/// plain seconds, an epoch timestamp (values at or above 10^12 are epoch milliseconds, at or
/// above 10^9 epoch seconds), or an RFC 3339 instant (Anthropic's
/// `anthropic-ratelimit-*-reset`); the wait to an instant is the delta to now.
pub(crate) fn parse_reset_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    #[expect(
        clippy::cast_possible_wrap,
        reason = "epoch milliseconds fit i64 for about 292 million years"
    )]
    let now = now_ms() as i64;
    match raw.parse::<f64>() {
        Ok(number) if number.is_finite() && number >= 0.0 => {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the parsed number is finite and non-negative, and the floor keeps the delta non-negative"
            )]
            let wait = if number >= 1e12 {
                (number as i64 - now).max(0) as u64
            } else if number >= 1e9 {
                ((number * 1000.0) as i64 - now).max(0) as u64
            } else {
                (number * 1000.0) as u64
            };
            Some(wait)
        }
        Ok(_) => None,
        Err(_) => parse_duration_ms(raw).or_else(|| {
            #[expect(
                clippy::cast_sign_loss,
                reason = "the floor keeps the delta non-negative for the u64 wait"
            )]
            parse_rfc3339_ms(raw).map(|instant| (instant - now).max(0) as u64)
        }),
    }
}

/// A minimal RFC 3339 parser (`2026-11-19T12:00:30Z`, fractional seconds and numeric offsets
/// allowed) into epoch milliseconds.
fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    let (date, time) = raw.split_once(['T', 't'])?;
    let mut date_parts = date.splitn(3, '-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let (clock, offset_seconds) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let sign_at = time.rfind(['+', '-'])?;
        let (clock, offset) = time.split_at(sign_at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':')?;
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        (clock, sign * (hours * 3600 + minutes * 60))
    };
    let mut clock_parts = clock.splitn(3, ':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let seconds: f64 = clock_parts.next()?.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a parsed second count is finite and non-negative"
    )]
    let millis = (seconds * 1000.0).round() as i64;
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 - offset_seconds) * 1000 + millis)
}

/// A Go-style duration (`1h2m3.5s`, `250ms`, `6m0s`) in milliseconds.
fn parse_duration_ms(raw: &str) -> Option<u64> {
    let mut total = 0.0_f64;
    let mut rest = raw;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|&end| end > 0)?;
        let amount: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_end] {
            "h" => 3_600_000.0,
            "m" => 60_000.0,
            "s" => 1000.0,
            "ms" => 1.0,
            _ => return None,
        };
        total += amount * scale;
        rest = &rest[unit_end..];
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a parsed duration is finite and non-negative; the wait is u64 ms"
    )]
    Some(total as u64)
}

fn parse_http_date(raw: &str) -> Option<i64> {
    // Minimal IMF-fixdate parser: "Sun, 06 Nov 1994 08:49:37 GMT".
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<&str> = parts[4].split(':').collect();
    if time.len() < 3 {
        return None;
    }
    let (h, m, s): (i64, i64, i64) = (
        time[0].parse().ok()?,
        time[1].parse().ok()?,
        time[2].parse().ok()?,
    );
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + h * 3600 + m * 60 + s) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
