//! `get_session_stats` over the worker session store, serving scripted and
//! real engine sessions. The token-estimate helpers live in
//! `pa_types::usage` (shared with pa-core's `compact.status` host request).

use serde_json::{json, Value};

use pa_types::context_breakdown::RequestCache;
use pa_types::usage::{
    calculate_context_tokens, estimate_tokens, valid_assistant_usage, ContextUsage,
};

use crate::session_store::{SessionEntry, SessionFile};

/// Compute the `get_session_stats` response data for one session file.
/// `context_window` is the engine model's context window; `None` (or zero)
/// omits `contextUsage`.
///
/// The message counts walk the ACTIVE BRANCH (gap-bridged) from the latest
/// compaction's `firstKeptEntryId` onward. `tokens`/`cost` are the branch's
/// spend across every compaction: the fold `get_context_tree` serves as the
/// root's `totalUsage` (assistant rows with their attributed child spend,
/// plus the compaction, branch-summary, and `model_usage` rows), plus a
/// windowed load's folded older path. `contextUsage` keeps the strict branch.
pub fn session_stats(store: &SessionFile, context_window: Option<u64>) -> Value {
    let branch = store.branch();
    let messages: Vec<&Value> = branch
        .iter()
        .filter(|entry| entry.type_ == "message")
        .filter_map(|entry| entry.fields.get("message"))
        .collect();
    let (durable_messages, boundary) = kept_region_messages(store);
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut tool_results = 0u64;
    let mut tool_calls = 0u64;
    for message in &durable_messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => user_messages += 1,
            Some("assistant") => {
                assistant_messages += 1;
                tool_calls += tool_call_count(message);
            }
            Some("toolResult") => tool_results += 1,
            _ => {}
        }
    }
    let (_, spend) =
        crate::state_getters::compute_own_and_total_usage(&store.branch_bridged(), store.entries());
    let field = |name: &str| spend.get(name).and_then(Value::as_u64).unwrap_or_default();
    let mut input = field("input");
    let mut output = field("output");
    let mut cache_read = field("cacheRead");
    let mut cache_write = field("cacheWrite");
    let mut cost = spend
        .get("cost")
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    // The window is a load optimization, not a session state: its folded
    // older path is spend on this branch, and without a compaction boundary
    // the kept region's counts cover that path too.
    let mut older_total_messages = 0u64;
    if let Some(window) = &store.window {
        let older = &window.older_path_stats;
        input += older.input;
        output += older.output;
        cache_read += older.cache_read;
        cache_write += older.cache_write;
        cost += older.cost;
        if boundary.is_none() {
            user_messages += older.user_messages;
            assistant_messages += older.assistant_messages;
            tool_results += older.tool_results;
            tool_calls += older.tool_calls;
            older_total_messages = older.total_messages;
        }
    }
    let total_messages = durable_messages.len() as u64 + older_total_messages;
    let mut stats = json!({
        "sessionFile": store.path.display().to_string(),
        "sessionId": store.session_id(),
        "userMessages": user_messages,
        "assistantMessages": assistant_messages,
        "toolCalls": tool_calls,
        "toolResults": tool_results,
        "totalMessages": total_messages,
        "tokens": {
            "input": input,
            "output": output,
            "cacheRead": cache_read,
            "cacheWrite": cache_write,
            "total": input + output + cache_read + cache_write,
        },
        "cost": cost,
    });
    if let Some(usage) = context_usage(&branch, &messages, context_window) {
        stats["contextUsage"] = json!(usage);
    }
    stats
}

/// The messages after the latest ON-CHAIN compaction: the active branch,
/// gap-bridged, restricted to the kept region (an off-chain compaction never
/// bounds it). Returns the messages and the boundary position (`None` = all kept).
fn kept_region_messages(store: &SessionFile) -> (Vec<&Value>, Option<usize>) {
    let entries = store.entries();
    let chain = store.branch_bridged_positions();
    // The latest compaction ON THE CHAIN bounds the kept region; its
    // `firstKeptEntryId` names the first row the post-compaction reload keeps.
    // A torn write that lost the boundary row falls back to the compaction entry.
    let boundary = chain
        .iter()
        .rev()
        .find(|position| entries[**position].type_ == "compaction")
        .map(|compaction| {
            entries[*compaction]
                .fields
                .get("firstKeptEntryId")
                .and_then(Value::as_str)
                .and_then(|id| chain.iter().find(|position| entries[**position].id == id))
                .unwrap_or(compaction)
        });
    let messages = chain
        .iter()
        .filter(|position| match boundary {
            Some(boundary) => **position >= *boundary,
            None => true,
        })
        .filter(|position| entries[**position].type_ == "message")
        .filter_map(|position| entries[*position].fields.get("message"))
        .collect();
    (messages, boundary.copied())
}

/// `contextUsage` for one whole store (the `get_context_tree` root node):
/// `None` when the context window is unknown, matching `session_stats`.
pub(crate) fn store_context_usage(
    store: &SessionFile,
    context_window: Option<u64>,
) -> Option<ContextUsage> {
    let branch = store.branch();
    let messages: Vec<&Value> = branch
        .iter()
        .filter(|entry| entry.type_ == "message")
        .filter_map(|entry| entry.fields.get("message"))
        .collect();
    context_usage(&branch, &messages, context_window)
}

/// `toolCall` content blocks on one assistant message.
pub(crate) fn tool_call_count(message: &Value) -> u64 {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("toolCall"))
                .count() as u64
        })
        .unwrap_or_default()
}

/// Estimated context usage: the last valid assistant usage plus trailing
/// message estimates; `null` tokens right after a compaction without a usable
/// post-compaction usage; `None` when the context window is unknown.
fn context_usage(
    branch: &[&SessionEntry],
    messages: &[&Value],
    context_window: Option<u64>,
) -> Option<ContextUsage> {
    let context_window = context_window.filter(|window| *window > 0)?;

    // The latest compaction entry on the branch, if any (TS `getLatestCompactionEntry`).
    let compaction_index = branch.iter().rposition(|entry| entry.type_ == "compaction");
    if let Some(compaction_index) = compaction_index {
        // Only usage from an assistant that responded after the compaction
        // boundary is trustworthy (earlier usage reflects the old context size).
        let post_compaction_usage = branch
            .iter()
            .rev()
            .take(branch.len() - compaction_index - 1)
            .filter_map(|entry| entry.fields.get("message"))
            .find_map(valid_assistant_usage);
        let usable =
            post_compaction_usage.is_some_and(|usage| calculate_context_tokens(&usage) > 0);
        if !usable {
            return Some(ContextUsage {
                tokens: None,
                context_window,
                percent: None,
                estimated: false,
                last_request: None,
            });
        }
    }

    // TS `estimateContextTokens`: the last valid assistant usage anchors the
    // estimate; messages after it are added with the chars/4 heuristic.
    let mut tokens = 0u64;
    let mut last_request = None;
    match messages
        .iter()
        .rposition(|message| valid_assistant_usage(message).is_some())
    {
        Some(last_usage_index) => {
            let usage = valid_assistant_usage(messages[last_usage_index]).expect("checked");
            tokens += calculate_context_tokens(&usage);
            last_request = serde_json::from_value::<RequestCache>(usage).ok();
            tokens += messages[last_usage_index + 1..]
                .iter()
                .map(|message| estimate_tokens(message))
                .sum::<u64>();
        }
        None => {
            tokens += messages
                .iter()
                .map(|message| estimate_tokens(message))
                .sum::<u64>();
        }
    }
    Some(ContextUsage {
        last_request,
        ..ContextUsage::of(tokens, context_window)
    })
}

/// The live context estimate while an assistant reply streams: the prompt the provider already
/// reported for this request (Anthropic's `message_start` usage), else the last reading (the
/// settled context plus the estimated rows appended since), plus the streamed output so far by the
/// chars/4 heuristic. A provider's mid-stream output count is not trusted (Anthropic reports a
/// placeholder until `message_delta`). A reported prompt also yields this request's cache split.
/// `None` when neither anchor is known (right after a compaction, before the first reply).
pub(crate) fn streaming_context_usage(
    base: Option<&ContextUsage>,
    partial: &Value,
) -> Option<ContextUsage> {
    let base = base?;
    let reported = partial
        .get("usage")
        .and_then(|usage| serde_json::from_value::<RequestCache>(usage.clone()).ok())
        .filter(|request| request.input + request.cache_read + request.cache_write > 0);
    let prompt = match reported {
        Some(request) => request.input + request.cache_read + request.cache_write,
        None => base.tokens?,
    };
    Some(ContextUsage {
        estimated: true,
        last_request: reported.or(base.last_request),
        ..ContextUsage::of(prompt + estimate_tokens(partial), base.context_window)
    })
}

/// `totalTokens` when present, else the four-field sum.
#[cfg(test)]
mod tests;
