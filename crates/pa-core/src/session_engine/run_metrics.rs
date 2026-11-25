//! Per-run harness metrics: what one run (a user message to its `agent_end`)
//! cost in model requests, eval cells, tokens and cache, and how it ended.
//!
//! One accounting, two feeds. [`MetricsMeter`] folds a run's facts: each
//! settled model request, each tool result, each compaction, each subagent
//! spawn. The live session telemetry feeds it from the agent events and
//! writes the finished [`RunMetrics`] onto `agent run completed`
//! (`super::telemetry`); [`transcript_run_metrics`] feeds the same meter
//! from a saved session file, so an offline runner reads the numbers the
//! live event carries.
//!
//! Every value is a count, a token sum, a duration, a ratio or a fixed
//! vocabulary word: no prompt, output, tool argument or file content.
//!
//! The cache-prefix break: a request whose cache read falls more than
//! [`CACHE_BREAK_TOLERANCE_TOKENS`] below the previous request's whole
//! prompt (uncached input + cache read + cache write) re-sent part of a
//! prefix the provider had already seen, uncached. A request is compared
//! only against the previous settled request of the same provider and
//! model, and only once that provider has reported cache activity (a cache
//! read or write) on an earlier request: a provider that never reports
//! cache tokens has no comparison, and its runs report `cache_breaks` as
//! `None`, never zero. A compaction resets the baseline (the summary
//! replaced the prefix on purpose and is counted as a compaction instead),
//! and an errored or aborted request neither compares nor moves it. This
//! is the provider's reading (telemetry `cache_read_break_count`), so it
//! also catches a cache that expired or a request routed to a cold
//! replica, which no check of the bytes the harness sent can see.

mod transcript;

#[cfg(test)]
mod tests;

use pa_agent::types::{AssistantContent, AssistantMessage, StopReason};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::provider_retry::{provider_stream_failure_kind, REPETITION_LOOP_KIND};

pub use transcript::transcript_run_metrics;

/// How far a request's cache read may fall below the previous prompt
/// before it counts as a break. Providers cache in blocks (`OpenAI` in
/// 128-token steps after a 1,024-token minimum; Gemini's implicit cache has
/// its own granularity), so a read a little under the previous prompt is a
/// normal append; every break that matters for cost is far larger.
pub const CACHE_BREAK_TOLERANCE_TOKENS: u64 = 1024;

/// The tool result details key (`true`) with which the repeated-call loop
/// guard (`session_engine::loop_guard`) marks the result whose turn it ended.
pub(crate) const LOOP_GUARD_STOP_DETAIL: &str = "loopGuardStop";

/// Whether a tool result's `details` say the loop guard ended the turn
/// there ([`LOOP_GUARD_STOP_DETAIL`]).
fn loop_guard_stopped(details: &Value) -> bool {
    details.get(LOOP_GUARD_STOP_DETAIL).and_then(Value::as_bool) == Some(true)
}

/// How a run ended: the `stop_reason` vocabulary of its last model request,
/// plus the two endings that vocabulary folds away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnEndReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
    /// The run made no model request.
    #[serde(rename = "unknown")]
    Unknown,
    /// A final answer with no visible text and no tool call.
    #[serde(rename = "empty")]
    Empty,
    /// The stream loop guard cut the reply for repeating itself (an error
    /// of kind `repetition_loop`), or the repeated-call guard ended the
    /// turn after the same `eval` cell kept returning the same result.
    #[serde(rename = "loop_guard")]
    LoopGuard,
    /// Automatic compaction could not reduce the context and stopped the
    /// run (`super::compaction_refill`).
    #[serde(rename = "context_unreducible")]
    ContextUnreducible,
}

impl TurnEndReason {
    /// The wire word (the telemetry `end_reason` vocabulary).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TurnEndReason::Stop => "stop",
            TurnEndReason::Length => "length",
            TurnEndReason::ToolUse => "toolUse",
            TurnEndReason::Error => "error",
            TurnEndReason::Aborted => "aborted",
            TurnEndReason::Unknown => "unknown",
            TurnEndReason::Empty => "empty",
            TurnEndReason::LoopGuard => "loop_guard",
            TurnEndReason::ContextUnreducible => "context_unreducible",
        }
    }
}

/// A run's tokens: input split by how the provider billed it, and output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSplit {
    /// Prompt tokens neither read from nor written to the cache (the
    /// usage's `input`).
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

/// One compaction counted into the run.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompactionPoint {
    /// The context the last settled reply reported (prompt + output) over
    /// that model's window in the compiled catalog; `None` when the run had
    /// no settled reply since the last compaction or the model is not in
    /// the catalog.
    pub context_fraction: Option<f64>,
}

/// Whether the feed sees subagent spawns: the live session counts each
/// admitted `rlm-spawn`; a session file records none (the spawn ledger is
/// a separate file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnVisibility {
    Observed,
    NotRecorded,
}

/// One run's metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Model requests (steps), failed ones included.
    pub model_requests: u64,
    /// Requests that ended in an error.
    pub failed_model_requests: u64,
    /// Tool results of the kernel tool (`eval`; the legacy Python names too).
    pub eval_cells: u64,
    /// The eval cells' own reported durations, summed (bash waits included).
    pub eval_cell_ms: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub tokens: TokenSplit,
    /// Cache read over all prompt tokens, token-weighted; `None` without
    /// prompt tokens.
    pub cache_hit_rate: Option<f64>,
    /// Cache-prefix breaks (module docs); `None` when no request of the run
    /// could be compared.
    pub cache_breaks: Option<u64>,
    /// Bounded `bash` awaits that finished inside their window / yielded.
    pub bash_waits_completed: u64,
    pub bash_waits_yielded: u64,
    /// Admitted subagent spawns; `None` where the feed cannot see them.
    pub subagent_spawns: Option<u64>,
    /// Replies cut off at the output limit that the run continued behind a
    /// guiding row (`AgentSession::continue_after_output_limit`).
    pub output_limit_continuations: u64,
    pub compactions: Vec<CompactionPoint>,
    pub end_reason: TurnEndReason,
    /// Live: `agent_start` to `agent_end`. Offline: the run's first message
    /// timestamp to its last.
    pub wall_ms: u64,
}

/// The previous settled request, the baseline the next one is compared to.
#[derive(Debug)]
struct CacheBaseline {
    provider: String,
    model: String,
    prompt: u64,
    /// This provider and model reported a cache read or write on this or
    /// an earlier request.
    reports_cache: bool,
}

/// The open run's running tallies.
#[derive(Debug, Default)]
struct RunTally {
    model_requests: u64,
    failed_model_requests: u64,
    eval_cells: u64,
    eval_cell_ms: u64,
    tool_calls: u64,
    tool_errors: u64,
    tokens: TokenSplit,
    compared_requests: u64,
    cache_breaks: u64,
    bash_waits_completed: u64,
    bash_waits_yielded: u64,
    subagent_spawns: u64,
    output_limit_continuations: u64,
    compactions: Vec<CompactionPoint>,
    end_reason: Option<TurnEndReason>,
}

/// The accumulator behind [`RunMetrics`]. It lives as long as the session:
/// the cache baseline crosses run boundaries (a run's first request reads
/// the prefix the previous run left), the tallies reset at
/// [`MetricsMeter::finish_run`].
#[derive(Debug)]
pub struct MetricsMeter {
    spawns: SpawnVisibility,
    baseline: Option<CacheBaseline>,
    /// The last settled reply's context tokens and its model's window.
    last_context: Option<(u64, Option<u64>)>,
    run: RunTally,
}

impl MetricsMeter {
    #[must_use]
    pub fn new(spawns: SpawnVisibility) -> Self {
        Self {
            spawns,
            baseline: None,
            last_context: None,
            run: RunTally::default(),
        }
    }

    /// One model request settled (its assistant message ended).
    pub fn request(&mut self, message: &AssistantMessage) {
        let run = &mut self.run;
        let usage = &message.usage;
        run.model_requests += 1;
        run.tokens.uncached_input += usage.input;
        run.tokens.cache_read += usage.cache_read;
        run.tokens.cache_write += usage.cache_write;
        run.tokens.output += usage.output;
        let end_reason = match message.stop_reason {
            StopReason::Stop => {
                let visible = message.content.iter().any(|block| match block {
                    AssistantContent::Text(text) => !text.text.trim().is_empty(),
                    AssistantContent::ToolCall(_) => true,
                    AssistantContent::Thinking(_) => false,
                });
                if visible {
                    TurnEndReason::Stop
                } else {
                    TurnEndReason::Empty
                }
            }
            StopReason::Length => TurnEndReason::Length,
            StopReason::ToolUse => TurnEndReason::ToolUse,
            StopReason::Error => {
                if provider_stream_failure_kind(message).as_deref() == Some(REPETITION_LOOP_KIND) {
                    TurnEndReason::LoopGuard
                } else {
                    TurnEndReason::Error
                }
            }
            StopReason::Aborted => TurnEndReason::Aborted,
        };
        run.end_reason = Some(end_reason);
        match message.stop_reason {
            StopReason::Error => {
                run.failed_model_requests += 1;
                return;
            }
            StopReason::Aborted => return,
            StopReason::Stop | StopReason::Length | StopReason::ToolUse => {}
        }
        let prompt = usage.input + usage.cache_read + usage.cache_write;
        if prompt == 0 {
            // No usage reported: nothing to compare and nothing to compare to.
            return;
        }
        let previous = self.baseline.take().filter(|baseline| {
            baseline.provider == message.provider && baseline.model == message.model
        });
        let reports_cache = usage.cache_read > 0
            || usage.cache_write > 0
            || previous
                .as_ref()
                .is_some_and(|baseline| baseline.reports_cache);
        if let Some(previous) = previous.filter(|baseline| baseline.reports_cache) {
            run.compared_requests += 1;
            if usage.cache_read + CACHE_BREAK_TOLERANCE_TOKENS < previous.prompt {
                run.cache_breaks += 1;
            }
        }
        self.baseline = Some(CacheBaseline {
            provider: message.provider.clone(),
            model: message.model.clone(),
            prompt,
            reports_cache,
        });
        let window = pa_ai::models_generated::get_model(&message.provider, &message.model)
            .map(|model| model.context_window)
            .filter(|window| *window > 0);
        self.last_context = Some((prompt + usage.output, window));
    }

    /// One tool result. `details` is the result's details object (`Null`
    /// when it has none).
    pub fn tool_result(&mut self, tool_name: &str, is_error: bool, details: &Value) {
        let run = &mut self.run;
        run.tool_calls += 1;
        run.tool_errors += u64::from(is_error);
        if loop_guard_stopped(details) {
            run.end_reason = Some(TurnEndReason::LoopGuard);
        }
        if pa_types::ai::kernel_tool_language(tool_name).is_some() {
            run.eval_cells += 1;
            run.eval_cell_ms += details
                .get("durationMs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if let Some(waits) = details.get("bashWaits") {
            let count = |key: &str| waits.get(key).and_then(Value::as_u64).unwrap_or(0);
            run.bash_waits_completed += count("completed");
            run.bash_waits_yielded += count("yielded");
        }
    }

    /// A compaction counted into the run: it records how full the context
    /// was and resets the cache baseline.
    pub fn compaction(&mut self) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below f64's 2^53 exact-integer range"
        )]
        let context_fraction = self
            .last_context
            .take()
            .and_then(|(tokens, window)| window.map(|window| tokens as f64 / window as f64));
        self.run
            .compactions
            .push(CompactionPoint { context_fraction });
        self.baseline = None;
    }

    /// Automatic compaction stopped the run because it could not reduce
    /// the context: the run ends as [`TurnEndReason::ContextUnreducible`].
    pub fn context_unreducible(&mut self) {
        self.run.end_reason = Some(TurnEndReason::ContextUnreducible);
    }

    /// One admitted subagent spawn.
    pub fn subagent_spawn(&mut self) {
        self.run.subagent_spawns += 1;
    }

    /// One reply cut off at the output limit, continued behind a guiding row.
    pub fn output_limit_continuation(&mut self) {
        self.run.output_limit_continuations += 1;
    }

    /// Close the open run: its metrics, and fresh tallies for the next one.
    pub fn finish_run(&mut self, wall_ms: u64) -> RunMetrics {
        let run = std::mem::take(&mut self.run);
        let tokens = run.tokens;
        let prompt = tokens.uncached_input + tokens.cache_read + tokens.cache_write;
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below f64's 2^53 exact-integer range"
        )]
        let cache_hit_rate = (prompt > 0).then(|| tokens.cache_read as f64 / prompt as f64);
        RunMetrics {
            model_requests: run.model_requests,
            failed_model_requests: run.failed_model_requests,
            eval_cells: run.eval_cells,
            eval_cell_ms: run.eval_cell_ms,
            tool_calls: run.tool_calls,
            tool_errors: run.tool_errors,
            tokens,
            cache_hit_rate,
            cache_breaks: (run.compared_requests > 0).then_some(run.cache_breaks),
            bash_waits_completed: run.bash_waits_completed,
            bash_waits_yielded: run.bash_waits_yielded,
            subagent_spawns: match self.spawns {
                SpawnVisibility::Observed => Some(run.subagent_spawns),
                SpawnVisibility::NotRecorded => None,
            },
            output_limit_continuations: run.output_limit_continuations,
            compactions: run.compactions,
            end_reason: run.end_reason.unwrap_or(TurnEndReason::Unknown),
            wall_ms,
        }
    }
}
