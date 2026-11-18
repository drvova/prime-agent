//! The `/usage` page's text formats: reset times (the local clock plus a countdown), money,
//! token counts, and the 30-day spark. Pure: the local-time lookup is a parameter, so the zone
//! rules are tested without touching the process's time zone.

use pa_types::platform::LocalCivil;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A span of milliseconds as the page's countdown: `45s`, `13m`, `2h 13m`, `3d 4h`. Floors each
/// unit, so the label only ever ticks down.
#[must_use]
pub(crate) fn countdown(span_ms: i64) -> String {
    let span = span_ms.max(0);
    if span < MINUTE_MS {
        return format!("{}s", span / 1000);
    }
    if span < HOUR_MS {
        return format!("{}m", span / MINUTE_MS);
    }
    if span < DAY_MS {
        let minutes = (span % HOUR_MS) / MINUTE_MS;
        return match minutes {
            0 => format!("{}h", span / HOUR_MS),
            minutes => format!("{}h {minutes}m", span / HOUR_MS),
        };
    }
    let hours = (span % DAY_MS) / HOUR_MS;
    match hours {
        0 => format!("{}d", span / DAY_MS),
        hours => format!("{}d {hours}h", span / DAY_MS),
    }
}

/// How much of a reset label a narrow row keeps (the drop order runs top to bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ResetDetail {
    /// `resets 14:30 (in 2h 13m)`.
    ClockAndCountdown,
    /// `resets in 2h 13m`.
    Countdown,
    /// `2h 13m`.
    Bare,
}

/// The local wall-clock time of a reset relative to `now_ms`: the clock alone on the same local
/// day, the weekday within the coming week, else the date. `None` without a zone binding.
fn local_when(
    now_ms: i64,
    at_ms: i64,
    civil: &dyn Fn(i64) -> Option<LocalCivil>,
) -> Option<String> {
    let at = civil(at_ms)?;
    let now = civil(now_ms)?;
    let clock = format!("{:02}:{:02}", at.hour, at.minute);
    let same_day = (at.year, at.month, at.day) == (now.year, now.month, now.day);
    if same_day {
        return Some(clock);
    }
    let weekday = WEEKDAYS[usize::from(at.weekday % 7)];
    if (at_ms - now_ms).abs() < 6 * DAY_MS {
        return Some(format!("{weekday} {clock}"));
    }
    let month = MONTHS[usize::from(at.month.clamp(1, 12) - 1)];
    Some(format!("{weekday} {} {month} {clock}", at.day))
}

/// The reset label at `detail`: `resets 14:30 (in 2h 13m)` ahead of the reset, `reset 14:30
/// (5m ago)` once it passed (the provider has not reported the new window yet). Without a zone
/// binding the clock drops and the countdown stands alone.
#[must_use]
pub(crate) fn reset_label(
    now_ms: i64,
    resets_at_ms: i64,
    detail: ResetDetail,
    civil: &dyn Fn(i64) -> Option<LocalCivil>,
) -> String {
    let ahead = resets_at_ms > now_ms;
    let span = countdown((resets_at_ms - now_ms).abs());
    let when = match detail {
        ResetDetail::ClockAndCountdown => local_when(now_ms, resets_at_ms, civil),
        ResetDetail::Countdown | ResetDetail::Bare => None,
    };
    match (detail, ahead, when) {
        (ResetDetail::Bare, true, _) => span,
        (ResetDetail::Bare, false, _) => format!("{span} ago"),
        (ResetDetail::ClockAndCountdown | ResetDetail::Countdown, true, Some(when)) => {
            format!("resets {when} (in {span})")
        }
        (ResetDetail::ClockAndCountdown | ResetDetail::Countdown, true, None) => {
            format!("resets in {span}")
        }
        (ResetDetail::ClockAndCountdown | ResetDetail::Countdown, false, Some(when)) => {
            format!("reset {when} ({span} ago)")
        }
        (ResetDetail::ClockAndCountdown | ResetDetail::Countdown, false, None) => {
            format!("reset {span} ago")
        }
    }
}

/// When the countdown to (or since) `resets_at_ms` next changes its text, as epoch ms: the next
/// second under a minute, the next minute under a day, else the next hour.
#[must_use]
pub(crate) fn next_label_change(now_ms: i64, resets_at_ms: i64) -> i64 {
    let span = (resets_at_ms - now_ms).abs();
    let unit = if span < MINUTE_MS {
        1000
    } else if span < DAY_MS {
        MINUTE_MS
    } else {
        HOUR_MS
    };
    if resets_at_ms > now_ms {
        // Counting down: the floor drops one millisecond past the span's unit remainder.
        now_ms + span % unit + 1
    } else {
        // Counting up since the reset.
        now_ms + (unit - span % unit)
    }
}

/// US dollars: `$0.42`, `$12.30`, `$1,234`; a nonzero amount under a cent reads `<$0.01`.
#[must_use]
pub(crate) fn money(cost: f64) -> String {
    if cost > 0.0 && cost < 0.005 {
        return "<$0.01".to_string();
    }
    if cost < 1000.0 {
        return format!("${cost:.2}");
    }
    let whole = format!("{:.0}", cost.round());
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("${grouped}")
}

/// A compact token count: `950`, `12.3k`, `4.5M`, `1.2B`.
#[must_use]
pub(crate) fn tokens(count: u64) -> String {
    let value = count as f64;
    match count {
        0..=999 => count.to_string(),
        1_000..=999_999 => format!("{:.1}k", value / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", value / 1e6),
        1_000_000_000.. => format!("{:.1}B", value / 1e9),
    }
}

/// One glyph per value, scaled to the largest: a zero day is `·`, any nonzero day at least `▁`.
#[must_use]
pub(crate) fn spark(values: &[f64]) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    values
        .iter()
        .map(|value| {
            if *value <= 0.0 || max <= 0.0 {
                '·'
            } else {
                let level = ((value / max) * 7.0).round() as usize;
                LEVELS[level.min(7)]
            }
        })
        .collect()
}
