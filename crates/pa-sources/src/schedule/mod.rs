//! The one schedule grammar and evaluator: when a timer source fires, and
//! when a scheduled job (pa-core's store) runs next.
//!
//! A [`Schedule`] fires every interval, on a five-field cron expression, or
//! once at a time. It is read from a timer's options ([`Schedule::every`],
//! [`Cron::parse`], [`Schedule::at`]) or from a scheduled job's sentence
//! ([`Schedule::parse`]: `in 10m`, `every 5m`, `at <ISO time>`, or cron).
//! The shortest interval is the caller's: a timer source takes 1 s, the
//! scheduled-jobs store 10 s.
//!
//! All times are Unix milliseconds, and cron fields are read in UTC.

use std::time::Duration;

const SECOND_MS: u64 = 1_000;
const MINUTE_MS: u64 = 60 * SECOND_MS;
const DAY_MINUTES: u64 = 24 * 60;
/// How far ahead (and back) a cron search looks, in days: 28 years, the
/// period after which dates fall on the same weekdays again, so any
/// day-of-month and weekday pair that exists at all is found.
const CRON_HORIZON_DAYS: u64 = 28 * 366;

/// Why a schedule was refused, worded for whoever wrote it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ScheduleError(String);

/// The units a duration may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnits {
    /// Seconds, minutes, hours and days: an interval.
    WithSeconds,
    /// Minutes, hours and days: the delay of a scheduled job's `in`.
    FromMinutes,
}

/// When something fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Fires each `interval_ms` after the last fire (or after the start,
    /// before the first).
    Every {
        interval_ms: u64,
    },
    Cron(Cron),
    /// Fires once, at `unix_ms`.
    At {
        unix_ms: u64,
    },
}

/// A scheduled job's sentence, read: the schedule, and the expression the
/// job keeps (the sentence without its quotes, a cron alias spelled out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSchedule {
    pub schedule: Schedule,
    pub expression: String,
}

impl Schedule {
    /// A scheduled job's sentence, read at `now_ms`:
    ///
    /// - `in <delay>`: once, the delay (minutes, hours or days) from now;
    /// - `every <interval>` or `each <interval>`: an interval of at least
    ///   `min_interval`;
    /// - `at <ISO time>`: once, at a time after now ([`parse_instant`]);
    /// - otherwise a cron expression ([`Cron::parse`]).
    ///
    /// The keywords are read in any case, and the sentence may be wrapped
    /// in one pair of matching quotes.
    ///
    /// # Errors
    ///
    /// The sentence is empty, or its form refuses it.
    pub fn parse(
        text: &str,
        now_ms: u64,
        min_interval: Duration,
    ) -> Result<ParsedSchedule, ScheduleError> {
        let text = text.trim();
        let text = match text.as_bytes() {
            [b'"', .., b'"'] | [b'\'', .., b'\''] => &text[1..text.len() - 1],
            _ => text,
        };
        if text.is_empty() {
            return Err(ScheduleError("A schedule cannot be empty".to_string()));
        }
        let lower = text.to_lowercase();
        let parsed = |schedule| {
            Ok(ParsedSchedule {
                schedule,
                expression: text.to_string(),
            })
        };
        if let Some(delay) = lower.strip_prefix("in ") {
            let unix_ms = parse_duration(delay, DurationUnits::FromMinutes)
                .and_then(|delay| now_ms.checked_add(delay))
                .ok_or_else(|| {
                    ScheduleError(format!(
                        "`in` takes a delay of minutes, hours or days (in 10m, in 2 hours), got {text:?}"
                    ))
                })?;
            return parsed(Schedule::At { unix_ms });
        }
        if let Some(interval) = ["every ", "each "]
            .iter()
            .find_map(|prefix| lower.strip_prefix(prefix))
        {
            return parsed(Schedule::every(interval, min_interval)?);
        }
        if lower.starts_with("at ") {
            let unix_ms = parse_instant(&text[3..]).ok_or_else(|| {
                ScheduleError("Invalid one-shot schedule. Use: at <ISO date>".to_string())
            })?;
            if unix_ms <= now_ms {
                return Err(ScheduleError(
                    "One-shot schedule must be in the future".to_string(),
                ));
            }
            return parsed(Schedule::At { unix_ms });
        }
        let cron = Cron::parse(text).map_err(|error| {
            ScheduleError(format!(
                "{error}; a schedule is `in <delay>`, `every <interval>`, `at <ISO date>` or a cron expression"
            ))
        })?;
        Ok(ParsedSchedule {
            expression: cron.expression.clone(),
            schedule: Schedule::Cron(cron),
        })
    }

    /// An interval ([`parse_duration`] with seconds) of at least
    /// `min_interval`.
    ///
    /// # Errors
    ///
    /// `text` is not a duration, or it is shorter than `min_interval`.
    pub fn every(text: &str, min_interval: Duration) -> Result<Schedule, ScheduleError> {
        let interval_ms = parse_duration(text, DurationUnits::WithSeconds).ok_or_else(|| {
            ScheduleError(format!(
                "an interval is like 30s, 10m, 1h30m or 2 hours, got {text:?}"
            ))
        })?;
        if u128::from(interval_ms) < min_interval.as_millis() {
            let seconds = min_interval.as_secs();
            let plural = if seconds == 1 { "" } else { "s" };
            return Err(ScheduleError(format!(
                "Recurring interval must be at least {seconds} second{plural}"
            )));
        }
        Ok(Schedule::Every { interval_ms })
    }

    /// Once, at an ISO time ([`parse_instant`]), past or not.
    ///
    /// # Errors
    ///
    /// `text` is not an ISO time.
    pub fn at(text: &str) -> Result<Schedule, ScheduleError> {
        let unix_ms = parse_instant(text).ok_or_else(|| {
            ScheduleError(format!(
                "`at` is an ISO time such as 2026-11-25T09:30:00Z, got {text:?}"
            ))
        })?;
        Ok(Schedule::At { unix_ms })
    }

    /// The first time the schedule fires strictly after `after`; `None`
    /// when it never fires again.
    #[must_use]
    pub fn next_after(&self, after: u64) -> Option<u64> {
        match self {
            Schedule::Every { interval_ms } => after.checked_add(*interval_ms),
            Schedule::Cron(cron) => cron.next_after(after),
            Schedule::At { unix_ms } => (*unix_ms > after).then_some(*unix_ms),
        }
    }

    /// The latest time the schedule fires in `due..=now`, given that it
    /// fires at `due` (so `due` itself when `now` is before the next fire):
    /// what a fire that catches up on missed ones is for.
    #[must_use]
    pub fn latest_due(&self, due: u64, now: u64) -> u64 {
        if now <= due {
            return due;
        }
        match self {
            Schedule::Every { interval_ms } => due + (now - due) / interval_ms * interval_ms,
            Schedule::Cron(cron) => cron.latest_at_or_before(now).unwrap_or(due).max(due),
            Schedule::At { .. } => due,
        }
    }
}

/// A duration: one or more `<count><unit>` parts (`30s`, `10m`, `1h30m`,
/// `2 hours`, `1h 30m`), a unit being `s`/`sec`/`secs`/`second`/`seconds`
/// (when `units` allows them), `m`/`min`/`mins`/`minute`/`minutes`,
/// `h`/`hr`/`hrs`/`hour`/`hours` or `d`/`day`/`days`, in lower case. `None`
/// when `text` is not one, or overflows.
#[must_use]
pub fn parse_duration(text: &str, units: DurationUnits) -> Option<u64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let count: u64 = rest[..digits].parse().ok()?;
        rest = rest[digits..].trim_start();
        let letters = rest.len()
            - rest
                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .len();
        let unit_ms = match &rest[..letters] {
            "s" | "sec" | "secs" | "second" | "seconds" => match units {
                DurationUnits::WithSeconds => SECOND_MS,
                DurationUnits::FromMinutes => return None,
            },
            "m" | "min" | "mins" | "minute" | "minutes" => MINUTE_MS,
            "h" | "hr" | "hrs" | "hour" | "hours" => 60 * MINUTE_MS,
            "d" | "day" | "days" => DAY_MINUTES * MINUTE_MS,
            _ => return None,
        };
        total = count
            .checked_mul(unit_ms)
            .and_then(|part| total.checked_add(part))?;
        rest = rest[letters..].trim_start();
    }
    Some(total)
}

/// An ISO 8601 time, as Unix milliseconds: `<date>T<time>[<offset>]`, the
/// date `YYYY-MM-DD` (a real date, not before 1970), the time `HH`, `HH:MM`
/// or `HH:MM:SS[.fraction]`, and the offset `Z`, `+HH`, `+HH:MM` or the
/// same with `-`; no offset is UTC. `T` and `Z` may be lower case. `None`
/// when `text` is not one.
#[must_use]
pub fn parse_instant(text: &str) -> Option<u64> {
    let (date, time) = text.trim().split_once(['T', 't'])?;
    let mut date = date.split('-');
    let (year, month, day): (u64, u64, u64) = (
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
    );
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if date.next().is_some()
        || year < 1970
        || !(1..=12).contains(&month)
        || !(1..=month_days).contains(&day)
    {
        return None;
    }
    let (time, offset_ms, ahead) = if let Some(time) = time.strip_suffix(['Z', 'z']) {
        (time, 0, true)
    } else if let Some(index) = time
        .rfind('+')
        .or_else(|| time.get(1..)?.rfind('-').map(|index| index + 1))
    {
        let mut offset = time[index + 1..].split(':');
        let hours: u64 = offset.next()?.parse().ok()?;
        let minutes: u64 = offset.next().map_or(Some(0), |part| part.parse().ok())?;
        if offset.next().is_some() || hours > 23 || minutes > 59 {
            return None;
        }
        (
            &time[..index],
            (hours * 60 + minutes) * MINUTE_MS,
            time.as_bytes()[index] == b'+',
        )
    } else {
        (time, 0, true)
    };
    let mut clock = time.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next().map_or(Some(0), |part| part.parse().ok())?;
    let (second, millis): (u64, u64) = match clock.next() {
        None => (0, 0),
        Some(part) => match part.split_once('.') {
            None => (part.parse().ok()?, 0),
            Some((second, fraction)) => {
                if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                let millis = format!("{fraction:0<3}")[..3].parse().ok()?;
                (second.parse().ok()?, millis)
            }
        },
    };
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    let march_year = if month <= 2 { year - 1 } else { year };
    let year_of_era = march_year % 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = march_year / 400 * 146_097 + day_of_era - 719_468;
    let local = (days * DAY_MINUTES + hour * 60 + minute) * MINUTE_MS + second * SECOND_MS + millis;
    if ahead {
        local.checked_sub(offset_ms)
    } else {
        local.checked_add(offset_ms)
    }
}

/// A parsed five-field cron expression: each field's allowed values as a
/// bit set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    /// The five fields as written, an alias spelled out.
    expression: String,
    minutes: u64,
    hours: u64,
    days_of_month: u64,
    months: u64,
    /// 0 is Sunday; a 7 in the expression is folded into 0.
    days_of_week: u64,
}

impl Cron {
    /// `minute hour day-of-month month day-of-week`, each `*`, a number, a
    /// range `a-b`, a step `*/n` or `a-b/n` (`n` at most the field's
    /// largest value), or a comma list of those; or one of `@hourly`,
    /// `@daily` (`@midnight`), `@weekly`, `@monthly`, `@yearly`
    /// (`@annually`). A minute fires when every field matches it, the
    /// day-of-month and the weekday included.
    ///
    /// # Errors
    ///
    /// The expression is not five valid fields or an alias, or it matches
    /// no date.
    pub fn parse(text: &str) -> Result<Cron, ScheduleError> {
        let expression = match text.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, monthday, month, weekday] = fields[..] else {
            return Err(ScheduleError(format!(
                "a cron expression has five fields (minute hour day-of-month month day-of-week) or is @hourly, @daily, @weekly, @monthly or @yearly, got {text:?}"
            )));
        };
        let mut days_of_week = parse_field(weekday, 0, 7)?;
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        let cron = Cron {
            expression: expression.to_string(),
            minutes: parse_field(minute, 0, 59)?,
            hours: parse_field(hour, 0, 23)?,
            days_of_month: parse_field(monthday, 1, 31)?,
            months: parse_field(month, 1, 12)?,
            days_of_week,
        };
        if cron.next_after(0).is_none() {
            return Err(ScheduleError(format!(
                "the cron expression {text:?} matches no date"
            )));
        }
        Ok(cron)
    }

    /// The five fields, an alias spelled out (`@hourly` is `0 * * * *`).
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    fn day_matches(&self, day: u64) -> bool {
        // The month and day of month (Howard Hinnant's `civil_from_days`).
        let day_of_era = (day + 719_468) % 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_from_march = (5 * day_of_year + 2) / 153;
        let day_of_month = day_of_year - (153 * month_from_march + 2) / 5 + 1;
        let month = (month_from_march + 2) % 12 + 1;
        let weekday = (day + 4) % 7; // 1970-01-01 was a Thursday.
        self.months & (1 << month) != 0
            && self.days_of_month & (1 << day_of_month) != 0
            && self.days_of_week & (1 << weekday) != 0
    }

    /// The minutes of a matching day, as minutes since midnight, in order.
    fn day_minutes(&self) -> impl DoubleEndedIterator<Item = u64> + '_ {
        (0..24u64)
            .filter(|hour| self.hours & (1 << hour) != 0)
            .flat_map(move |hour| {
                (0..60u64)
                    .filter(|minute| self.minutes & (1 << minute) != 0)
                    .map(move |minute| hour * 60 + minute)
            })
    }

    fn next_after(&self, after: u64) -> Option<u64> {
        let first = after / MINUTE_MS + 1;
        let first_day = first / DAY_MINUTES;
        (first_day..first_day + CRON_HORIZON_DAYS)
            .filter(|day| self.day_matches(*day))
            .find_map(|day| {
                self.day_minutes()
                    .map(|minute| day * DAY_MINUTES + minute)
                    .find(|minute| *minute >= first)
            })
            .map(|minute| minute * MINUTE_MS)
    }

    fn latest_at_or_before(&self, at: u64) -> Option<u64> {
        let last = at / MINUTE_MS;
        let last_day = last / DAY_MINUTES;
        (last_day.saturating_sub(CRON_HORIZON_DAYS)..=last_day)
            .rev()
            .filter(|day| self.day_matches(*day))
            .find_map(|day| {
                self.day_minutes()
                    .rev()
                    .map(|minute| day * DAY_MINUTES + minute)
                    .find(|minute| *minute <= last)
            })
            .map(|minute| minute * MINUTE_MS)
    }
}

/// One cron field as a bit set of the values in `min..=max` it allows.
fn parse_field(field: &str, min: u64, max: u64) -> Result<u64, ScheduleError> {
    let invalid = || ScheduleError(format!("cron field {field:?} is not values in {min}-{max}"));
    let number = |text: &str| -> Result<u64, ScheduleError> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        text.parse().map_err(|_| invalid())
    };
    let mut bits = 0;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, number(step)?),
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            (number(start)?, number(end)?)
        } else {
            let value = number(range)?;
            (value, value)
        };
        if step == 0 || step > max || start < min || start > end || end > max {
            return Err(invalid());
        }
        let mut value = start;
        while value <= end {
            bits |= 1 << value;
            value += step;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests;
