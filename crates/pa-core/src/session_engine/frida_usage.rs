//! The kernel `frida-*` functions' adoption counts. Each counted call sends
//! one `frida.used` host request, fire-and-forget (`{"fact": "<name>"}`; the
//! kernel never reads the reply), which this handler folds into the
//! session's `agent repl frida` counters. The request names the kind of call
//! only: never a target, a pid, a device, a script, or a path.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One counted `frida-*` call, by the kernel's `fact` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FridaFact {
    /// `frida-devices` or `frida-add-remote`.
    Devices,
    Ps,
    Kill,
    /// `frida-attach` on a running process.
    Attach,
    /// `frida-attach` with `#:spawn #t`.
    Spawn,
    ScriptLoad,
    Trace,
    Compile,
    /// A call refused because this build links no Frida.
    Unavailable,
}

/// Observes each counted call (adoption telemetry).
pub(crate) type FridaObserver = Arc<dyn Fn(FridaFact) + Send + Sync>;

/// Register `frida.used`. Without telemetry the observer is `None` and the
/// request only answers.
pub(crate) fn register_frida_usage(
    handlers: &mut HostRequestHandlers,
    observer: Option<FridaObserver>,
) {
    handlers.register(
        "frida.used",
        host_handler(move |payload| {
            let observer = observer.clone();
            async move {
                let fact = match payload.data.get("fact").and_then(Value::as_str) {
                    Some("devices") => FridaFact::Devices,
                    Some("ps") => FridaFact::Ps,
                    Some("kill") => FridaFact::Kill,
                    Some("attach") => FridaFact::Attach,
                    Some("spawn") => FridaFact::Spawn,
                    Some("script_load") => FridaFact::ScriptLoad,
                    Some("trace") => FridaFact::Trace,
                    Some("compile") => FridaFact::Compile,
                    Some("unavailable") => FridaFact::Unavailable,
                    Some(other) => anyhow::bail!("unknown frida.used fact {other:?}"),
                    None => anyhow::bail!("frida.used needs a string fact"),
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
        register_frida_usage(
            &mut handlers,
            Some(Arc::new(move |fact| sink.lock().unwrap().push(fact))),
        );
        let handler = handlers.get("frida.used").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"fact": "spawn"}),
            json!({"fact": "script_load"}),
            json!({"fact": "trace"}),
            json!({"fact": "unavailable"}),
            json!({"fact": "pid:4242"}),
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
                vec![
                    Ok(json!({})),
                    Ok(json!({})),
                    Ok(json!({})),
                    Ok(json!({})),
                    Err("unknown frida.used fact \"pid:4242\"".to_string()),
                    Err("frida.used needs a string fact".to_string()),
                ],
                vec![
                    FridaFact::Spawn,
                    FridaFact::ScriptLoad,
                    FridaFact::Trace,
                    FridaFact::Unavailable,
                ],
            )
        );
    }
}
