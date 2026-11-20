//! The `edit_check.fact` host request: the kernel's edit-then-check
//! (`rlm.edit_check`) reports how each check after an edit went, so the
//! session can count it (adoption telemetry). The kernel sends it without
//! awaiting the reply; the check itself never waits on the host.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// How one check after an edit went (a count: never a path, a command, or output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditCheckOutcome {
    /// Ran and found nothing.
    Clean,
    /// Ran and found diagnostics.
    Found,
    /// Outlived the edit's wait; its end arrives as its own fact.
    Late,
    /// Dropped: its learned cost exceeds its benefit.
    Skipped,
    /// Could not start, or ran past its hard limit.
    Failed,
}

/// Hears each reported check outcome.
pub(crate) type EditCheckObserver = Arc<dyn Fn(EditCheckOutcome) + Send + Sync>;

/// Register `edit_check.fact` (a session without telemetry answers and counts nothing).
pub(crate) fn register_edit_check_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<EditCheckObserver>,
) {
    handlers.register(
        "edit_check.fact",
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let outcome = match payload.data.get("outcome").and_then(Value::as_str) {
                    Some("clean") => EditCheckOutcome::Clean,
                    Some("found") => EditCheckOutcome::Found,
                    Some("late") => EditCheckOutcome::Late,
                    Some("skipped") => EditCheckOutcome::Skipped,
                    Some("failed") => EditCheckOutcome::Failed,
                    other => anyhow::bail!("edit_check.fact: unknown outcome {other:?}"),
                };
                if let Some(observer) = observer {
                    observer(outcome);
                }
                Ok(json!({}))
            })
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::kernel::shared::HostRequestPayload;

    /// Each reported outcome reaches the observer; an unknown one is refused.
    #[tokio::test]
    async fn reported_outcomes_are_counted_and_unknown_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_edit_check_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |outcome| seen.lock().unwrap().push(outcome))
            }),
        );
        let handler = handlers.get("edit_check.fact").expect("registered");
        let mut replies = Vec::new();
        for outcome in ["late", "found", "skipped", "clean", "failed", "exploded"] {
            let reply = handler(HostRequestPayload {
                data: json!({"outcome": outcome, "kind": "cargo"}),
            })
            .await;
            replies.push(reply.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, true, true, true, true, false],
                vec![
                    EditCheckOutcome::Late,
                    EditCheckOutcome::Found,
                    EditCheckOutcome::Skipped,
                    EditCheckOutcome::Clean,
                    EditCheckOutcome::Failed,
                ],
            )
        );
    }
}
