//! When a timer fires: every interval, on a five-field cron expression, or
//! once at a time. All times are Unix milliseconds, and cron fields are read
//! in UTC.

const MINUTE_MS: u64 = 60_000;
const DAY_MINUTES: u64 = 24 * 60;
/// How far ahead (and back) a cron search looks, in days: 28 years, the
/// period after which dates fall on the same weekdays again, so any
/// day-of-month and weekday pair that exists at all is found.
const CRON_HORIZON_DAYS: u64 = 28 * 366;

/// A timer's schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Schedule {
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

impl Schedule {
    /// The first time the schedule fires strictly after `after`; `None`
    /// when it never fires again.
    pub(crate) fn next_after(&self, after: u64) -> Option<u64> {
        match self {
            Schedule::Every { interval_ms } => after.checked_add(*interval_ms),
            Schedule::Cron(cron) => cron.next_after(after),
            Schedule::At { unix_ms } => (*unix_ms > after).then_some(*unix_ms),
        }
    }

    /// The latest time the schedule fires in `due..=now`, given that it
    /// fires at `due` (so `due` itself when `now` is before the next fire).
    pub(crate) fn latest_due(&self, due: u64, now: u64) -> u64 {
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

/// `every`: one or more `<count><unit>` parts (`30s`, `10m`, `1h30m`, `2d`),
/// at least one second in all.
pub(crate) fn parse_every(text: &str) -> Result<u64, String> {
    let invalid = || format!("`every` is like 30s, 10m, 1h30m or 2d, got {text:?}");
    let mut total: u64 = 0;
    let mut digits = String::new();
    for letter in text.trim().chars() {
        if letter.is_ascii_digit() {
            digits.push(letter);
            continue;
        }
        let unit_ms: u64 = match letter {
            's' => 1_000,
            'm' => MINUTE_MS,
            'h' => 60 * MINUTE_MS,
            'd' => DAY_MINUTES * MINUTE_MS,
            _ => return Err(invalid()),
        };
        let count: u64 = digits.parse().map_err(|_| invalid())?;
        digits.clear();
        total = count
            .checked_mul(unit_ms)
            .and_then(|part| total.checked_add(part))
            .ok_or_else(invalid)?;
    }
    if !digits.is_empty() || total < 1_000 {
        return Err(invalid());
    }
    Ok(total)
}

/// `at`: an RFC 3339 time with seconds and an offset
/// (`2026-11-25T09:30:00Z`, `2026-11-25T10:30:00.5+01:00`), not before 1970.
pub(crate) fn parse_at(text: &str) -> Result<u64, String> {
    let invalid = || format!("`at` is an RFC 3339 time such as 2026-11-25T09:30:00Z, got {text:?}");
    let digits = |part: &[u8]| -> Result<u64, String> {
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        Ok(part
            .iter()
            .fold(0, |value, digit| value * 10 + u64::from(digit - b'0')))
    };
    let bytes = text.as_bytes();
    let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if bytes.len() < 20
        || separators
            .iter()
            .any(|(index, separator)| bytes[*index] != *separator)
        || !matches!(bytes[10], b'T' | b't')
    {
        return Err(invalid());
    }
    let (year, month, day) = (
        digits(&bytes[0..4])?,
        digits(&bytes[5..7])?,
        digits(&bytes[8..10])?,
    );
    let (hour, minute, second) = (
        digits(&bytes[11..13])?,
        digits(&bytes[14..16])?,
        digits(&bytes[17..19])?,
    );
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if year < 1970
        || !(1..=12).contains(&month)
        || !(1..=month_days).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid());
    }
    let mut rest = &bytes[19..];
    let mut millis = 0;
    if let [b'.', fraction @ ..] = rest {
        let length = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if length == 0 {
            return Err(invalid());
        }
        let mut first_three = [b'0'; 3];
        for (place, digit) in fraction[..length.min(3)].iter().enumerate() {
            first_three[place] = *digit;
        }
        millis = digits(&first_three)?;
        rest = &fraction[length..];
    }
    // Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    let march_year = if month <= 2 { year - 1 } else { year };
    let year_of_era = march_year % 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = march_year / 400 * 146_097 + day_of_era - 719_468;
    let local = (days * DAY_MINUTES + hour * 60 + minute) * MINUTE_MS + second * 1_000 + millis;
    match rest {
        [b'Z' | b'z'] => Ok(local),
        [sign @ (b'+' | b'-'), hours @ .., b':', _, _] if hours.len() == 2 => {
            let offset = (digits(hours)? * 60 + digits(&rest[4..6])?) * MINUTE_MS;
            if *sign == b'+' {
                local.checked_sub(offset).ok_or_else(invalid)
            } else {
                Ok(local + offset)
            }
        }
        _ => Err(invalid()),
    }
}

/// A parsed five-field cron expression: each field's allowed values as a
/// bit set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cron {
    minutes: u64,
    hours: u64,
    days_of_month: u64,
    months: u64,
    /// 0 is Sunday; a 7 in the expression is folded into 0.
    days_of_week: u64,
}

impl Cron {
    /// `minute hour day-of-month month day-of-week`, each `*`, a number, a
    /// range `a-b`, a step `*/n` or `a-b/n`, or a comma list of those; or
    /// one of `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. A
    /// minute fires when every field matches it, the day-of-month and the
    /// weekday included (the scheduled-jobs store reads them the same way).
    pub(crate) fn parse(text: &str) -> Result<Cron, String> {
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
            return Err(format!(
                "`cron` has five fields (minute hour day-of-month month day-of-week) or is @hourly, @daily, @weekly, @monthly or @yearly, got {text:?}"
            ));
        };
        let mut days_of_week = parse_field(weekday, 0, 7)?;
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        let cron = Cron {
            minutes: parse_field(minute, 0, 59)?,
            hours: parse_field(hour, 0, 23)?,
            days_of_month: parse_field(monthday, 1, 31)?,
            months: parse_field(month, 1, 12)?,
            days_of_week,
        };
        if cron.next_after(0).is_none() {
            return Err(format!("`cron` {text:?} matches no date"));
        }
        Ok(cron)
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
fn parse_field(field: &str, min: u64, max: u64) -> Result<u64, String> {
    let invalid = || format!("cron field {field:?} is not values in {min}-{max}");
    let number = |text: &str| -> Result<u64, String> {
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
        if step == 0 || start < min || start > end || end > max {
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
