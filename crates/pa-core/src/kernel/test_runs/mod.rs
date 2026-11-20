//! The `tests.summary` host request: the kernel parsed an awaited test run
//! (`rlm.test_results`) and asks the host to keep its whole output and each
//! failure's section in the session's spill store, so the typed summary can
//! name a handle for each instead of carrying the text. The host also counts
//! the run (adoption telemetry). An ephemeral session has no store: every
//! handle answers `null` and the summary goes out without them.

#[cfg(test)]
mod tests;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};

/// One summarized test run (adoption counts only: never a name, a path, or output).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TestRunFact {
    /// Failed tests the summary counted.
    pub(crate) failed: u64,
    /// Failures that got their own section (and handle).
    pub(crate) failures: usize,
    /// The whole output got a handle.
    pub(crate) stored: bool,
}

/// Hears each summarized test run.
pub(crate) type TestRunObserver = std::sync::Arc<dyn Fn(TestRunFact) + Send + Sync>;

/// Register `tests.summary` over the session's store (`None`: no handles).
pub(crate) fn register_test_summary_handler(
    handlers: &mut HostRequestHandlers,
    store: Option<&SpillStore>,
    observer: Option<TestRunObserver>,
) {
    let store = store.cloned();
    handlers.register(
        "tests.summary",
        host_handler(move |payload| {
            let store = store.clone();
            let observer = observer.clone();
            Box::pin(async move {
                let data = &payload.data;
                let output = data
                    .get("output")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("tests.summary: output must be a string"))?;
                let failures: Vec<&str> = data
                    .get("failures")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("tests.summary: failures must be a list"))?
                    .iter()
                    .map(|failure| failure.as_str().unwrap_or_default())
                    .collect();
                // Only queues each write: the reply never waits on disk.
                let keep = |text: &str| {
                    store.as_ref().filter(|_| !text.is_empty()).map(|store| {
                        format!(
                            "{HANDLE_PREFIX}{}",
                            store.spill(SpillCopy::of_text(text)).id
                        )
                    })
                };
                let output_handle = keep(output);
                let failure_handles: Vec<Option<String>> =
                    failures.iter().map(|failure| keep(failure)).collect();
                if let Some(observer) = observer {
                    observer(TestRunFact {
                        failed: data
                            .get("failed")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                        failures: failures.len(),
                        stored: output_handle.is_some(),
                    });
                }
                Ok(json!({ "output": output_handle, "failures": failure_handles }))
            })
        }),
    );
}
