//! The `hypotheses.record` host request: the kernel's `rlm.hypotheses` (`rlm.hypothesis`)
//! reports each finished run (how many approaches it tried, how many passed their check, and
//! whether one won) so the session can count it (adoption telemetry). The worktrees, children,
//! and checks all run in the kernel; nothing here touches them.

use std::sync::Arc;

use anyhow::Context;
use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One finished `rlm.hypotheses` run (counts only: never a task, an approach, or a diff).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HypothesesRun {
    pub(crate) approaches: u64,
    pub(crate) passed: u64,
    pub(crate) winner: bool,
}

/// Hears each reported run.
pub(crate) type HypothesesObserver = Arc<dyn Fn(HypothesesRun) + Send + Sync>;

/// Register `hypotheses.record` (a session without telemetry counts nothing).
pub(crate) fn register_hypotheses_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<HypothesesObserver>,
) {
    handlers.register(
        "hypotheses.record",
        host_handler(move |payload| {
            let observer = observer.clone();
            async move {
                let count = |key: &str| payload.data.get(key).and_then(Value::as_u64);
                let run = HypothesesRun {
                    approaches: count("approaches")
                        .context("hypotheses.record: approaches must be a count")?,
                    passed: count("passed").context("hypotheses.record: passed must be a count")?,
                    winner: payload
                        .data
                        .get("winner")
                        .and_then(Value::as_bool)
                        .context("hypotheses.record: winner must be a boolean")?,
                };
                if let Some(observer) = observer {
                    observer(run);
                }
                Ok(json!({}))
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::kernel::shared::HostRequestPayload;

    /// Each reported run reaches the observer; a malformed one is refused.
    #[tokio::test]
    async fn runs_are_counted_and_malformed_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_hypotheses_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |run| seen.lock().unwrap().push(run))
            }),
        );
        let handler = handlers.get("hypotheses.record").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"approaches": 3, "passed": 1, "winner": true}),
            json!({"approaches": 2, "passed": 0}),
        ] {
            replies.push(handler(HostRequestPayload { data }).await.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, false],
                vec![HypothesesRun {
                    approaches: 3,
                    passed: 1,
                    winner: true
                }],
            )
        );
    }
}
