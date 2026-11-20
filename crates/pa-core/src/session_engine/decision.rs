//! `rlm.decide(question, options, *, context=None)`: one bounded decision by the session's own
//! model, asked from inside a running cell. The model writes a whole search, edit, test, retry
//! loop as one cell that runs at kernel speed and pays a model call only at a real decision
//! point, instead of a full round (transcript replay, tool call, result) per step.
//!
//! A decision is one short standalone request: a fixed instruction, the question, the
//! numbered options and the caller's `context`; no tools and no conversation, so it neither
//! grows the transcript nor touches the main loop's cached prefix. It runs on the session's
//! provider stream function with the session's model and thinking level (never another model),
//! which admits every request through provider admission (`pa_ai::admission`) like any turn.
//! Each call is billed as a `model_usage` row (source `decide`), and one cell may ask at most
//! [`DECISIONS_PER_CELL`]; the reply carries the compact `[decide] question → choice` row the
//! kernel prints into the cell's output, the only transcript growth a decision causes.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use pa_agent::agent::Agent;
use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions};
use pa_agent::types::{AssistantContent, Message, StopReason, UserContent, UserMessage};
use serde_json::{json, Value};

use super::model_usage::ModelUsage;
use super::rlm_usage::RlmChildUsageStore;
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The host request the kernel's `rlm.decide` sends.
pub(crate) const DECIDE_REQUEST_TYPE: &str = "rlm.decide";
/// The most decisions one cell may ask for; the next one is refused before any model call.
pub(crate) const DECISIONS_PER_CELL: u32 = 8;
/// Output cap of one decision: a number, with room for a thinking model's reasoning.
const DECIDE_MAX_OUTPUT_TOKENS: u64 = 1024;
/// Wall-clock cap of one decision; past it the cell gets the timeout as its error.
pub(crate) const DECIDE_TIMEOUT: Duration = Duration::from_secs(120);
/// The `model_usage` source of a decision.
const DECIDE_USAGE_SOURCE: &str = "decide";
/// The cells whose decision counts are kept (cells run one at a time, plus background cells).
const TRACKED_CELLS: usize = 64;
/// Characters of the question a transcript row keeps.
const ROW_QUESTION_CHARS: usize = 120;
const QUESTION_CHARS_MAX: usize = 2_000;
const OPTION_CHARS_MAX: usize = 200;
const OPTIONS_MAX: usize = 16;
const CONTEXT_CHARS_MAX: usize = 8_000;

const DECIDE_INSTRUCTION: &str = "You make one decision for a program an agent is running. \
Read the question, the context and the numbered options, then reply with the number of the \
one option to take and nothing else.";

/// What one `rlm.decide` call came to, for telemetry (counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecideFact {
    /// The model chose an option.
    Decided,
    /// The cell had used its decisions; no model call was made.
    Limited,
    /// The call failed (provider error, timeout, unparseable answer).
    Failed,
}

/// Hears each decision's outcome (session telemetry).
pub(crate) type DecideObserver = Arc<dyn Fn(DecideFact) + Send + Sync>;

/// One parsed `rlm.decide` request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecisionRequest {
    question: String,
    options: Vec<String>,
    context: Option<String>,
    /// The asking cell's id; empty when the call came from outside a cell.
    cell: String,
}

/// One decision's result.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Decision {
    index: usize,
    /// The tokens the call billed (0 when the provider counted none).
    tokens: u64,
}

/// The session's decision maker: its agent (weakly, like the goal judge: the kernel's handlers
/// reach it, and a strong edge would keep a dropped session alive), the provider function, and
/// where the call's usage is billed.
pub(crate) struct Decider {
    agent: Weak<Agent>,
    stream_fn: StreamFn,
    usage_store: Arc<dyn RlmChildUsageStore>,
    timeout: Duration,
}

impl Decider {
    #[must_use]
    pub(crate) fn new(
        agent: Weak<Agent>,
        stream_fn: StreamFn,
        usage_store: Arc<dyn RlmChildUsageStore>,
    ) -> Self {
        Self {
            agent,
            stream_fn,
            usage_store,
            timeout: DECIDE_TIMEOUT,
        }
    }

    /// One capped provider call that picks an option of `request`; its usage is billed to the
    /// session whatever the answer.
    #[tracing::instrument(name = "rlm_decide", skip_all)]
    async fn decide(&self, request: &DecisionRequest) -> anyhow::Result<Decision> {
        let agent = self
            .agent
            .upgrade()
            .ok_or_else(|| anyhow!("rlm.decide: the session is closed"))?;
        let state = agent.state().await;
        let mut prompt = format!("Question: {}\n", request.question.trim());
        if let Some(context) = request.context.as_deref().filter(|c| !c.trim().is_empty()) {
            let _ = write!(prompt, "\nContext:\n{}\n", context.trim_end());
        }
        prompt.push_str("\nOptions:\n");
        for (number, option) in (1..).zip(&request.options) {
            let _ = writeln!(prompt, "{number}. {option}");
        }
        prompt.push_str("\nReply with the number of one option.");
        let context = LlmContext {
            system_prompt: Some(DECIDE_INSTRUCTION.to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text(prompt),
                timestamp: pa_agent::now_ms(),
            })],
            tools: Vec::new(),
        };
        let options = StreamRequestOptions {
            max_tokens: Some(DECIDE_MAX_OUTPUT_TOKENS),
            reasoning: state.thinking_level,
            session_id: agent.session_id().map(str::to_string),
            // Like the goal judge: never take the session's Codex socket or its continuation.
            transport: Some(pa_agent::types::Transport::Sse),
            ..StreamRequestOptions::default()
        };
        let call = async {
            let mut stream = (self.stream_fn)(state.model, context, options).await?;
            while stream.next_event().await.is_some() {}
            stream.result().await
        };
        let reply = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| {
                anyhow!(
                    "rlm.decide: the model did not answer within {:?}",
                    self.timeout
                )
            })??;
        let usage = serde_json::to_value(&reply.usage)
            .and_then(serde_json::from_value::<pa_types::ai::Usage>)
            .ok()
            .and_then(|usage| {
                ModelUsage::new(DECIDE_USAGE_SOURCE, &reply.provider, &reply.model, usage)
            });
        let tokens = usage.as_ref().map_or(0, |usage| usage.usage.total_tokens);
        if let Some(usage) = usage {
            let session_id = agent.session_id().unwrap_or_default().to_string();
            if let Err(error) = self.usage_store.append_model_usage(session_id, usage).await {
                tracing::warn!("rlm.decide usage row not persisted: {error}");
            }
        }
        if reply.stop_reason == StopReason::Error {
            bail!("rlm.decide: {}", reply.error_message.unwrap_or_default());
        }
        let text: String = reply
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
            })
            .collect();
        let index = parse_choice(&text, &request.options).ok_or_else(|| {
            anyhow!(
                "rlm.decide: the model's answer {:?} names none of the {} options",
                text.trim(),
                request.options.len()
            )
        })?;
        Ok(Decision { index, tokens })
    }
}

/// The decision counts of the most recent cells.
#[derive(Debug, Default)]
struct CellBudgets {
    cells: VecDeque<(String, u32)>,
}

impl CellBudgets {
    /// Take one decision for `cell`: the decisions it has now used, or `None` when it had
    /// already used [`DECISIONS_PER_CELL`].
    fn take(&mut self, cell: &str) -> Option<u32> {
        let position = self.cells.iter().position(|(id, _)| id == cell);
        let used = match position {
            Some(position) => &mut self.cells[position].1,
            None => {
                if self.cells.len() == TRACKED_CELLS {
                    self.cells.pop_front();
                }
                self.cells.push_back((cell.to_string(), 0));
                &mut self.cells.back_mut().expect("just pushed").1
            }
        };
        if *used >= DECISIONS_PER_CELL {
            return None;
        }
        *used += 1;
        Some(*used)
    }
}

/// The `rlm.decide` host side of one session: the per-cell bound, the late-bound decider (the
/// handler registers before the agent exists), and the telemetry observer.
#[derive(Default)]
pub(crate) struct DecisionHost {
    decider: OnceLock<Decider>,
    budgets: Mutex<CellBudgets>,
    observer: Option<DecideObserver>,
}

impl DecisionHost {
    pub(crate) fn new(observer: Option<DecideObserver>) -> Self {
        Self {
            observer,
            ..Self::default()
        }
    }

    /// Bind the session's decider (the engine, once its agent exists). A second bind is ignored.
    pub(crate) fn bind(&self, decider: Decider) {
        let _ = self.decider.set(decider);
    }

    fn note(&self, fact: DecideFact) {
        if let Some(observer) = &self.observer {
            observer(fact);
        }
    }

    /// Answer one `rlm.decide` request: `{index, choice, row, used, limit}`.
    async fn answer(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let Some(used) = self
            .budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(&request.cell)
        else {
            self.note(DecideFact::Limited);
            bail!(
                "rlm.decide: this cell used its {DECISIONS_PER_CELL} decisions; decide the rest in code or end the cell and decide in the next turn"
            );
        };
        let decider = self
            .decider
            .get()
            .ok_or_else(|| anyhow!("rlm.decide is not available in this session yet"))?;
        let decision = match decider.decide(&request).await {
            Ok(decision) => decision,
            Err(error) => {
                self.note(DecideFact::Failed);
                return Err(error);
            }
        };
        self.note(DecideFact::Decided);
        let choice = &request.options[decision.index];
        Ok(json!({
            "index": decision.index,
            "choice": choice,
            "row": decision_row(&request.question, choice),
            "used": used,
            "limit": DECISIONS_PER_CELL,
            "tokens": decision.tokens,
        }))
    }
}

/// Register `rlm.decide` over `host`.
pub(crate) fn register_decide_handler(
    handlers: &mut HostRequestHandlers,
    host: &Arc<DecisionHost>,
) {
    let host = Arc::clone(host);
    handlers.register(
        DECIDE_REQUEST_TYPE,
        host_handler(move |payload| {
            let host = Arc::clone(&host);
            async move { host.answer(&payload.data).await }
        }),
    );
}

fn bounded_text(value: Option<&Value>, field: &str, max_chars: usize) -> anyhow::Result<String> {
    let text = value
        .and_then(Value::as_str)
        .with_context(|| format!("rlm.decide: {field} must be a string"))?;
    if text.trim().is_empty() {
        bail!("rlm.decide: {field} must be non-empty");
    }
    let count = text.chars().count();
    if count > max_chars {
        bail!("rlm.decide: {field} is at most {max_chars} characters (got {count})");
    }
    Ok(text.to_string())
}

fn parse_request(data: &Value) -> anyhow::Result<DecisionRequest> {
    let question = bounded_text(data.get("question"), "question", QUESTION_CHARS_MAX)?;
    let raw_options = data
        .get("options")
        .and_then(Value::as_array)
        .context("rlm.decide: options must be a list of strings")?;
    if !(2..=OPTIONS_MAX).contains(&raw_options.len()) {
        bail!(
            "rlm.decide: options must hold 2 to {OPTIONS_MAX} choices (got {})",
            raw_options.len()
        );
    }
    let mut options = Vec::with_capacity(raw_options.len());
    for option in raw_options {
        let option = bounded_text(Some(option), "every option", OPTION_CHARS_MAX)?;
        if options.contains(&option) {
            bail!("rlm.decide: options must be distinct");
        }
        options.push(option);
    }
    let context = match data.get("context") {
        None | Some(Value::Null) => None,
        Some(Value::String(context)) => {
            let count = context.chars().count();
            if count > CONTEXT_CHARS_MAX {
                bail!(
                    "rlm.decide: context is at most {CONTEXT_CHARS_MAX} characters (got {count})"
                );
            }
            Some(context.clone())
        }
        Some(other) => bail!("rlm.decide: context must be a string, got {other}"),
    };
    let cell = data
        .get("cell")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(DecisionRequest {
        question,
        options,
        context,
        cell,
    })
}

/// The option an answer names: its first number in range, else an option's exact text.
fn parse_choice(answer: &str, options: &[String]) -> Option<usize> {
    let answer = answer.trim();
    let number = answer
        .split(|c: char| !c.is_ascii_digit())
        .find(|token| !token.is_empty())
        .and_then(|token| token.parse::<usize>().ok());
    if let Some(number) = number.filter(|number| (1..=options.len()).contains(number)) {
        return Some(number - 1);
    }
    let bare = answer.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.');
    options
        .iter()
        .position(|option| option.trim().eq_ignore_ascii_case(bare))
}

/// The compact transcript row of one decision: `[decide] question → choice`, the question's
/// first line cut to [`ROW_QUESTION_CHARS`].
fn decision_row(question: &str, choice: &str) -> String {
    let first_line = question.trim().lines().next().unwrap_or_default();
    let mut shown: String = first_line.chars().take(ROW_QUESTION_CHARS).collect();
    if shown.chars().count() < question.trim().chars().count() {
        shown.push('…');
    }
    format!("[decide] {shown} → {choice}")
}

#[cfg(test)]
mod tests;
