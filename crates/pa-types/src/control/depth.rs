//! The adaptive recursion bound for delegation: how many levels of subagents a session may spawn
//! below itself when no setting fixes the bound.
//!
//! It is the same shape as the capacity [`super::Controller`], on a coarser resource: delegated
//! work that comes back checked and passing is headroom, so the bound rises one level after
//! [`DEPTH_RAISE_STREAK`] such results in a row; a failed or timed-out child is the throttle, so
//! the bound drops one level at once. It never leaves [`DEPTH_FLOOR`]..=[`DEPTH_CEILING`]. Pure:
//! nothing here locks, spawns, or reads a clock.

/// The fewest levels a session may always delegate.
pub const DEPTH_FLOOR: u32 = 1;
/// The most levels the bound grows to: past it, a delegation tree's briefs and results are
/// too far from the session that asked for them to stay checkable.
pub const DEPTH_CEILING: u32 = 4;
/// Where the bound starts (the workspace default before it adapted).
pub const DEPTH_INITIAL: u32 = 2;
/// Checked, passing child results in a row that raise the bound one level.
pub const DEPTH_RAISE_STREAK: u32 = 2;

/// How one delegated child's work came back, as the depth bound sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationOutcome {
    /// It finished and its acceptance checks passed.
    CheckedPassing,
    /// It failed, or its acceptance checks failed.
    Failed,
    /// A bounded wait for it ran out before it finished.
    TimedOut,
}

/// What one observation did to the bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthChange {
    Raised,
    Lowered,
    Unchanged,
}

/// The adaptive bound: the current number of levels and the passing streak toward the next
/// raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthController {
    max_depth: u32,
    streak: u32,
}

impl Default for DepthController {
    fn default() -> Self {
        Self::new()
    }
}

impl DepthController {
    /// A bound at [`DEPTH_INITIAL`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_depth: DEPTH_INITIAL,
            streak: 0,
        }
    }

    /// The levels a session may delegate now.
    #[must_use]
    pub const fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// Fold one child's outcome into the bound.
    pub fn observe(&mut self, outcome: DelegationOutcome) -> DepthChange {
        let before = self.max_depth;
        match outcome {
            DelegationOutcome::CheckedPassing => {
                self.streak += 1;
                if self.streak >= DEPTH_RAISE_STREAK {
                    self.streak = 0;
                    self.max_depth = (self.max_depth + 1).min(DEPTH_CEILING);
                }
            }
            DelegationOutcome::Failed | DelegationOutcome::TimedOut => {
                self.streak = 0;
                self.max_depth = self.max_depth.saturating_sub(1).max(DEPTH_FLOOR);
            }
        }
        match self.max_depth.cmp(&before) {
            std::cmp::Ordering::Greater => DepthChange::Raised,
            std::cmp::Ordering::Less => DepthChange::Lowered,
            std::cmp::Ordering::Equal => DepthChange::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_streaks_raise_and_failures_lower_inside_the_floor_and_ceiling() {
        use DelegationOutcome::{CheckedPassing, Failed, TimedOut};
        let mut controller = DepthController::new();
        let trace: Vec<(DepthChange, u32)> = [
            CheckedPassing,
            CheckedPassing,
            CheckedPassing,
            Failed,
            CheckedPassing,
            CheckedPassing,
            CheckedPassing,
            CheckedPassing,
            CheckedPassing,
            CheckedPassing,
            TimedOut,
            Failed,
            Failed,
            Failed,
        ]
        .into_iter()
        .map(|outcome| (controller.observe(outcome), controller.max_depth()))
        .collect();
        assert_eq!(
            trace,
            vec![
                (DepthChange::Unchanged, 2),
                (DepthChange::Raised, 3),
                (DepthChange::Unchanged, 3),
                // A failure resets the streak as well as the level.
                (DepthChange::Lowered, 2),
                (DepthChange::Unchanged, 2),
                (DepthChange::Raised, 3),
                (DepthChange::Unchanged, 3),
                (DepthChange::Raised, 4),
                (DepthChange::Unchanged, 4),
                (DepthChange::Unchanged, 4),
                (DepthChange::Lowered, 3),
                (DepthChange::Lowered, 2),
                (DepthChange::Lowered, 1),
                (DepthChange::Unchanged, 1),
            ]
        );
    }
}
