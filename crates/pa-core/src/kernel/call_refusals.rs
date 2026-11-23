//! The `call.refused` host request: the kernel reports each call it refused
//! before the function ran, by what the call got wrong, so the session can
//! count them (adoption telemetry). The kernel sends it without awaiting the
//! reply.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// What a refused call got wrong (a class: never a function, an option's
/// name, or a value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallRefusal {
    /// An option the function does not declare.
    UnknownOption,
    /// The same option given twice.
    RepeatedOption,
    /// An option with no value after it.
    OptionWithoutValue,
    /// Too few or too many arguments.
    Arity,
    /// An argument an MCP tool's schema does not declare.
    ToolArgument,
}

impl CallRefusal {
    /// Every class, as the request spells them.
    pub(crate) const ALL: [CallRefusal; 5] = [
        CallRefusal::UnknownOption,
        CallRefusal::RepeatedOption,
        CallRefusal::OptionWithoutValue,
        CallRefusal::Arity,
        CallRefusal::ToolArgument,
    ];

    /// The class as `call.refused` carries it.
    pub(crate) fn class(self) -> &'static str {
        match self {
            CallRefusal::UnknownOption => "unknown_option",
            CallRefusal::RepeatedOption => "repeated_option",
            CallRefusal::OptionWithoutValue => "option_without_value",
            CallRefusal::Arity => "arity",
            CallRefusal::ToolArgument => "tool_argument",
        }
    }
}

/// Hears each reported refusal.
pub(crate) type RefusalObserver = Arc<dyn Fn(CallRefusal) + Send + Sync>;

/// Register `call.refused` (a session without telemetry counts nothing).
pub(crate) fn register_call_refusal_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<RefusalObserver>,
) {
    handlers.register(
        "call.refused",
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let class = payload.data.get("class").and_then(Value::as_str);
                let refusal = CallRefusal::ALL
                    .into_iter()
                    .find(|refusal| Some(refusal.class()) == class)
                    .ok_or_else(|| anyhow::anyhow!("call.refused: unknown class {class:?}"))?;
                if let Some(observer) = observer {
                    observer(refusal);
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

    /// Each reported refusal reaches the observer; a malformed one is refused.
    #[tokio::test]
    async fn refusals_are_counted_and_malformed_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_call_refusal_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |refusal| seen.lock().unwrap().push(refusal))
            }),
        );
        let handler = handlers.get("call.refused").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"class": "unknown_option"}),
            json!({"class": "tool_argument"}),
            json!({"class": "bash"}),
            json!({}),
        ] {
            replies.push(handler(HostRequestPayload { data }).await.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, true, false, false],
                vec![CallRefusal::UnknownOption, CallRefusal::ToolArgument],
            )
        );
    }
}
