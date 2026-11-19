//! The context window's details payload: the `get_context_breakdown` response the daemon builds
//! and the TUI's context window renders. It says what the next request carries by category (an
//! estimate), the session's prompt-cache totals, its compaction history, its spend, and one row per
//! subagent. Response-only (no TS counterpart to round-trip, so no catch-all maps); every field
//! defaults, so a reply from an older daemon (or one with extra keys from a newer daemon) still
//! parses, and a category an engine cannot measure is absent rather than zero.

use serde::{Deserialize, Serialize};

/// The `get_context_breakdown` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextBreakdown {
    /// The request-view estimate by category; `None` when the engine cannot measure it (a
    /// scripted engine, or a session whose loop is not built yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<ContextCategories>,
    /// Why the estimate is missing when measuring it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories_error: Option<String>,
    /// Prompt-cache token totals over the session's branch.
    pub cache: CacheTotals,
    /// The branch's compactions.
    pub compaction: CompactionHistory,
    /// The session's spend: its own and the family total (its subagents included).
    pub cost: SessionCost,
    /// Every subagent below the session, depth-first.
    pub agents: Vec<AgentContextRow>,
}

/// One category's estimated share of the next request: chars/4 tokens and how many items (blocks,
/// messages, or tool schemas) it counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CategoryUsage {
    pub tokens: u64,
    pub items: u64,
}

/// What the next request carries, by category (chars/4 estimates over the loop's own copy of the
/// prompt, the tool schemas, and the message list).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextCategories {
    /// The cache-stable static layers of the system prompt.
    pub system_static: CategoryUsage,
    /// The system prompt's dynamic tail (packages, project context, skills, environment).
    pub system_dynamic: CategoryUsage,
    /// The tool schemas sent beside the prompt.
    pub tools: CategoryUsage,
    /// User and assistant text, plus host messages (compaction summaries, notices).
    pub messages: CategoryUsage,
    /// Assistant tool-call arguments.
    pub tool_calls: CategoryUsage,
    /// Tool-result text, after context thinning.
    pub tool_results: CategoryUsage,
    /// Image blocks in user messages and tool results.
    pub images: CategoryUsage,
    /// Assistant thinking blocks.
    pub thinking: CategoryUsage,
    /// The `[harness-digest]` context message.
    pub harness_digest: CategoryUsage,
    /// What context thinning kept out of the request; `None` while thinning is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinning: Option<ThinningSavings>,
}

/// Aged tool results the request view replays thinned, and the tokens that saved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThinningSavings {
    pub thinned_results: u64,
    pub tokens_saved: u64,
}

/// Prompt-cache token totals over the branch, plus the last request's own split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CacheTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The latest valid assistant response's prompt split; `None` before the first response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_request: Option<RequestCache>,
}

/// One request's prompt tokens: fresh input, read from the cache, and written to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RequestCache {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// The branch's compactions and the latest one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionHistory {
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<LastCompaction>,
}

/// The latest compaction: the context it replaced and when (epoch ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastCompaction {
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
}

/// The session's spend in USD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionCost {
    /// This session's own responses.
    pub own: f64,
    /// This session plus every subagent below it.
    pub total: f64,
}

/// One subagent's context and spend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentContextRow {
    pub label: String,
    /// Levels below the session (a direct child is 1).
    pub depth: u32,
    /// The roster status word (`running`, `idle`, `completed`, `deleted`, ...).
    pub status: String,
    /// Context tokens in use; `None` when unknown (right after a compaction).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// This agent's spend and its own subagents'.
    pub cost: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An older daemon's reply (no `categories`, no `agents`) and a newer one's extra keys both
    /// parse: the payload is additive.
    #[test]
    fn a_partial_or_extended_reply_parses() {
        let parsed: ContextBreakdown = serde_json::from_value(serde_json::json!({
            "cache": {"cacheRead": 10, "future": true},
            "compaction": {"count": 1, "last": {"tokensBefore": 90}},
            "later": {"field": 1},
        }))
        .expect("the reply parses");
        assert_eq!(
            parsed,
            ContextBreakdown {
                cache: CacheTotals {
                    cache_read: 10,
                    ..CacheTotals::default()
                },
                compaction: CompactionHistory {
                    count: 1,
                    last: Some(LastCompaction {
                        tokens_before: 90,
                        at_ms: None,
                    }),
                },
                ..ContextBreakdown::default()
            }
        );
    }
}
