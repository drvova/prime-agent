//! The state queries: `session.context`, `session.usage`, `session.tree`,
//! `session.context_tree` and `session.breakdown`, answered from pa-core's
//! own readers of a session manager (`active_context`, `get_tree`,
//! `usage_host::session_usage_json`, `turn_boundary::context_usage`) and,
//! for an open session, its engine (`context_categories`).
//!
//! An open session's manager is read on the session's own runtime, inside
//! its scope, under the same lock its turns write through; a saved one's is
//! opened for the read. Nothing is written. The subagent rows of a context
//! tree and a breakdown come with the daemon's subagents (phase 7e): until
//! then they are empty.

use anyhow::Context as _;
use pa_core::session::manager::SessionManager;
use pa_core::session::tree::SessionTree;
use pa_types::context_breakdown::{
    CacheTotals, CompactionHistory, ContextBreakdown, LastCompaction, RequestCache, SessionCost,
};
use pa_types::session::FileEntry;
use pa_types::usage::valid_assistant_usage;
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::session::{LiveEngine, SessionBuilder};

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// Runs `read` over the session manager of the session `args.session`
    /// names, and gives the engine of an open one.
    async fn read<T: Send + 'static>(
        &self,
        args: &Value,
        read: impl FnOnce(&SessionManager) -> T + Send + 'static,
    ) -> anyhow::Result<(String, T, Option<LiveEngine>)> {
        let (id, path, open) = self.session(args)?;
        if open {
            let live = self
                .runtime
                .session_engine(&id)
                .with_context(|| format!("session {id}'s engine is not built yet"))?;
            let persistence = live.engine.session.shared_persistence();
            let value = live
                .runtime
                .spawn(async move { read(&*persistence.lock().await) })
                .await
                .context("the session's runtime ended during the read")?;
            return Ok((id, value, Some(live)));
        }
        let cwd = self.cwd.clone();
        let session_dir = self.session_dir.clone();
        let value = tokio::task::spawn_blocking(move || {
            anyhow::ensure!(
                pa_core::session::store::store().exists(&path),
                "no session at {}",
                path.display()
            );
            let dir = path
                .parent()
                .map_or(session_dir, std::path::Path::to_path_buf);
            Ok(read(&SessionManager::open(&cwd, &dir, &path)))
        })
        .await
        .context("the read's thread panicked")??;
        Ok((id, value, None))
    }

    pub(super) async fn context(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, context, _) = self.read(args, SessionManager::active_context).await?;
        let messages = context
            .messages
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({
            "session": id,
            "messages": messages,
            "thinkingLevel": context.thinking_level,
            "serviceTier": context.service_tier,
            "model": context.model.map(|(provider, model_id)| json!({
                "provider": provider,
                "modelId": model_id,
            })),
        }))
    }

    pub(super) async fn usage(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, (mut usage, branch), live) = self
            .read(args, |manager| {
                let branch: Vec<FileEntry> =
                    manager.get_branch(None).into_iter().cloned().collect();
                (pa_core::usage_host::session_usage_json(manager), branch)
            })
            .await?;
        let mut counts = json!({ "user": 0, "assistant": 0, "toolCalls": 0, "toolResults": 0 });
        for message in branch.iter().filter_map(message_of) {
            let key = match message["role"].as_str() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                Some("toolResult") => "toolResults",
                _ => continue,
            };
            counts[key] = json!(counts[key].as_u64().unwrap_or_default() + 1);
            let calls = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "toolCall")
                .count();
            counts["toolCalls"] =
                json!(counts["toolCalls"].as_u64().unwrap_or_default() + calls as u64);
        }
        usage["session"] = json!(id);
        usage["messages"] = counts;
        usage["contextUsage"] = context_usage_json(&branch, live.as_ref()).await;
        Ok(usage)
    }

    pub(super) async fn tree(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, (nodes, leaf), _) = self
            .read(args, |manager| {
                let entries = manager.retained_entries();
                let tree = SessionTree::build(entries);
                let nodes: Vec<Value> = entries
                    .iter()
                    .filter(|entry| !matches!(entry, FileEntry::Label { .. }))
                    .filter_map(|entry| {
                        let id = entry.id()?;
                        let kind = serde_json::to_value(entry).ok()?["type"].clone();
                        Some(json!({
                            "id": id,
                            "parentId": entry.parent_id(),
                            "type": kind,
                            "label": tree.labels.get(id).cloned().flatten(),
                        }))
                    })
                    .collect();
                (nodes, manager.get_leaf_id().map(str::to_string))
            })
            .await?;
        Ok(json!({ "session": id, "leafId": leaf, "nodes": nodes }))
    }

    pub(super) async fn context_tree(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, (name, usage, model, branch), live) = self
            .read(args, |manager| {
                let branch: Vec<FileEntry> =
                    manager.get_branch(None).into_iter().cloned().collect();
                (
                    manager.get_session_name(),
                    pa_core::usage_host::session_usage_json(manager),
                    manager.active_context().model,
                    branch,
                )
            })
            .await?;
        Ok(json!({
            "session": id,
            "id": "root",
            "label": name.unwrap_or_else(|| "main agent".to_string()),
            "status": if live.is_some() { "active" } else { "idle" },
            "model": model.map(|(provider, model_id)| json!({ "provider": provider, "id": model_id })),
            "ownUsage": usage["cost"]["own"],
            "totalUsage": usage["cost"]["total"],
            "contextUsage": context_usage_json(&branch, live.as_ref()).await,
            "children": [],
        }))
    }

    pub(super) async fn breakdown(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, (usage, branch), live) = self
            .read(args, |manager| {
                let branch: Vec<FileEntry> =
                    manager.get_branch(None).into_iter().cloned().collect();
                (pa_core::usage_host::session_usage_json(manager), branch)
            })
            .await?;
        let tokens = |field: &str| usage["tokens"][field].as_u64().unwrap_or_default();
        let last_request = branch
            .iter()
            .rev()
            .filter_map(message_of)
            .find_map(|message| valid_assistant_usage(&message))
            .map(|usage| RequestCache {
                input: usage["input"].as_u64().unwrap_or_default(),
                cache_read: usage["cacheRead"].as_u64().unwrap_or_default(),
                cache_write: usage["cacheWrite"].as_u64().unwrap_or_default(),
            });
        let compactions: Vec<u64> = branch
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Compaction { payload, .. } => Some(payload.tokens_before),
                _ => None,
            })
            .collect();
        let categories = match &live {
            Some(live) => {
                let engine = std::sync::Arc::clone(&live.engine);
                Some(
                    live.runtime
                        .spawn(async move { engine.context_categories().await })
                        .await
                        .context("the session's runtime ended during the estimate")?,
                )
            }
            None => None,
        };
        let breakdown = ContextBreakdown {
            categories,
            categories_error: None,
            cache: CacheTotals {
                input: tokens("input"),
                output: tokens("output"),
                cache_read: tokens("cacheRead"),
                cache_write: tokens("cacheWrite"),
                last_request,
            },
            compaction: CompactionHistory {
                count: compactions.len() as u64,
                last: compactions.last().map(|tokens_before| LastCompaction {
                    tokens_before: *tokens_before,
                    at_ms: None,
                }),
            },
            cost: SessionCost {
                own: usage["cost"]["own"].as_f64().unwrap_or_default(),
                total: usage["cost"]["total"].as_f64().unwrap_or_default(),
            },
            agents: Vec::new(),
        };
        let mut value = serde_json::to_value(breakdown)?;
        value["session"] = json!(id);
        Ok(value)
    }
}

/// A transcript entry's message, as JSON.
fn message_of(entry: &FileEntry) -> Option<Value> {
    match entry {
        FileEntry::Message { message, .. } => serde_json::to_value(message).ok(),
        _ => None,
    }
}

/// The branch's estimated context use against the open session's model's
/// window (`null` for a saved session or an unknown window).
async fn context_usage_json(branch: &[FileEntry], live: Option<&LiveEngine>) -> Value {
    let Some(live) = live else {
        return Value::Null;
    };
    let window = live
        .engine
        .session
        .agent()
        .state()
        .await
        .model
        .context_window;
    pa_core::session_engine::turn_boundary::context_usage(branch, Some(window)).map_or(
        Value::Null,
        |usage| {
            json!({
                "tokens": usage.tokens,
                "contextWindow": usage.context_window,
                "percent": usage.percent,
            })
        },
    )
}
