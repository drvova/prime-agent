//! The `command_cache.fact` host request: the kernel's read-only command
//! cache (`rlm.command_cache`) reports each lookup of an allowlisted `bash()`
//! command or an `fs.read`, hit or miss, so the session can count it
//! (adoption telemetry). The kernel sends it without awaiting the reply.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One cache lookup (a count: never a command, a path, or output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheLookup {
    /// A `fs.read` (else an allowlisted `bash()` command).
    pub(crate) file_read: bool,
    pub(crate) hit: bool,
}

/// Hears each reported lookup.
pub(crate) type CacheObserver = Arc<dyn Fn(CacheLookup) + Send + Sync>;

/// Register `command_cache.fact` (a session without telemetry counts nothing).
pub(crate) fn register_command_cache_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<CacheObserver>,
) {
    handlers.register(
        "command_cache.fact",
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let file_read = match payload.data.get("source").and_then(Value::as_str) {
                    Some("bash") => false,
                    Some("fs.read") => true,
                    other => anyhow::bail!("command_cache.fact: unknown source {other:?}"),
                };
                let hit = payload
                    .data
                    .get("hit")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow::anyhow!("command_cache.fact: hit must be a boolean"))?;
                if let Some(observer) = observer {
                    observer(CacheLookup { file_read, hit });
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

    /// Each reported lookup reaches the observer; a malformed one is refused.
    #[tokio::test]
    async fn lookups_are_counted_and_malformed_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_command_cache_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |lookup| seen.lock().unwrap().push(lookup))
            }),
        );
        let handler = handlers.get("command_cache.fact").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"source": "bash", "hit": true}),
            json!({"source": "fs.read", "hit": false}),
            json!({"source": "curl", "hit": true}),
            json!({"source": "bash"}),
        ] {
            replies.push(handler(HostRequestPayload { data }).await.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, true, false, false],
                vec![
                    CacheLookup {
                        file_read: false,
                        hit: true
                    },
                    CacheLookup {
                        file_read: true,
                        hit: false
                    },
                ],
            )
        );
    }
}
