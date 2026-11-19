//! The `fs.grep`, `fs.glob` and `fs.find` host requests the kernel's
//! `rlm.fs` client sends. Each runs on a blocking thread: a search walks the
//! disk, and the async runtime (and every session it serves) never waits on it.
//!
//! Payloads carry the absolute `root` and the REPL's `cwd` (the client
//! resolves both); replies are the reports' `to_json` shapes.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use serde_json::Value;

use super::{find, glob, grep, parse_find, parse_glob, parse_grep, FsOperation, SearchFact};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Observes each finished call (adoption telemetry); never sees a pattern
/// or a path.
pub(crate) type SearchObserver = Arc<dyn Fn(SearchFact) + Send + Sync>;

/// One session's `fs.*` handler wiring.
pub(crate) struct FsSearchHost {
    pub(crate) observer: Option<SearchObserver>,
}

impl FsSearchHost {
    /// Register the `fs.grep`, `fs.glob`, and `fs.find` handlers.
    pub(crate) fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        let handler = |operation: FsOperation| {
            let host = Arc::clone(&host);
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(operation, payload.data).await }
            })
        };
        handlers.register("fs.grep", handler(FsOperation::Grep));
        handlers.register("fs.glob", handler(FsOperation::Glob));
        handlers.register("fs.find", handler(FsOperation::Find));
    }

    #[tracing::instrument(skip_all, fields(operation = ?operation))]
    async fn handle(&self, operation: FsOperation, data: Value) -> anyhow::Result<Value> {
        let started = Instant::now();
        let outcome = tokio::task::spawn_blocking(move || run(operation, &data))
            .await
            .context("fs search thread")?;
        if let Some(observer) = &self.observer {
            let (results, truncated) = match &outcome {
                Ok((_, results, truncated)) => (Some(*results), *truncated),
                Err(_) => (None, false),
            };
            observer(SearchFact {
                operation,
                results,
                truncated,
                elapsed: started.elapsed(),
            });
        }
        outcome.map(|(reply, _, _)| reply)
    }
}

/// Parse and run one call: the reply, its result count, and whether it was
/// capped.
fn run(operation: FsOperation, data: &Value) -> anyhow::Result<(Value, usize, bool)> {
    match operation {
        FsOperation::Grep => {
            let request = parse_grep(data)?;
            let report = grep(&request)?;
            Ok((
                report.to_json(request.max_results),
                report.matches.len(),
                report.truncated,
            ))
        }
        FsOperation::Glob => {
            let request = parse_glob(data)?;
            let listing = glob(&request)?;
            Ok((
                listing.to_json(request.max_results),
                listing.entries.len(),
                listing.truncated,
            ))
        }
        FsOperation::Find => {
            let request = parse_find(data)?;
            let listing = find(&request)?;
            Ok((
                listing.to_json(request.max_results),
                listing.entries.len(),
                listing.truncated,
            ))
        }
    }
}
