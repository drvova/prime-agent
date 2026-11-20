//! The host side of the kernel `worktree` capability. The git work runs in the kernel
//! (`rlm.worktree`); after each operation it reports `worktree.record` here: the operation's
//! outcome for the adoption counts, and the worktree's name and checkout path for the ledger
//! behind the session-end notice that names the worktrees a session left unremoved.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// One reported worktree operation (counts only reach telemetry: never a name or a path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeFact {
    Created,
    Diffed,
    /// A patch merge applied the delta to the parent's checkout.
    Merged,
    /// A branch merge committed the delta to a branch.
    Branched,
    /// A merge found conflicts and applied nothing.
    Conflicted,
    Removed,
    Failed,
}

impl WorktreeFact {
    fn parse(op: &str, outcome: &str) -> Option<Self> {
        Some(match (op, outcome) {
            ("create", "ok") => WorktreeFact::Created,
            ("diff", "ok") => WorktreeFact::Diffed,
            ("merge", "applied") => WorktreeFact::Merged,
            ("merge", "branch") => WorktreeFact::Branched,
            ("merge", "conflicts") => WorktreeFact::Conflicted,
            ("remove", "ok") => WorktreeFact::Removed,
            (_, "failed") => WorktreeFact::Failed,
            // An empty merge brought nothing home (an outcome, not an adoption count), and
            // `list` counts nothing.
            _ => return None,
        })
    }
}

/// Hears each reported worktree operation.
pub(crate) type WorktreeObserver = Arc<dyn Fn(WorktreeFact) + Send + Sync>;

/// The worktrees this session created and has not removed, by name.
#[derive(Debug, Default)]
pub struct WorktreeLedger {
    live: Mutex<BTreeMap<String, PathBuf>>,
}

impl WorktreeLedger {
    /// The session-end notice: the worktrees still on disk that this session never removed.
    #[must_use]
    pub fn unremoved_notice(&self) -> Option<String> {
        let live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let left: Vec<String> = live
            .iter()
            .filter(|(_, path)| path.is_dir())
            .map(|(name, path)| format!("{name} ({})", path.display()))
            .collect();
        (!left.is_empty()).then(|| {
            format!(
                "This session left {} worktree{} unremoved: {}. Merge or remove them with worktree.merge/worktree.remove, or delete them with `git worktree remove <path>`.",
                left.len(),
                if left.len() == 1 { "" } else { "s" },
                left.join(", ")
            )
        })
    }

    fn note(&self, op: &str, outcome: &str, name: Option<&str>, path: Option<&str>) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (op, outcome, name) {
            ("create", "ok", Some(name)) => {
                if let Some(path) = path {
                    live.insert(name.to_string(), PathBuf::from(path));
                }
            }
            ("remove", "ok", Some(name)) => {
                live.remove(name);
            }
            _ => {}
        }
    }
}

/// Register `worktree.record` over `ledger`, reporting each operation to `observer`.
pub(crate) fn register_worktree_host_handler(
    handlers: &mut HostRequestHandlers,
    ledger: Arc<WorktreeLedger>,
    observer: Option<WorktreeObserver>,
) {
    handlers.register(
        "worktree.record",
        host_handler(move |payload| {
            let ledger = Arc::clone(&ledger);
            let observer = observer.clone();
            async move {
                let field = |key: &str| payload.data.get(key).and_then(Value::as_str);
                let op = field("op").context("worktree.record needs an op")?;
                let outcome = field("outcome").context("worktree.record needs an outcome")?;
                ledger.note(op, outcome, field("name"), field("path"));
                if let (Some(observer), Some(fact)) = (&observer, WorktreeFact::parse(op, outcome))
                {
                    observer(fact);
                }
                Ok(json!({}))
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn record(handlers: &HostRequestHandlers, data: Value) {
        let handler = handlers.get("worktree.record").expect("registered").clone();
        handler(crate::kernel::shared::HostRequestPayload { data })
            .await
            .expect("recorded");
    }

    /// The ledger names the created worktrees still on disk at session end, a removed one drops
    /// out, and the observer hears each counted outcome.
    #[tokio::test]
    async fn the_session_end_notice_names_the_unremoved_worktrees() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (kept, gone) = (dir.path().join("lane-a"), dir.path().join("lane-b"));
        std::fs::create_dir_all(&kept).expect("kept dir");
        std::fs::create_dir_all(&gone).expect("gone dir");
        let ledger = Arc::new(WorktreeLedger::default());
        let facts = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::clone(&facts);
        let mut handlers = HostRequestHandlers::default();
        register_worktree_host_handler(
            &mut handlers,
            Arc::clone(&ledger),
            Some(Arc::new(move |fact| heard.lock().unwrap().push(fact))),
        );
        for (name, path) in [("lane-a", &kept), ("lane-b", &gone)] {
            record(
                &handlers,
                json!({ "op": "create", "outcome": "ok", "name": name, "path": path }),
            )
            .await;
        }
        record(&handlers, json!({ "op": "merge", "outcome": "conflicts" })).await;
        record(
            &handlers,
            json!({ "op": "remove", "outcome": "ok", "name": "lane-b" }),
        )
        .await;
        assert_eq!(
            ledger.unremoved_notice(),
            Some(format!(
                "This session left 1 worktree unremoved: lane-a ({}). Merge or remove them with worktree.merge/worktree.remove, or delete them with `git worktree remove <path>`.",
                kept.display()
            ))
        );
        assert_eq!(
            *facts.lock().unwrap(),
            [
                WorktreeFact::Created,
                WorktreeFact::Created,
                WorktreeFact::Conflicted,
                WorktreeFact::Removed
            ]
        );
        std::fs::remove_dir_all(&kept).expect("remove kept");
        assert_eq!(ledger.unremoved_notice(), None);
    }
}
