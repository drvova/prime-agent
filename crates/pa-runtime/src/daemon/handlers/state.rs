//! The state queries: `session.context`, `session.usage`, `session.tree`,
//! `session.context_tree` and `session.breakdown`, answered from pa-core's
//! own readers of a session manager (`active_context`, `get_tree`,
//! `usage_host::session_usage_json`, `turn_boundary::context_usage`) and,
//! for an open session, its engine (`context_categories`).
//!
//! An open session's manager is read on the session's own runtime, inside
//! its scope, under the same lock its turns write through; a saved one's is
//! opened for the read. Nothing is written. An open session's context tree
//! has a child node, and its breakdown an agent row, for each subagent below
//! it (each read from that subagent's own engine); a saved one's has none.

use anyhow::Context as _;
use pa_core::session::manager::SessionManager;
use pa_core::session::tree::SessionTree;
use pa_types::context_breakdown::{
    AgentContextRow, CacheTotals, CompactionHistory, ContextBreakdown, LastCompaction,
    RequestCache, SessionCost,
};
use pa_types::session::FileEntry;
use pa_types::usage::valid_assistant_usage;
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::session::{LiveEngine, SessionBuilder};
use crate::subagents::FamilyMember;

/// What one subagent's engine says for the state queries.
struct MemberState {
    model: Option<(String, String)>,
    usage: Value,
    context_usage: Value,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// The state of subagent `member`'s session, read from its engine on its
    /// own runtime; `None` once it no longer runs.
    async fn member_state(&self, member: &FamilyMember) -> Option<MemberState> {
        let live = self.runtime.session_engine(&member.session_id)?;
        let persistence = live.engine.session.shared_persistence();
        let (model, usage, branch) = live
            .runtime
            .spawn(async move {
                let manager = persistence.lock().await;
                let branch: Vec<FileEntry> =
                    manager.get_branch(None).into_iter().cloned().collect();
                (
                    manager.active_context().model,
                    pa_core::usage_host::session_usage_json(&manager),
                    branch,
                )
            })
            .await
            .ok()?;
        Some(MemberState {
            model,
            usage,
            context_usage: context_usage_json(&branch, Some(&live)).await,
        })
    }

    /// The context-tree nodes of `members` and their subtrees.
    async fn member_nodes(&self, members: &[FamilyMember]) -> Vec<Value> {
        // Depth first, without recursion: a node is built once its
        // children's are.
        let mut pending: Vec<(&FamilyMember, bool)> =
            members.iter().rev().map(|member| (member, false)).collect();
        let mut built: Vec<Value> = Vec::new();
        while let Some((member, expanded)) = pending.pop() {
            if !expanded {
                pending.push((member, true));
                pending.extend(member.children.iter().rev().map(|child| (child, false)));
                continue;
            }
            let children = built.split_off(built.len() - member.children.len());
            let state = self.member_state(member).await;
            built.push(json!({
                "id": member.child_id,
                "sessionId": member.session_id,
                "label": member.name,
                "status": member.status,
                "model": state.as_ref().and_then(|state| state.model.clone()).map(
                    |(provider, model_id)| json!({ "provider": provider, "id": model_id })
                ),
                "ownUsage": state.as_ref().map_or(Value::Null, |state| state.usage["cost"]["own"].clone()),
                "totalUsage": state.as_ref().map_or(Value::Null, |state| state.usage["cost"]["total"].clone()),
                "contextUsage": state.as_ref().map_or(Value::Null, |state| state.context_usage.clone()),
                "children": children,
            }));
        }
        built
    }

    /// The breakdown's agent rows of `members` and their subtrees, each
    /// below its parent.
    async fn member_rows(&self, members: &[FamilyMember]) -> Vec<AgentContextRow> {
        let mut pending: Vec<&FamilyMember> = members.iter().rev().collect();
        let mut rows = Vec::new();
        while let Some(member) = pending.pop() {
            let state = self.member_state(member).await;
            let context = state.as_ref().map(|state| &state.context_usage);
            rows.push(AgentContextRow {
                label: member.name.clone(),
                depth: member.depth,
                status: member.status.to_string(),
                context_tokens: context.and_then(|usage| usage["tokens"].as_u64()),
                context_window: context.and_then(|usage| usage["contextWindow"].as_u64()),
                cost: state
                    .as_ref()
                    .and_then(|state| state.usage["cost"]["total"].as_f64())
                    .unwrap_or_default(),
            });
            pending.extend(member.children.iter().rev());
        }
        rows
    }

    /// Runs `read` over the session manager of the session `args.session`
    /// names, and gives the engine of an open one.
    async fn read<T: Send + 'static>(
        &self,
        args: &Value,
        read: impl FnOnce(&SessionManager) -> T + Send + 'static,
    ) -> anyhow::Result<(String, T, Option<LiveEngine>)> {
        let (id, path, open) = self.session(args)?;
        if open {
            let live = self.runtime.built_engine(&id).await?;
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
            "children": self.member_nodes(&self.runtime.subagents().family_of(&id).await).await,
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
            agents: self
                .member_rows(&self.runtime.subagents().family_of(&id).await)
                .await,
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
