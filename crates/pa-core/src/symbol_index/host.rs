//! The `fs.symbol` host request the kernel's `rlm.fs` client sends: one
//! name's definitions and references from the project's symbol index, with a
//! spill handle to each definition's source. The query runs on a blocking
//! thread and starts a background refresh; it never waits for a build.

use serde_json::{json, Value};

use super::{Found, SymbolIndex};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};

/// How long a query before the first full pass waits for it (a small project
/// finishes inside it; a large one answers partial).
const COLD_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
/// The default and largest number of references one query returns.
const DEFAULT_REFERENCES: usize = 50;
const MAX_REFERENCES: usize = 500;

/// One answered query (adoption counts only: never a name or a path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SymbolFact {
    pub(crate) definitions: usize,
    pub(crate) references: usize,
    /// Answered before the first full pass finished.
    pub(crate) partial: bool,
}

/// Hears each answered `fs.symbol` query.
pub(crate) type SymbolObserver = std::sync::Arc<dyn Fn(SymbolFact) + Send + Sync>;

fn found_json(found: &Found) -> Value {
    let mut value = json!({"path": found.path, "line": found.line, "text": found.text});
    if let Some(kind) = &found.kind {
        value["kind"] = json!(kind);
    }
    value
}

/// Register `fs.symbol` over the project's index (its first query starts the build).
pub(crate) fn register_symbol_handler(
    handlers: &mut HostRequestHandlers,
    index: SymbolIndex,
    store: Option<SpillStore>,
    observer: Option<SymbolObserver>,
) {
    handlers.register(
        "fs.symbol",
        host_handler(move |payload| {
            let index = index.clone();
            let store = store.clone();
            let observer = observer.clone();
            Box::pin(async move {
                let name = payload
                    .data
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && !name.chars().any(char::is_whitespace))
                    .ok_or_else(|| anyhow::anyhow!("fs.symbol: name must be one identifier"))?
                    .to_string();
                let max_references = payload
                    .data
                    .get("maxReferences")
                    .and_then(Value::as_u64)
                    .map_or(DEFAULT_REFERENCES, |max| (max as usize).min(MAX_REFERENCES));
                index.refresh();
                if !index.is_complete() {
                    let _ = tokio::time::timeout(COLD_WAIT, index.refreshed()).await;
                }
                let answer = {
                    let index = index.clone();
                    tokio::task::spawn_blocking(move || index.query(&name, max_references))
                        .await
                        .map_err(|error| anyhow::anyhow!("fs.symbol: {error}"))?
                };
                // The spill only queues each write: the reply never waits on disk.
                let source_handles: Vec<Option<String>> = answer
                    .sources
                    .iter()
                    .map(|source| {
                        store.as_ref().map(|store| {
                            format!(
                                "{HANDLE_PREFIX}{}",
                                store.spill(SpillCopy::of_text(source)).id
                            )
                        })
                    })
                    .collect();
                if let Some(observer) = observer {
                    observer(SymbolFact {
                        definitions: answer.definitions.len(),
                        references: answer.references.len(),
                        partial: answer.partial,
                    });
                }
                let definitions: Vec<Value> = answer
                    .definitions
                    .iter()
                    .zip(&source_handles)
                    .map(|(found, handle)| {
                        let mut value = found_json(found);
                        value["handle"] = json!(handle);
                        value
                    })
                    .collect();
                Ok(json!({
                    "definitions": definitions,
                    "references": answer.references.iter().map(found_json).collect::<Vec<_>>(),
                    "truncated": answer.truncated,
                    "partial": answer.partial,
                }))
            })
        }),
    );
}
