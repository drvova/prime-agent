//! Wall-clock rendering: an epoch timestamp as the local hour and minute.

/// The local `(hour, minute)` of `epoch_ms` milliseconds since the Unix epoch, through the
/// platform's time-zone rules (`localtime_r`). `None` when the instant is out of range or
/// the platform has no binding here (Windows renders no clock).
#[cfg(unix)]
#[must_use]
#[allow(
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
}
