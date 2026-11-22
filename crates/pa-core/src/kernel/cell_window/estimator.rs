//! The IO-free cell-duration estimator behind the learned yield window.
//!
//! Cell wall times in one project are roughly log-normal: most cells take a
//! similar time and a few take many times longer. The estimator keeps an
//! exponentially weighted mean and variance of `ln(duration)`, so the newest
//! cells dominate and a project whose cells change is followed within a few
//! cells. The window is the estimated 90th percentile times a headroom factor,
//! clamped to `[WINDOW_FLOOR, WINDOW_CEILING]`. Until `MIN_SAMPLES` cells have
//! been seen the cold window (the old fixed 60 s) applies.
//!
//! It owns no clock, file, or lock; [`super::learner`] feeds it and persists
//! its [`EstimatorState`]. That keeps it movable to a shared controller crate
//! later without changes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The window before anything is learned: the previous fixed default.
pub(crate) const COLD_WINDOW: Duration =
    Duration::from_millis(crate::settings::types::DEFAULT_CELL_TIMEOUT_MS);

/// The shortest learned window. Below about 10 s, ordinary jitter (a cold
/// import, a busy disk) would push routine cells into the background and cost
/// the model a round; a stuck cell still returns six times sooner than at 60 s.
pub(crate) const WINDOW_FLOOR: Duration = Duration::from_secs(10);

/// The longest learned window: the tool's own maximum `timeout`.
pub(crate) const WINDOW_CEILING: Duration =
    Duration::from_secs(crate::tools::kernel_cell::CELL_TIMEOUT_MAX_S);

/// Weight of the newest cell in the running mean and variance. At 0.4 a
/// cell's weight falls under 1% after ten cells (`0.6^10`), so a project that
/// changes is followed within a handful of cells. The variance term reacts in
/// one cell: a project that turns slow gets a wide window at once (a window
/// that is too long only costs a stuck cell's wait, a too-short one costs a
/// round per cell), and it narrows again over about five fast cells.
const RECENCY_WEIGHT: f64 = 0.4;

/// The window covers this many times the estimated 90th percentile, so a cell
/// a little slower than usual still ends inside the call.
const HEADROOM: f64 = 1.5;

/// The standard normal 90th-percentile z-score (P90 of a log-normal is
/// `exp(mean + Z90 * sd)` in log space).
const Z90: f64 = 1.281_551_565_545;

/// Cells seen before the learned window replaces the cold one: one or two
/// cells say little about a project, three already set its scale.
const MIN_SAMPLES: u32 = 3;

/// Cells faster than this count as this long. Their exact time cannot move a
/// window floored at 10 s, and the spread between a 5 ms cell and a 2 s one
/// would otherwise inflate the variance and keep the window wide.
const SAMPLE_FLOOR_MS: f64 = 1_000.0;

/// The estimator's whole persisted state (log-milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EstimatorState {
    /// Cells observed, saturating.
    pub(crate) samples: u32,
    /// Exponentially weighted mean of `ln(ms)`.
    pub(crate) mean_log_ms: f64,
    /// Exponentially weighted variance of `ln(ms)`.
    pub(crate) var_log_ms: f64,
}

impl EstimatorState {
    /// A state read from disk is usable only with finite, non-negative moments.
    pub(crate) fn is_valid(&self) -> bool {
        self.mean_log_ms.is_finite() && self.var_log_ms.is_finite() && self.var_log_ms >= 0.0
    }
}

/// Where a learned-policy window came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LearnedWindow {
    /// Fewer than [`MIN_SAMPLES`] cells seen: the cold window.
    Cold(Duration),
    /// From the project's recent cells.
    Learned(Duration),
}

/// The recency-weighted log-duration estimator (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct CellDurationEstimator {
    state: Option<EstimatorState>,
}

impl CellDurationEstimator {
    pub(crate) fn from_state(state: EstimatorState) -> Self {
        Self { state: Some(state) }
    }

    /// The state to persist; `None` before the first cell.
    pub(crate) fn state(&self) -> Option<EstimatorState> {
        self.state
    }

    /// Add one finished cell's wall time.
    pub(crate) fn observe(&mut self, duration: Duration) {
        let x = (duration.as_secs_f64() * 1000.0).max(SAMPLE_FLOOR_MS).ln();
        self.state = Some(match self.state {
            None => EstimatorState {
                samples: 1,
                mean_log_ms: x,
                var_log_ms: 0.0,
            },
            Some(state) => {
                // West's incremental exponentially weighted mean and variance.
                let diff = x - state.mean_log_ms;
                let step = RECENCY_WEIGHT * diff;
                EstimatorState {
                    samples: state.samples.saturating_add(1),
                    mean_log_ms: state.mean_log_ms + step,
                    var_log_ms: (1.0 - RECENCY_WEIGHT) * (state.var_log_ms + diff * step),
                }
            }
        });
    }

    /// The window the next cell gets.
    pub(crate) fn window(&self) -> LearnedWindow {
        match self.state {
            Some(state) if state.samples >= MIN_SAMPLES => {
                let p90_ms = (state.mean_log_ms + Z90 * state.var_log_ms.sqrt()).exp();
                let window = Duration::from_secs_f64(
                    (p90_ms * HEADROOM / 1000.0).min(WINDOW_CEILING.as_secs_f64()),
                );
                LearnedWindow::Learned(window.clamp(WINDOW_FLOOR, WINDOW_CEILING))
            }
            Some(_) | None => LearnedWindow::Cold(COLD_WINDOW),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project's windows after each cell, in whole seconds (`C`: cold).
    fn windows(durations_s: &[u64]) -> Vec<String> {
        let mut estimator = CellDurationEstimator::default();
        durations_s
            .iter()
            .map(|seconds| {
                estimator.observe(Duration::from_secs(*seconds));
                match estimator.window() {
                    LearnedWindow::Cold(window) => format!("C{}", window.as_secs()),
                    LearnedWindow::Learned(window) => window.as_secs().to_string(),
                }
            })
            .collect()
    }

    /// Fast cells shrink the window from the cold 60 s to the floor once the
    /// minimum sample count is reached; a project that turns slow (2-minute
    /// builds) grows it past those builds on the first slow cell, up to the
    /// ceiling; and turning fast again shrinks it back to the floor.
    #[test]
    fn the_window_follows_fast_and_slow_projects() {
        assert_eq!(
            windows(&[2, 3, 2, 2, 120, 130, 125, 2, 2, 2, 2, 2, 2]),
            ["C60", "C60", "10", "10", "202", "538", "600", "296", "105", "44", "22", "13", "10"]
        );
    }
}
