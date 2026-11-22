//! The `fs.read`, `fs.edit`, and `fs.write` host requests the kernel's file
//! functions send. Each runs on a blocking thread: reading, hashing, and the atomic
//! replace touch the disk, and the async runtime never waits on them.
//!
//! Payloads carry the absolute `path` and the REPL's `cwd` (the client
//! resolves both). A stale-anchor rejection is a normal reply
//! (`applied: false`); every other failure is an error, and none writes.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use serde_json::Value;

use super::seen::SeenFiles;
use super::write::{parse_write, write};
use super::{edit, parse_edit, parse_read, read, EditFact, FsEditOperation, Rewrite};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Observes each finished call (adoption telemetry); never sees a path or
/// file content.
pub(crate) type EditObserver = Arc<dyn Fn(EditFact) + Send + Sync>;

/// One session's `fs.read`/`fs.edit`/`fs.write` handler wiring.
pub(crate) struct FsEditHost {
    pub(crate) observer: Option<EditObserver>,
    /// The session's seen files: whole-file re-reads shrink to "unchanged" or a diff. `None`
    /// shows every read whole.
    pub(crate) seen: Option<Arc<SeenFiles>>,
}

impl FsEditHost {
    /// Register the `fs.read`, `fs.edit`, and `fs.write` handlers.
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
        handlers.register("fs.write", handler(FsEditOperation::Write));
    }

    #[tracing::instrument(skip_all, fields(operation = ?operation))]
    async fn handle(&self, operation: FsEditOperation, data: Value) -> anyhow::Result<Value> {
        let path = data
            .get("path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from);
        let mut outcome = tokio::task::spawn_blocking(move || run(operation, &data))
            .await
            .context("fs edit thread")?;
        if let (Some(seen), Some(path), Ok((reply, _, rewrite))) =
            (&self.seen, path, outcome.as_mut())
        {
            match (operation, rewrite.take()) {
                (FsEditOperation::Read, _) => seen.shape_read(&path, reply).await,
                (
                    FsEditOperation::Edit | FsEditOperation::Write,
                    Some(Rewrite { before, after }),
                ) => {
                    seen.note_edit(&path, &before, after).await;
                }
                (FsEditOperation::Edit | FsEditOperation::Write, None) => {}
            }
        }
        let fact = match &outcome {
            Ok((_, fact, _)) => *fact,
            Err(_) => EditFact::Failed { operation },
        };
        if let Some(observer) = &self.observer {
            observer(fact);
        }
        let label = match operation {
            FsEditOperation::Read => "fs.read",
            FsEditOperation::Edit => "fs.edit",
            FsEditOperation::Write => "fs.write",
        };
        outcome
            .map(|(reply, _, _)| reply)
            .map_err(|error| anyhow!("{label}: {error:#}"))
    }
}

/// Parse and run one call: the reply, its telemetry fact, and an applied edit's rewrite.
fn run(
    operation: FsEditOperation,
    data: &Value,
) -> anyhow::Result<(Value, EditFact, Option<Rewrite>)> {
    match operation {
        FsEditOperation::Read => {
            let (reply, lines) = read(&parse_read(data)?)?;
            Ok((reply, EditFact::Read { lines }, None))
        }
        FsEditOperation::Edit => edit(&parse_edit(data)?),
        FsEditOperation::Write => write(&parse_write(data)?),
    }
}
