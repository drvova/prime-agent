//! `get_context_breakdown`: the TUI context window's details. One request answers the whole
//! window: the engine's category estimate, the store's prompt-cache totals and compaction
//! history, and the context tree's spend and subagent rows. Every source is in memory (the tree's
//! children come from its background cache), so the reply never waits on a disk walk.

use pa_types::context_breakdown::{
    AgentContextRow, CacheTotals, CompactionHistory, ContextBreakdown, LastCompaction,
    RequestCache, SessionCost,
};
use pa_types::usage::valid_assistant_usage;
use serde_json::Value;

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::session_store::SessionFile;
use crate::worker::Worker;

const COMMAND: &str = "get_context_breakdown";

fn u64_at(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

fn cost_at(usage: Option<&Value>) -> f64 {
    usage
        .and_then(|usage| usage.get("cost"))
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .unwrap_or_default()
}

/// The store's prompt-cache totals (the `/session` spend fold) with the latest valid response's
/// own split, and the branch's compactions.
fn store_history(store: &SessionFile) -> (CacheTotals, CompactionHistory) {
    let stats = crate::session_stats::session_stats(store, None);
    let tokens = stats.get("tokens").cloned().unwrap_or_default();
    let branch = store.branch_bridged();
    let last_request = branch
        .iter()
        .rev()
        .filter_map(|entry| entry.fields.get("message"))
        .find_map(valid_assistant_usage)
        .map(|usage| RequestCache {
            input: u64_at(&usage, "input"),
            cache_read: u64_at(&usage, "cacheRead"),
            cache_write: u64_at(&usage, "cacheWrite"),
        });
    let cache = CacheTotals {
        input: u64_at(&tokens, "input"),
        output: u64_at(&tokens, "output"),
        cache_read: u64_at(&tokens, "cacheRead"),
        cache_write: u64_at(&tokens, "cacheWrite"),
        last_request,
    };
    let compactions: Vec<_> = branch
        .iter()
        .filter(|entry| entry.type_ == "compaction")
        .collect();
    let compaction = CompactionHistory {
        count: compactions.len() as u64,
        last: compactions.last().map(|entry| LastCompaction {
            tokens_before: u64_at(&entry.fields, "tokensBefore"),
            at_ms: crate::util::iso_to_unix_ms(&entry.timestamp),
        }),
    };
    (cache, compaction)
}

/// The tree root's spend and every node below it as a depth-first row.
fn tree_rows(tree: &Value) -> (SessionCost, Vec<AgentContextRow>) {
    fn walk(node: &Value, depth: u32, rows: &mut Vec<AgentContextRow>) {
        for child in node
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let usage = child.get("contextUsage");
            let status = if child.get("deleted").and_then(Value::as_bool) == Some(true) {
                "deleted"
            } else {
                child
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("idle")
            };
            rows.push(AgentContextRow {
                label: child
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                depth,
                status: status.to_string(),
                context_tokens: usage.and_then(|usage| usage.get("tokens")?.as_u64()),
                context_window: usage.and_then(|usage| usage.get("contextWindow")?.as_u64()),
                cost: cost_at(child.get("totalUsage").or_else(|| child.get("ownUsage"))),
            });
            walk(child, depth + 1, rows);
        }
    }
    let cost = SessionCost {
        own: cost_at(tree.get("ownUsage")),
        total: cost_at(tree.get("totalUsage")),
    };
    let mut rows = Vec::new();
    walk(tree, 1, &mut rows);
    (cost, rows)
}

impl Worker {
    /// `get_context_breakdown`: the context window's one request.
    pub(crate) async fn handle_get_context_breakdown(&self) -> DaemonResponse {
        if let Err(response) = self.require_created(COMMAND) {
            return response;
        }
        let history = {
            let core = self.core.lock().unwrap();
            core.store.as_ref().map(store_history)
        };
        let Some((cache, compaction)) = history else {
            return response_failure(None, COMMAND, "Session is still initializing", None);
        };
        let tree = self.handle_get_context_tree().await;
        let (cost, agents) = tree.data.as_ref().map_or_default(tree_rows);
        let (categories, categories_error) = match self.engine.context_categories().await {
            Ok(categories) => (categories, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let breakdown = ContextBreakdown {
            categories,
            categories_error,
            cache,
            compaction,
            cost,
            agents,
        };
        response_success(
            None,
            COMMAND,
            Some(serde_json::to_value(&breakdown).unwrap_or(Value::Null)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// Over the worker's command path: after one scripted turn the reply carries the turn's
    /// prompt-cache split and spend, no compactions, no subagents, and no category estimate
    /// (the scripted engine has no loop to measure).
    #[tokio::test]
    async fn the_reply_folds_the_store_and_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "breakdown-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let session = json!({ "activeSessionId": "breakdown-session" });
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "breakdown" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let _ = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": "breakdown-session", "message": "hi" }),
            )
            .await;
        let response = worker.dispatch(COMMAND, &session).await;
        assert!(response.success, "failed: {response:?}");
        let reply: ContextBreakdown =
            serde_json::from_value(response.data.expect("data")).expect("the typed reply");
        assert_eq!(
            reply,
            ContextBreakdown {
                cache: CacheTotals {
                    input: 120,
                    output: 8,
                    last_request: Some(RequestCache {
                        input: 120,
                        ..RequestCache::default()
                    }),
                    ..CacheTotals::default()
                },
                ..ContextBreakdown::default()
            }
        );
    }

    /// The tree folds to the root's spend and one depth-first row per subagent; a deleted
    /// billing row keeps its spend under the `deleted` status.
    #[test]
    fn the_tree_flattens_depth_first_with_the_root_spend() {
        let tree = json!({
            "id": "root",
            "ownUsage": {"cost": {"total": 0.25}},
            "totalUsage": {"cost": {"total": 0.40}},
            "children": [
                {
                    "label": "researcher", "status": "running",
                    "contextUsage": {"tokens": 9000, "contextWindow": 128_000},
                    "totalUsage": {"cost": {"total": 0.10}},
                    "children": [{
                        "label": "helper", "status": "idle",
                        "contextUsage": {"tokens": null, "contextWindow": 128_000},
                        "ownUsage": {"cost": {"total": 0.02}},
                    }],
                },
                {"label": "gone", "status": "completed", "deleted": true,
                 "ownUsage": {"cost": {"total": 0.05}}},
            ],
        });
        let row = |label: &str, depth, status: &str, tokens, window, cost| AgentContextRow {
            label: label.to_string(),
            depth,
            status: status.to_string(),
            context_tokens: tokens,
            context_window: window,
            cost,
        };
        assert_eq!(
            tree_rows(&tree),
            (
                SessionCost {
                    own: 0.25,
                    total: 0.40,
                },
                vec![
                    row("researcher", 1, "running", Some(9000), Some(128_000), 0.10),
                    row("helper", 2, "idle", None, Some(128_000), 0.02),
                    row("gone", 1, "deleted", None, None, 0.05),
                ]
            )
        );
    }
}
