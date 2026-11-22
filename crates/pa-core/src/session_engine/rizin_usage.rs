//! The kernel `rz-*` functions' adoption counts. Each counted call sends one
//! `rizin.used` host request, fire-and-forget (`{"fact": "<name>"}`; the
//! kernel never reads the reply), which this handler folds into the
//! session's `agent repl rizin` counters. The request names the kind of call
//! only: never a target, a path, a command, or output.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One counted `rz-*` call, by the kernel's `fact` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RizinFact {
    /// `rz-open` without the debugger.
    Open,
    /// `rz-open` with `#:debug #t`.
    DebugOpen,
    /// `rz-cmd` that got its reply.
    Command,
    /// `rz-cmdj` that got its reply.
    JsonCommand,
    /// A command interrupted by its time limit or its cell's stop.
    Interrupted,
    /// A command that found its rizin dead, or whose rizin was killed
    /// because it did not answer an interrupt.
    Died,
    /// An `rz-open` refused because no `rizin` is on the session's `PATH`.
    NotInstalled,
}

/// Observes each counted call (adoption telemetry).
pub(crate) type RizinObserver = Arc<dyn Fn(RizinFact) + Send + Sync>;

/// Register `rizin.used`. Without telemetry the observer is `None` and the
/// request only answers.
pub(crate) fn register_rizin_usage(
    handlers: &mut HostRequestHandlers,
    observer: Option<RizinObserver>,
) {
    handlers.register(
        "rizin.used",
        host_handler(move |payload| {
            let observer = observer.clone();
            async move {
                let fact = match payload.data.get("fact").and_then(Value::as_str) {
                    Some("open") => RizinFact::Open,
                    Some("debug_open") => RizinFact::DebugOpen,
                    Some("command") => RizinFact::Command,
                    Some("json_command") => RizinFact::JsonCommand,
                    Some("interrupted") => RizinFact::Interrupted,
                    Some("died") => RizinFact::Died,
                    Some("not_installed") => RizinFact::NotInstalled,
                    Some(other) => anyhow::bail!("unknown rizin.used fact {other:?}"),
                    None => anyhow::bail!("rizin.used needs a string fact"),
                };
                if let Some(observer) = &observer {
                    observer(fact);
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

    /// Every fact the kernel sends reaches the observer; an unknown or
    /// missing fact is refused and counts nothing.
    #[tokio::test]
    async fn each_fact_reaches_the_observer_and_unknown_facts_are_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        let sink = Arc::clone(&seen);
        register_rizin_usage(
            &mut handlers,
            Some(Arc::new(move |fact| sink.lock().unwrap().push(fact))),
        );
        let handler = handlers.get("rizin.used").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"fact": "open"}),
            json!({"fact": "debug_open"}),
            json!({"fact": "command"}),
            json!({"fact": "json_command"}),
            json!({"fact": "interrupted"}),
            json!({"fact": "died"}),
            json!({"fact": "not_installed"}),
            json!({"fact": "/bin/ls"}),
            json!({}),
        ] {
            replies.push(
                handler(HostRequestPayload { data })
                    .await
                    .map_err(|error| error.to_string()),
            );
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                [
                    vec![Ok(json!({})); 7],
                    vec![
                        Err("unknown rizin.used fact \"/bin/ls\"".to_string()),
                        Err("rizin.used needs a string fact".to_string()),
                    ],
                ]
                .concat(),
                vec![
                    RizinFact::Open,
                    RizinFact::DebugOpen,
                    RizinFact::Command,
                    RizinFact::JsonCommand,
                    RizinFact::Interrupted,
                    RizinFact::Died,
                    RizinFact::NotInstalled,
                ],
            )
        );
    }
}
