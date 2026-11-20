//! The idle-eviction window under memory pressure: a session worker nobody is attached to
//! passivates after `idleEvictionMinutes` of idleness. A user-set value always wins; unset, the
//! window is the default 90 minutes while memory is plentiful and shortens as available memory
//! runs low, so idle workers give their memory back when the machine needs it.

use pa_core::settings::{IdleEviction, DEFAULT_IDLE_EVICTION_MINUTES};

/// How often a parked worker under the adaptive window re-reads memory pressure: a window
/// armed while memory was plentiful must still shorten when it runs low.
pub(crate) const ADAPTIVE_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Who set the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThresholdSource {
    /// `idleEvictionMinutes`.
    User,
    /// The default, adapted to memory pressure.
    MemoryPressure,
}

/// The idle-eviction window in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvictionThreshold {
    pub(crate) minutes: u64,
    pub(crate) source: ThresholdSource,
}

/// The adaptive window for a host with `available_pct` percent of its memory available
/// (`None`: the platform does not say, the default holds).
pub(crate) fn minutes_for_memory(available_pct: Option<u32>) -> u64 {
    match available_pct {
        Some(0..10) => 5,
        Some(10..20) => 15,
        Some(20..30) => 45,
        Some(_) | None => DEFAULT_IDLE_EVICTION_MINUTES,
    }
}

/// Available memory as a share of the total, where the platform exposes it without a
/// blocking call (Linux's in-memory `/proc/meminfo`).
fn available_memory_pct() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |name: &str| -> Option<u64> {
            meminfo
                .lines()
                .find_map(|line| line.strip_prefix(name))?
                .trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let total = field("MemTotal").filter(|total| *total > 0)?;
        let available = field("MemAvailable")?;
        u32::try_from(available.saturating_mul(100) / total).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The window `setting` puts in force now (`None`: eviction is off).
pub(crate) fn eviction_threshold(setting: IdleEviction) -> Option<EvictionThreshold> {
    match setting {
        IdleEviction::Off => None,
        IdleEviction::Minutes(minutes) => Some(EvictionThreshold {
            minutes,
            source: ThresholdSource::User,
        }),
        IdleEviction::Adaptive => Some(EvictionThreshold {
            minutes: minutes_for_memory(available_memory_pct()),
            source: ThresholdSource::MemoryPressure,
        }),
    }
}

/// Whether a worker's ask, made against the window it read (`reported`), still holds against
/// the window in force now: a user window must match exactly (a stale ask against a raised
/// threshold is refused); an adaptive one holds when it was no shorter than the current one
/// (pressure that eased since the ask refuses it, pressure that grew does not).
pub(crate) fn ask_holds(reported: u64, current: EvictionThreshold) -> bool {
    match current.source {
        ThresholdSource::User => reported == current.minutes,
        ThresholdSource::MemoryPressure => reported >= current.minutes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adaptive_window_shortens_as_memory_runs_low_and_a_user_value_wins() {
        assert_eq!(
            [None, Some(80), Some(30), Some(25), Some(12), Some(3)].map(minutes_for_memory),
            [90, 90, 90, 45, 15, 5]
        );
        assert_eq!(
            [
                eviction_threshold(IdleEviction::Minutes(240)),
                eviction_threshold(IdleEviction::Off),
            ],
            [
                Some(EvictionThreshold {
                    minutes: 240,
                    source: ThresholdSource::User,
                }),
                None,
            ]
        );
    }

    #[test]
    fn a_stale_ask_is_refused_unless_pressure_only_grew() {
        let user = EvictionThreshold {
            minutes: 90,
            source: ThresholdSource::User,
        };
        let adaptive = EvictionThreshold {
            minutes: 15,
            source: ThresholdSource::MemoryPressure,
        };
        assert_eq!(
            [
                ask_holds(90, user),
                ask_holds(45, user),
                ask_holds(45, adaptive),
                ask_holds(15, adaptive),
                ask_holds(5, adaptive),
            ],
            [true, false, true, true, false]
        );
    }
}
