//! One session's delegation depth bound: a fixed bound (a setting, a chat override, or the
//! parent's bound), or, when nothing fixes it, the adaptive [`DepthController`] fed by how this
//! session's children come back from `rlm.collect`.
//!
//! The spawn check and a child's inherited bound read [`DelegationDepth::max_depth`]; the
//! kernel's collect handler feeds every result through [`DelegationDepth::observe`]; brief-mode
//! `better` scales a brief's depth with [`DelegationDepth::levels_below_child`].

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use pa_types::control::{DelegationOutcome, DepthChange, DepthController};

use super::rlm_host::RlmChildResult;

/// Where a session's bound comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthBound {
    /// A setting, a chat override, or the parent's bound fixes it.
    Fixed(u32),
    /// Nothing fixes it: it adapts to how delegated work comes back.
    Adaptive,
}

impl DepthBound {
    /// The bound a resolution (`resolve_rlm_max_depth`) gives: its `default` source adapts.
    #[must_use]
    pub fn resolved(max_depth: u64, source: &str) -> Self {
        match source {
            "default" => Self::Adaptive,
            _ => Self::Fixed(u32::try_from(max_depth).unwrap_or(u32::MAX)),
        }
    }
}

/// Whether a collect waited for its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectWait {
    /// `timeout_ms=0`: a snapshot; an unsettled child has not timed out.
    Snapshot,
    /// A bounded wait: a child still unsettled at its end timed out.
    Bounded,
}

#[derive(Debug)]
struct State {
    depth: u32,
    bound: DepthBound,
    controller: DepthController,
    /// Children whose settled outcome was folded in (each counts once).
    settled: HashSet<String>,
    /// Children whose timeout was folded in (each counts once).
    timed_out: HashSet<String>,
}

/// One session's bound (see the module docs).
#[derive(Debug)]
pub struct DelegationDepth {
    state: Mutex<State>,
}

impl Default for DelegationDepth {
    fn default() -> Self {
        Self::new(0, DepthBound::Adaptive)
    }
}

impl DelegationDepth {
    /// A session at `depth` with `bound`.
    #[must_use]
    pub fn new(depth: u32, bound: DepthBound) -> Self {
        Self {
            state: Mutex::new(State {
                depth,
                bound,
                controller: DepthController::new(),
                settled: HashSet::new(),
                timed_out: HashSet::new(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Re-seat the session's depth and bound (its identity arrived, or a chat override set it).
    pub fn configure(&self, depth: u32, bound: DepthBound) {
        let mut state = self.state();
        state.depth = depth;
        state.bound = bound;
    }

    /// Fix the bound (a chat override), keeping the session's depth.
    pub fn fix(&self, max_depth: u32) {
        self.state().bound = DepthBound::Fixed(max_depth);
    }

    /// The bound: how deep this session's subtree may reach.
    #[must_use]
    pub fn max_depth(&self) -> u32 {
        let state = self.state();
        match state.bound {
            DepthBound::Fixed(max_depth) => max_depth,
            DepthBound::Adaptive => state.controller.max_depth(),
        }
    }

    /// The levels a child spawned now may still delegate below itself.
    #[must_use]
    pub fn levels_below_child(&self) -> u32 {
        let depth = self.state().depth;
        self.max_depth().saturating_sub(depth + 1)
    }

    /// Fold one collect's results into an adaptive bound; what each folded outcome did.
    #[must_use]
    pub fn observe(&self, results: &[RlmChildResult], wait: CollectWait) -> Vec<DepthChange> {
        let mut state = self.state();
        match state.bound {
            DepthBound::Fixed(_) => return Vec::new(),
            DepthBound::Adaptive => {}
        }
        let mut changes = Vec::new();
        for result in results {
            let outcome = match (result.settled, wait) {
                (true, _) if state.settled.contains(&result.rlm_child_id) => None,
                (true, _) => match (result.status, result.check_status) {
                    ("done", Some("passed")) => Some(DelegationOutcome::CheckedPassing),
                    ("error", _) | (_, Some("failed")) => Some(DelegationOutcome::Failed),
                    // Unchecked or still-pending work says nothing yet; a later collect reads it
                    // again. A cancel is the parent's own decision.
                    _ => None,
                },
                (false, CollectWait::Bounded)
                    if !state.timed_out.contains(&result.rlm_child_id) =>
                {
                    Some(DelegationOutcome::TimedOut)
                }
                (false, CollectWait::Bounded | CollectWait::Snapshot) => None,
            };
            let Some(outcome) = outcome else {
                continue;
            };
            let id = result.rlm_child_id.clone();
            match outcome {
                DelegationOutcome::TimedOut => state.timed_out.insert(id),
                DelegationOutcome::CheckedPassing | DelegationOutcome::Failed => {
                    state.settled.insert(id)
                }
            };
            changes.push(state.controller.observe(outcome));
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        id: &str,
        status: &'static str,
        check_status: Option<&'static str>,
    ) -> RlmChildResult {
        RlmChildResult {
            rlm_child_id: id.to_string(),
            session_name: None,
            session_dir: None,
            status,
            settled: status != "running",
            waiting_on_you: false,
            answer_preview: None,
            answer_text: None,
            error: None,
            duration_ms: None,
            tool_use_count: None,
            replied_since_task: None,
            queued_ms: None,
            check_status,
        }
    }

    #[test]
    fn checked_passing_children_raise_the_bound_and_failures_and_timeouts_lower_it_once_each() {
        let depth = DelegationDepth::new(0, DepthBound::Adaptive);
        let passing = [
            result("a", "done", Some("passed")),
            result("b", "done", Some("passed")),
            // Unchecked and pending work, and a cancel, say nothing.
            result("c", "done", Some("none")),
            result("d", "done", Some("pending")),
            result("e", "cancelled", None),
        ];
        let raised = depth.observe(&passing, CollectWait::Snapshot);
        // The same settled children read again count once.
        let again = depth.observe(&passing, CollectWait::Snapshot);
        let snapshot = depth.observe(&[result("f", "running", None)], CollectWait::Snapshot);
        let lowered = depth.observe(
            &[
                result("f", "running", None),
                result("g", "done", Some("failed")),
                result("h", "error", None),
            ],
            CollectWait::Bounded,
        );
        assert_eq!(
            (raised, again, snapshot, lowered, depth.max_depth()),
            (
                vec![DepthChange::Unchanged, DepthChange::Raised],
                Vec::new(),
                Vec::new(),
                vec![
                    DepthChange::Lowered,
                    DepthChange::Lowered,
                    DepthChange::Unchanged
                ],
                1
            )
        );
        // A child at depth 0's bound 1 can delegate nothing below itself.
        assert_eq!(depth.levels_below_child(), 0);
    }

    #[test]
    fn a_fixed_bound_never_adapts() {
        let depth = DelegationDepth::new(1, DepthBound::resolved(3, "global"));
        let changes = depth.observe(
            &[result("a", "error", None), result("b", "error", None)],
            CollectWait::Snapshot,
        );
        assert_eq!(
            (changes, depth.max_depth(), depth.levels_below_child()),
            (Vec::new(), 3, 1)
        );
    }
}
