//! Wall-clock rendering: an epoch timestamp as the local hour and minute, or the full local
//! civil time (date, weekday, clock).

/// The local `(hour, minute)` of `epoch_ms` milliseconds since the Unix epoch, through the
/// platform's time-zone rules (`localtime_r`). `None` when the instant is out of range or
/// the platform has no binding here (Windows renders no clock).
#[cfg(unix)]
#[must_use]
#[expect(
    clippy::useless_conversion,
    reason = "time_t is i64 here but i32 on some 32-bit unix targets"
)]
pub fn local_clock(epoch_ms: i64) -> Option<(u8, u8)> {
    let seconds: libc::time_t = (epoch_ms / 1000).try_into().ok()?;
    // SAFETY: all-zero is a valid `tm` (integers and a null zone pointer).
    let mut civil: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: reads `seconds`, writes only `civil`; null means out of range.
    if unsafe { libc::localtime_r(&raw const seconds, &raw mut civil) }.is_null() {
        return None;
    }
    Some((
        u8::try_from(civil.tm_hour).ok()?,
        u8::try_from(civil.tm_min).ok()?,
    ))
}

/// The local `(hour, minute)` of `epoch_ms` (no binding on this platform).
#[cfg(not(unix))]
#[must_use]
pub fn local_clock(_epoch_ms: i64) -> Option<(u8, u8)> {
    None
}

/// One instant on the local calendar and clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCivil {
    pub year: i32,
    /// `1..=12`.
    pub month: u8,
    /// `1..=31`.
    pub day: u8,
    /// `0..=6`, Sunday first.
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
}

/// The local civil time of `epoch_ms` through the platform's time-zone rules (`localtime_r`).
/// `None` when the instant is out of range or the platform has no binding here.
#[cfg(unix)]
#[must_use]
#[expect(
    clippy::useless_conversion,
    reason = "time_t is i64 here but i32 on some 32-bit unix targets"
)]
pub fn local_civil(epoch_ms: i64) -> Option<LocalCivil> {
    let seconds: libc::time_t = epoch_ms.div_euclid(1000).try_into().ok()?;
    // SAFETY: all-zero is a valid `tm` (integers and a null zone pointer).
    let mut civil: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: reads `seconds`, writes only `civil`; null means out of range.
    if unsafe { libc::localtime_r(&raw const seconds, &raw mut civil) }.is_null() {
        return None;
    }
    Some(LocalCivil {
        year: civil.tm_year.checked_add(1900)?,
        month: u8::try_from(civil.tm_mon + 1).ok()?,
        day: u8::try_from(civil.tm_mday).ok()?,
        weekday: u8::try_from(civil.tm_wday).ok()?,
        hour: u8::try_from(civil.tm_hour).ok()?,
        minute: u8::try_from(civil.tm_min).ok()?,
    })
}

/// The local civil time of `epoch_ms` (no binding on this platform).
#[cfg(not(unix))]
#[must_use]
pub fn local_civil(_epoch_ms: i64) -> Option<LocalCivil> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// One hour later on the local clock is one hour later (the zone offset cancels out).
    #[test]
    fn the_clock_advances_with_the_timestamp() {
        let base = 1_763_300_000_000;
        let (hour, minute) = local_clock(base).unwrap();
        let (later_hour, later_minute) = local_clock(base + 3_600_000).unwrap();
        assert_eq!((later_hour, later_minute), ((hour + 1) % 24, minute));
    }

    /// The civil time agrees with the clock, and one day later is the next weekday.
    #[test]
    fn the_civil_time_matches_the_clock_and_steps_weekdays() {
        let base = 1_763_300_000_000;
        let civil = local_civil(base).unwrap();
        assert_eq!(local_clock(base), Some((civil.hour, civil.minute)));
        let next_day = local_civil(base + 86_400_000).unwrap();
        assert_eq!(next_day.weekday, (civil.weekday + 1) % 7);
    }
}
