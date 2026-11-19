//! The `fs.read` and `fs.edit` host requests the kernel's `rlm.fs` client
//! sends. Each runs on a blocking thread: reading, hashing, and the atomic
//! replace touch the disk, and the async runtime never waits on them.
//!
//! Payloads carry the absolute `path` and the REPL's `cwd` (the client
//! resolves both). A stale-anchor rejection is a normal reply
//! (`applied: false`); every other failure is an error, and none writes.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use serde_json::Value;

use super::{edit, parse_edit, parse_read, read, EditFact, FsEditOperation};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Observes each finished call (adoption telemetry); never sees a path or
/// file content.
pub(crate) type EditObserver = Arc<dyn Fn(EditFact) + Send + Sync>;

/// One session's `fs.read`/`fs.edit` handler wiring.
pub(crate) struct FsEditHost {
    pub(crate) observer: Option<EditObserver>,
}

impl FsEditHost {
    /// Register the `fs.read` and `fs.edit` handlers.
    pub(crate) fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        let handler = |operation: FsEditOperation| {
            let host = Arc::clone(&host);
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(operation, payload.data).await }
            })
        };
        handlers.register("fs.read", handler(FsEditOperation::Read));
        handlers.register("fs.edit", handler(FsEditOperation::Edit));
    }

    #[tracing::instrument(skip_all, fields(operation = ?operation))]
    async fn handle(&self, operation: FsEditOperation, data: Value) -> anyhow::Result<Value> {
        let outcome = tokio::task::spawn_blocking(move || run(operation, &data))
            .await
            .context("fs edit thread")?;
        let fact = match &outcome {
            Ok((_, fact)) => *fact,
            Err(_) => EditFact::Failed { operation },
        };
        if let Some(observer) = &self.observer {
            observer(fact);
        }
        let label = match operation {
            FsEditOperation::Read => "fs.read",
            FsEditOperation::Edit => "fs.edit",
        };
        outcome
            .map(|(reply, _)| reply)
            .map_err(|error| anyhow!("{label}: {error:#}"))
    }
}

/// Parse and run one call: the reply and its telemetry fact.
fn run(operation: FsEditOperation, data: &Value) -> anyhow::Result<(Value, EditFact)> {
    match operation {
        FsEditOperation::Read => {
            let (reply, lines) = read(&parse_read(data)?)?;
            Ok((reply, EditFact::Read { lines }))
        }
        FsEditOperation::Edit => edit(&parse_edit(data)?),
    }
}
