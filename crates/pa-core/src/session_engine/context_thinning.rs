//! Request-view thinning of aged tool results: under context pressure, the
//! provider request replays old, large tool results as a short head and tail
//! plus a one-line recall pointer instead of in full. The stored session
//! transcript (and the agent's live message list) stays complete; only the
//! copy handed to the provider is thinned, through the loop's
//! `transform_context` seam.
//!
//! Adapted from Nodecode's aged-result thinning (MIT; `context.lisp`: rounds
//! older than the newest 8 cut to a head plus a recall pointer under
//! pressure), with three changes:
//!
//! - **Cache-stable boundary.** The view is a pure function of the history
//!   and the settings. Rounds thin only in whole blocks of [`STEP_ROUNDS`], so
//!   the thinned prefix of the request is byte-identical from one request to
//!   the next and only moves (invalidating the provider's prompt cache from
//!   that point) once every `STEP_ROUNDS` tool rounds. Pressure is measured on
//!   the *unthinned* history (chars/4), never on provider usage of a thinned
//!   request, so thinning cannot feed back into its own trigger and flap.
//! - **Protected results.** The newest round (the results the next response
//!   reacts to) and the newest `keep_recent_rounds` rounds are never thinned;
//!   neither is a result whose tool call id the latest assistant text names,
//!   nor an error result no later call of the same tool has superseded with a
//!   success (the output an active fix still needs).
//! - **Recall.** A persistent session pages the thinned result's whole text
//!   out to its spill store ([`ThinnedRecall::Spill`]): the pointer is the
//!   spill's content handle, read back with `ctx.page`/`ctx.grep`, so nothing
//!   thinned is lost even after compaction drops the row. Without a store the
//!   pointer names `rlm.session.tool_result(<tool call id>)`, which the
//!   `rlm.session.tool_result` host request below answers from the stored
//!   transcript. The handle is a hash of the text, so the view stays a pure
//!   function of the history; the write is queued off the request path.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use pa_agent::agent_loop::TransformContextFn;
use pa_agent::types::{
    AgentMessage, AssistantContent, Message, ToolResultContent, UserContent, UserPart,
};
use pa_types::session::FileEntry;
use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::{OutputBudget, SpillStore};
use crate::session::manager::SessionManager;

#[cfg(test)]
mod tests;

/// Default estimated share of the context window (percent) past which aged
/// results thin: early enough that compaction (95%) is deferred, late enough
/// that short sessions never see a thinned result.
pub const DEFAULT_THRESHOLD_PERCENT: f64 = 65.0;
/// Default number of newest tool rounds that always replay whole.
pub const DEFAULT_KEEP_RECENT_ROUNDS: usize = 8;
/// Default size (characters) at or under which a result never thins.
pub const DEFAULT_MIN_CHARS: usize = 600;
/// The thinning boundary advances in blocks of this many tool rounds, so the
/// request prefix (and the provider's prompt cache over it) holds still for
/// `STEP_ROUNDS - 1` requests out of every `STEP_ROUNDS`.
pub const STEP_ROUNDS: usize = 8;
/// Characters of a thinned result kept from its start.
const HEAD_CHARS: usize = 240;
/// Characters of a thinned result kept from its end (a failing run's verdict
/// usually prints last).
const TAIL_CHARS: usize = 120;
/// Image blocks count like the compaction estimate (~1200 tokens).
const IMAGE_CHARS: u64 = 4_800;

/// The resolved `contextThinning` settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThinningSettings {
    pub enabled: bool,
    pub threshold_percent: f64,
    pub keep_recent_rounds: usize,
    pub min_chars: usize,
}

impl Default for ThinningSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
            keep_recent_rounds: DEFAULT_KEEP_RECENT_ROUNDS,
            min_chars: DEFAULT_MIN_CHARS,
        }
    }
}

impl ThinningSettings {
    /// Resolve the settings document's `contextThinning` block; an unset or
    /// out-of-range field takes its default.
    #[must_use]
    pub fn resolve(raw: Option<&crate::settings::types::ContextThinningSettings>) -> Self {
        let defaults = Self::default();
        let Some(raw) = raw else {
            return defaults;
        };
        Self {
            enabled: raw.enabled.unwrap_or(defaults.enabled),
            threshold_percent: raw
                .threshold_percent
                .filter(|percent| percent.is_finite() && *percent >= 0.0)
                .unwrap_or(defaults.threshold_percent),
            keep_recent_rounds: raw
                .keep_recent_rounds
                .map_or(defaults.keep_recent_rounds, |rounds| rounds as usize),
            min_chars: raw
                .min_chars
                .map_or(defaults.min_chars, |chars| chars as usize),
        }
    }
}

/// What one request's thinning saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinningStats {
    pub thinned_results: u64,
    /// Characters of tool-result text the request did not carry.
    pub chars_saved: u64,
}

impl ThinningStats {
    /// The chars/4 token estimate of [`Self::chars_saved`] (the compaction
    /// estimator's heuristic).
    #[must_use]
    pub fn tokens_saved(&self) -> u64 {
        self.chars_saved.div_ceil(4)
    }
}

/// Where a thinned result's whole text is read back from.
#[derive(Debug, Clone)]
pub enum ThinnedRecall {
    /// The stored transcript, through `rlm.session.tool_result(<call id>)`
    /// (an ephemeral session, which keeps no spill store).
    ToolResult,
    /// The session's spill store: the text pages out under its content
    /// handle, which `ctx.*` reads.
    Spill(SpillStore),
}

/// The thinned request view and what it saved.
#[derive(Debug, Clone, PartialEq)]
pub struct ThinnedView {
    pub messages: Vec<AgentMessage>,
    pub stats: ThinningStats,
}

/// Thin the request view of `messages` for a model with `context_window`
/// tokens, pointing each thinned result at `recall`. Pure: the same history
/// and settings always produce the same view (a spill store only gains the
/// thinned texts it does not hold yet, off this path).
#[must_use]
pub fn thin_context(
    messages: Vec<AgentMessage>,
    context_window: u64,
    settings: &ThinningSettings,
    recall: &ThinnedRecall,
) -> ThinnedView {
    let unchanged = |messages| ThinnedView {
        messages,
        stats: ThinningStats::default(),
    };
    if !settings.enabled || context_window == 0 {
        return unchanged(messages);
    }
    let estimated_tokens = estimate_tokens(&messages);
    if (estimated_tokens as f64) < context_window as f64 * settings.threshold_percent / 100.0 {
        return unchanged(messages);
    }

    // Round numbers by tool call id, and the latest assistant text.
    let mut round_of_call: HashMap<&str, usize> = HashMap::new();
    let mut rounds = 0usize;
    let mut latest_assistant_text = String::new();
    for message in &messages {
        let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
            continue;
        };
        latest_assistant_text.clear();
        let mut has_calls = false;
        for block in &assistant.content {
            match block {
                AssistantContent::Text(text) => latest_assistant_text.push_str(&text.text),
                AssistantContent::ToolCall(call) => {
                    round_of_call.insert(call.id.as_str(), rounds);
                    has_calls = true;
                }
                AssistantContent::Thinking(_) => {}
            }
        }
        rounds += usize::from(has_calls);
    }
    let thinnable = rounds.saturating_sub(settings.keep_recent_rounds.max(1));
    let boundary = thinnable / STEP_ROUNDS * STEP_ROUNDS;
    if boundary == 0 {
        return unchanged(messages);
    }

    // Error results no later success of the same tool superseded.
    let mut succeeded_later: HashSet<&str> = HashSet::new();
    let mut active_errors: HashSet<&str> = HashSet::new();
    for message in messages.iter().rev() {
        if let AgentMessage::Standard(Message::ToolResult(result)) = message {
            if result.is_error {
                if !succeeded_later.contains(result.tool_name.as_str()) {
                    active_errors.insert(result.tool_call_id.as_str());
                }
            } else {
                succeeded_later.insert(result.tool_name.as_str());
            }
        }
    }

    let thinned_ids: HashSet<String> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(result),
            _ => None,
        })
        .filter(|result| {
            round_of_call
                .get(result.tool_call_id.as_str())
                .is_some_and(|round| *round < boundary)
                && !active_errors.contains(result.tool_call_id.as_str())
                && !latest_assistant_text.contains(result.tool_call_id.as_str())
        })
        .map(|result| result.tool_call_id.clone())
        .collect();

    let mut stats = ThinningStats::default();
    let messages = messages
        .into_iter()
        .map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(mut result))
                if thinned_ids.contains(&result.tool_call_id) =>
            {
                // One head/pointer/tail block replaces the text blocks
                // (images keep their place); a small result, or one whose
                // thinned form would not be shorter, stays whole.
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text(text) => Some(text.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let total = text.chars().count();
                if total <= settings.min_chars.max(HEAD_CHARS + TAIL_CHARS) {
                    return AgentMessage::Standard(Message::ToolResult(result));
                }
                let head_end = text
                    .char_indices()
                    .nth(HEAD_CHARS)
                    .map_or(text.len(), |(at, _)| at);
                let tail_start = text
                    .char_indices()
                    .nth(total - TAIL_CHARS)
                    .map_or(text.len(), |(at, _)| at);
                let omitted = total - HEAD_CHARS - TAIL_CHARS;
                let mut thinned = String::with_capacity(HEAD_CHARS + TAIL_CHARS + 192);
                thinned.push_str(&text[..head_end]);
                match recall {
                    ThinnedRecall::Spill(store) => {
                        let _ = write!(thinned, "\n{}\n", store.retain(&text).thinned_marker());
                    }
                    ThinnedRecall::ToolResult => {
                        let _ = write!(
                            thinned,
                            "\n[... {omitted} of {total} chars of this older result thinned from the request; full text: (rlm-session-tool-result {:?}) ...]\n",
                            result.tool_call_id
                        );
                    }
                }
                thinned.push_str(&text[tail_start..]);
                let kept = thinned.chars().count();
                if kept >= total {
                    return AgentMessage::Standard(Message::ToolResult(result));
                }
                let mut replaced = false;
                result.content.retain_mut(|block| match block {
                    ToolResultContent::Text(text) if !replaced => {
                        replaced = true;
                        text.text = std::mem::take(&mut thinned);
                        true
                    }
                    ToolResultContent::Text(_) => false,
                    ToolResultContent::Image(_) => true,
                });
                stats.thinned_results += 1;
                stats.chars_saved += (total - kept) as u64;
                AgentMessage::Standard(Message::ToolResult(result))
            }
            other => other,
        })
        .collect();
    ThinnedView { messages, stats }
}

/// The chars/4 token estimate of the unthinned history (the compaction
/// estimator's sizing, on the loop's wire types).
fn estimate_tokens(messages: &[AgentMessage]) -> u64 {
    let chars = |text: &str| text.chars().count() as u64;
    messages
        .iter()
        .map(|message| match message {
            AgentMessage::Standard(Message::User(user)) => match &user.content {
                UserContent::Text(text) => chars(text),
                UserContent::Parts(parts) => parts
                    .iter()
                    .map(|part| match part {
                        UserPart::Text(text) => chars(&text.text),
                        UserPart::Image(_) => IMAGE_CHARS,
                    })
                    .sum(),
            },
            AgentMessage::Standard(Message::Assistant(assistant)) => assistant
                .content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(text) => chars(&text.text),
                    AssistantContent::Thinking(thinking) => chars(&thinking.thinking),
                    AssistantContent::ToolCall(call) => {
                        chars(&call.name) + chars(&call.arguments.to_string())
                    }
                })
                .sum(),
            AgentMessage::Standard(Message::ToolResult(result)) => result
                .content
                .iter()
                .map(|block| match block {
                    ToolResultContent::Text(text) => chars(&text.text),
                    ToolResultContent::Image(_) => IMAGE_CHARS,
                })
                .sum(),
            AgentMessage::Custom(custom) => chars(&custom.payload.to_string()),
        })
        .sum::<u64>()
        .div_ceil(4)
}

/// Reads the context window of the model the next request goes to.
pub type ContextWindowProbe = Arc<dyn Fn() -> pa_agent::BoxFut<'static, u64> + Send + Sync>;

/// Observes each request that thinned at least one result (telemetry).
pub type ThinningObserver = Arc<dyn Fn(&ThinningStats) + Send + Sync>;

/// The `transform_context` seam: publish each request's context fill to
/// `budget` (the cell output cap), and thin its view. Pure CPU over the
/// copy the loop hands it; it never awaits disk or network (the probe reads
/// in-memory model state).
#[must_use]
pub fn thinning_transform(
    settings: ThinningSettings,
    context_window: ContextWindowProbe,
    recall: ThinnedRecall,
    budget: OutputBudget,
    observer: Option<ThinningObserver>,
) -> TransformContextFn {
    Arc::new(move |messages, _signal| {
        let context_window = Arc::clone(&context_window);
        let observer = observer.clone();
        let recall = recall.clone();
        let budget = budget.clone();
        Box::pin(async move {
            let window = context_window().await;
            // The cells this request's answer runs size their output cap to
            // the context left (in-memory atomics; the executor reads them).
            budget.observe(estimate_tokens(&messages), window);
            if !settings.enabled {
                return Ok(messages);
            }
            let view = thin_context(messages, window, &settings, &recall);
            if view.stats.thinned_results > 0 {
                tracing::debug!(
                    thinned_results = view.stats.thinned_results,
                    chars_saved = view.stats.chars_saved,
                    "context thinning applied to the request view"
                );
                if let Some(observer) = observer {
                    observer(&view.stats);
                }
            }
            Ok(view.messages)
        })
    })
}

/// Register `rlm.session.tool_result`: one tool result's full text by its
/// tool call id, from the session's loaded transcript rows. Answers
/// `{found: false}` when no loaded row matches (a windowed session's
/// compacted-away rows); the kernel helper then pages `rlm.session.messages`.
pub fn register_tool_result_recall(
    handlers: &mut HostRequestHandlers,
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
) {
    // Weak: the kernel holds its handlers for its whole life.
    let session = Arc::downgrade(session);
    handlers.register(
        "rlm.session.tool_result",
        host_handler(move |payload| {
            let session = session.clone();
            Box::pin(async move {
                let Some(tool_call_id) = payload.data.get("toolCallId").and_then(Value::as_str)
                else {
                    anyhow::bail!("rlm.session.tool_result tool_call_id must be a string");
                };
                let Some(session) = session.upgrade() else {
                    anyhow::bail!("the session ended before the request could be served");
                };
                let session = session.lock().await;
                let found = session
                    .retained_entries()
                    .iter()
                    .find_map(|entry| match entry {
                        FileEntry::Message {
                            message: pa_types::session::AgentMessage::ToolResult(result),
                            ..
                        } if result.tool_call_id == tool_call_id => Some(result),
                        _ => None,
                    });
                Ok(match found {
                    Some(result) => json!({
                        "found": true,
                        "toolName": result.tool_name,
                        "isError": result.is_error,
                        "text": result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                pa_types::ai::UserContentBlock::Text(text) => {
                                    Some(text.text.as_str())
                                }
                                pa_types::ai::UserContentBlock::Image(_)
                                | pa_types::ai::UserContentBlock::Raw(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    }),
                    None => json!({ "found": false }),
                })
            })
        }),
    );
}
