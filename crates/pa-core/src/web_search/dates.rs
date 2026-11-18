//! Calendar arithmetic for search date filters: ISO dates to epoch seconds
//! and back (UTC, proleptic Gregorian), plus the relative ages engines print.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_SECONDS: i64 = 86_400;

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub(crate) fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// `YYYY-MM-DD` for an epoch second (UTC).
pub(crate) fn epoch_seconds_to_iso(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(DAY_SECONDS));
    format!("{year:04}-{month:02}-{day:02}")
}

/// UTC `YYYY-MM-DD` `days` before now.
pub(crate) fn iso_days_ago(days: i64) -> String {
    epoch_seconds_to_iso(now_epoch_seconds() - days * DAY_SECONDS)
}

/// Epoch seconds at UTC midnight of a `YYYY-MM-DD` prefix (a timestamp's
/// time part is honoured to the second when present).
pub(crate) fn iso_to_epoch_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    let date = value.get(..10)?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut seconds = days_from_civil(year, month, day) * DAY_SECONDS;
    if let Some(time) = value
        .get(11..19)
        .filter(|_| matches!(value.as_bytes().get(10), Some(b'T' | b' ')))
    {
        let mut fields = time.split(':').map(str::parse::<i64>);
        if let (Some(Ok(h)), Some(Ok(m)), Some(Ok(s))) =
            (fields.next(), fields.next(), fields.next())
        {
            seconds += h * 3600 + m * 60 + s;
        }
    }
    Some(seconds)
}

/// Best-effort publish time of a source date: ISO, or relative (`3 days ago`).
pub(crate) fn published_epoch_seconds(value: &str) -> Option<i64> {
    if let Some(seconds) = iso_to_epoch_seconds(value) {
        return Some(seconds);
    }
    let lowered = value.trim().to_lowercase();
    let rest = lowered.strip_suffix(" ago")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let amount: i64 = digits.parse().ok()?;
    let unit = rest[digits.len()..].trim().trim_end_matches('s');
    let unit_seconds = match unit {
        "m" | "min" | "minute" => 60,
        "h" | "hr" | "hour" => 3600,
        "d" | "day" => DAY_SECONDS,
        "w" | "week" => 7 * DAY_SECONDS,
        "mo" | "month" => 30 * DAY_SECONDS,
        "y" | "yr" | "year" => 365 * DAY_SECONDS,
        _ => return None,
    };
    Some(now_epoch_seconds() - amount * unit_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_through_epoch_seconds() {
        assert_eq!(iso_to_epoch_seconds("2024-03-01"), Some(1_709_251_200));
        assert_eq!(epoch_seconds_to_iso(1_709_251_200), "2024-03-01");
        assert_eq!(
            iso_to_epoch_seconds("2024-03-01T01:02:03.000Z"),
            Some(1_709_251_200 + 3723)
        );
    }
}
