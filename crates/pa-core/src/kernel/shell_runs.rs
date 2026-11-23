//! The `shell.fact` host request: each command the kernel ran on the
//! built-in shell reports how many utilities it ran inside the process and
//! how many programs it started, so the session can count them (adoption
//! telemetry). The kernel sends it without awaiting the reply.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One command on the built-in shell (counts: never a command or output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShellRun {
    /// Utilities run inside the process.
    pub(crate) in_process: u64,
    /// Programs started as processes.
    pub(crate) spawned: u64,
    /// Utilities that handed their command to the program of the same name.
    pub(crate) handed_over: u64,
    /// Utilities that could do neither: the machine has no such program.
    pub(crate) unavailable: u64,
}

/// Hears each reported command.
pub(crate) type ShellObserver = Arc<dyn Fn(ShellRun) + Send + Sync>;

/// Register `shell.fact` (a session without telemetry counts nothing).
pub(crate) fn register_shell_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<ShellObserver>,
) {
    handlers.register(
        "shell.fact",
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let count = |name: &str| {
                    payload
                        .data
                        .get(name)
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("shell.fact: {name} must be a count"))
                };
                let run = ShellRun {
                    in_process: count("inProcess")?,
                    spawned: count("spawned")?,
                    handed_over: count("handedOver")?,
                    unavailable: count("unavailable")?,
                };
                if let Some(observer) = observer {
                    observer(run);
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

    /// Each reported command reaches the observer; a malformed one is refused.
    #[tokio::test]
    async fn commands_are_counted_and_malformed_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_shell_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |run| seen.lock().unwrap().push(run))
            }),
        );
        let handler = handlers.get("shell.fact").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"inProcess": 3, "spawned": 2, "handedOver": 1, "unavailable": 0}),
            json!({"inProcess": 3, "spawned": "two", "handedOver": 1, "unavailable": 0}),
            json!({"inProcess": 3}),
        ] {
            replies.push(handler(HostRequestPayload { data }).await.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, false, false],
                vec![ShellRun {
                    in_process: 3,
                    spawned: 2,
                    handed_over: 1,
                    unavailable: 0,
                }],
            )
        );
    }
}
