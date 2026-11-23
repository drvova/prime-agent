//! Elapsed time as the compact text the panels share, so a duration reads the same in the bash
//! view, the factory view and the goal panel: two units at most, the larger unit rolling over
//! (`45s`, `2m 05s`, `1h 07m`, `2d 03h`), with sub-minute precision where the source has it.

/// Whole seconds: `45s`, `2m 05s`, `1h 07m`, `2d 03h`.
#[must_use]
pub(crate) fn seconds(total: u64) -> String {
    let minutes = total / 60;
    let hours = minutes / 60;
    if minutes == 0 {
        format!("{total}s")
    } else if hours == 0 {
        format!("{minutes}m {:02}s", total % 60)
    } else if hours < 24 {
        format!("{hours}h {:02}m", minutes % 60)
    } else {
        format!("{}d {:02}h", hours / 24, hours % 24)
    }
}

/// Milliseconds: `780ms` under a second, tenths under a minute (`3.4s`), then as [`seconds`].
#[must_use]
pub(crate) fn millis(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1_000, (ms % 1_000) / 100)
    } else {
        seconds(ms / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each unit rolls over into the next: no `71m 05s`, no `25h 00m`.
    #[test]
    fn durations_roll_over_into_the_larger_unit() {
        assert_eq!(
            [
                millis(780),
                millis(3_412),
                millis(59_999),
                millis(125_000),
                seconds(45),
                seconds(4_265),
                seconds(86_399),
                seconds(183_600),
            ],
            ["780ms", "3.4s", "59.9s", "2m 05s", "45s", "1h 11m", "23h 59m", "2d 03h"]
                .map(str::to_string)
        );
    }
}
