//! Session telemetry: the agent-event state machine behind the session
//! events `agent started`, `agent run completed` (one per user turn, every
//! per-call fact folded into it as aggregates), and `agent session ended`
//! (with the per-session counters); the TUI client reports `agent command
//! used`. Behavioral port of the TS `installAgentTelemetry` subscriber
//! (`packages/coding-agent/src/core/telemetry.ts`) plus the #2117 v2
//! enrichment.
//!
//! Divergence from the TS state machine, documented: the TS subscriber tracks
//! `turnActionActive` because the TS session loop can span several agent runs
//! inside one queued turn action. The Rust loop pairs every `AgentStart` with
//! exactly one `AgentEnd` per admitted run, so one run == one
//! `AgentStart..AgentEnd` window and no turn-action tracking is needed.
//! A retried turn re-enters the loop, so each retry attempt is its own
//! window: the failed window reports the error occurrence, the retry
//! window reports the recovery.
//!
//! Privacy contract: this module emits counter/duration/category facts only —
//! never prompt text, model output, tool arguments or results. Failed
//! model calls classify through [`super::error_classify`]: only fixed
//! diagnostics and reviewed fixed strings ride events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pa_agent::agent::Subscription;
use pa_agent::types::{AgentEvent, AssistantMessage, StopReason, Usage};
use pa_telemetry::{
    base_properties, Properties, RunTrigger, TelemetryClient, TelemetryClientConfig, ToolCategory,
};
use pa_types::question::QuestionStatus;
use pa_types::todo::{TodoList, TodoStatus};
use serde_json::Value;

use super::auto_retry::AutoRetryEvent;
use super::error_classify::classify_error_message;
use super::provider_retry::{provider_stream_failure_kind, REPETITION_LOOP_KIND};
use super::run_metrics::{MetricsMeter, SpawnVisibility};

mod track;
pub use track::{
    track_catalog_refresh, track_compaction_abort_declared, track_daemon_event_summary,
    track_deleted_child_usage_captured, track_model_refused, track_provider_oauth_login,
    track_session_crashed, track_sessions_imported, track_worker_adoption,
    track_worker_children_closed, OAuthLoginSurface,
};

mod classify;
pub use classify::provider_category;

mod capability_counts;
use capability_counts::CapabilityCounts;

mod bash_quiet;
mod builtin_shell;
mod call_refusals;
mod edit_checks;
mod edit_matches;
mod error_enrichment;
mod hypotheses;
mod prefetch;
mod previews;
mod readonly_cache;
mod rizin;
mod symbol_index;
mod test_summaries;

mod status;
use classify::{error_category, model_category, opt_value, run_outcome};
pub use status::{
    set_telemetry_enabled_text, telemetry_endpoint, telemetry_status_text, telemetry_switch,
    TelemetrySwitch, TELEMETRY_SWITCH_VARS,
};

#[cfg(test)]
mod tests;

pub const EXECUTION_MODE_UNKNOWN: &str = "unknown";

/// Telemetry wiring supplied by the composition root (`SessionEngineConfig`).
/// `None` telemetry (opt-out) installs nothing.
/// A session's live opt-out switch: the enabled answer the recording
/// The recording seams ask the switch at turn boundaries (run and turn
/// starts) and cache the answer for the events in between: a mid-turn
/// opt-out is observed at the next boundary, and the client's flush
/// drops everything queued while the switch is off.
#[derive(Clone)]
pub struct RecordingSwitch {
    /// Env-then-settings resolution, asked live at the turn boundaries:
    /// false means telemetry is off right now.
    pub enabled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl RecordingSwitch {
    /// A plain switch for the seams: recording gates live on `enabled`
    /// at the turn boundaries.
    #[must_use]
    pub fn test(enabled: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self { enabled }
    }
}

pub struct TelemetryWiring {
    /// The shared client (base properties are stamped here, per event).
    pub client: TelemetryClient,
    pub execution_mode: Option<String>,
    /// Injectable clock (millis since epoch); defaults to system time.
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    /// The live opt-out switch the recording seams consult at the turn
    /// boundaries: while it answers false there, the run state machine
    /// severs and nothing records until the next boundary, and the
    /// client drops captures queued while the switch is off. `None` is
    /// always on (tests and one-shot paths).
    pub telemetry_enabled: Option<RecordingSwitch>,
}

/// Installed session telemetry: the subscription plus the state the live
/// agent events feed; finalizes the session on `end()`.
pub struct SessionTelemetry {
    client: TelemetryClient,
    state: Arc<Mutex<TelemetryState>>,
    execution_mode: String,
    counters: Arc<SessionCounters>,
    /// `end()` runs exactly once (session close and later kill/shutdown
    /// paths may both reach it; only the first emits the ended event).
    ended: std::sync::atomic::AtomicBool,
    /// The agent's session id: the provider layer counts its Responses transport choices under
    /// it (`agent responses transport`).
    agent_session_id: Option<String>,
    _subscription: Option<Subscription>,
}

impl SessionTelemetry {
    /// A handle with no live subscription, for tests driving the state machine directly.
    #[cfg(test)]
    pub(crate) fn detached(
        client: TelemetryClient,
        state: Arc<Mutex<TelemetryState>>,
        execution_mode: String,
    ) -> Self {
        Self {
            client,
            state,
            execution_mode,
            counters: Arc::default(),
            ended: std::sync::atomic::AtomicBool::new(false),
            agent_session_id: None,
            _subscription: None,
        }
    }
}

/// Everything the subscriber accumulates. One mutex: the agent delivers events
/// serially, but the tracker is also fed from compaction call sites outside
/// the event stream.
pub(crate) struct TelemetryState {
    session_id: String,
    started_at: u64,
    totals: SessionTotals,
    active_run: Option<ActiveRun>,
    tool_starts: HashMap<String, u64>,
    /// The live opt-out switch from the wiring: asked at the turn
    /// boundaries, never per event.
    telemetry_enabled: Option<RecordingSwitch>,
    /// The cached switch answer from the last turn boundary. Recording
    /// on any other event consults only this flag, so a streaming delta
    /// never re-reads and re-parses the settings file.
    recording: bool,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The process-wide request-auth resolver counters at the start of the
    /// session's recorded window: `agent session ended` folds the delta
    /// (one daemon worker process serves one session). A boundary that
    /// observes an off period moves it forward, so the off window's
    /// exchanges never ride a later event.
    auth_baseline: crate::auth::resolver::AuthMetrics,
    /// The per-run harness metrics (`super::run_metrics`), fed only while a
    /// run is active; its cache baseline spans runs.
    meter: MetricsMeter,
}

/// Is recording on right now? `None` (tests, one-shot paths) is always
/// on; a set switch answers live. Called from under the state lock, so
/// the switch itself must never lock the telemetry state.
fn recording_on(state: &TelemetryState) -> bool {
    state
        .telemetry_enabled
        .as_ref()
        .is_none_or(|telemetry_switch| (telemetry_switch.enabled)())
}

/// While telemetry is off, the active run (if any) is severed: dropped
/// without emitting, together with its in-flight tables. The switch is
/// asked at the turn boundaries, so a run that spans an opt-out never
/// completes after the boundary that observes the off period — neither
/// its facts nor the off window's timing ride a later event.
fn sever_off_period_run(state: &mut TelemetryState) {
    state.active_run = None;
    state.tool_starts.clear();
    // The off window's requests are unseen: no baseline survives it.
    state.meter = MetricsMeter::new(SpawnVisibility::Observed);
}

/// One turn boundary (a run or turn start): ask the live switch, cache
/// the answer for the events until the next boundary, and cut the run
/// when the switch says off. Between boundaries nothing re-reads the
/// settings file — the client's flush drops everything queued while the
/// switch is off, which covers a mid-turn opt-out.
fn observe_turn_boundary(state: &mut TelemetryState) {
    let was_recording = state.recording;
    state.recording = recording_on(state);
    if !state.recording {
        sever_off_period_run(state);
    }
    if !(was_recording && state.recording) {
        state.auth_baseline = crate::auth::resolver::AuthMetrics::current();
    }
}

#[derive(Default)]
struct SessionTotals {
    run_count: u64,
    successful_run_count: u64,
    failed_run_count: u64,
    aborted_run_count: u64,
    prompt_count: u64,
    tool_call_count: u64,
    compaction_count: u64,
    retry_count: u64,
    model_error_count: u64,
    usage: UsageTotals,
}

#[derive(Default)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
    model_call_count: u64,
}

impl UsageTotals {
    fn add(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.total_tokens += usage.total_tokens;
        self.model_call_count += 1;
    }

    fn merge(&mut self, other: &UsageTotals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_tokens += other.total_tokens;
        self.model_call_count += other.model_call_count;
    }
}

// The run's independent lifecycle flags (ended, retry pending, trigger
// pending, usage complete); an enum would not change the flow.
#[allow(clippy::struct_excessive_bools)]
struct ActiveRun {
    started_at: u64,
    /// `AgentEnd` fired but not finalized yet: the post-run compaction drain
    /// still counts into it (finalizes at the next `AgentStart` or session end).
    ended: bool,
    /// Wall time of `AgentEnd`: the run's duration freezes here; deferring
    /// the finalize must not stretch it across the idle gap.
    ended_at: Option<u64>,
    first_turn_started_at: Option<u64>,
    first_model_event_ms: Option<u64>,
    visible_ttft_ms: Option<u64>,
    current_turn_started_at: Option<u64>,
    model_latency_ms: u64,
    max_model_latency_ms: u64,
    turn_count: u64,
    tool_call_count: u64,
    tool_error_count: u64,
    compaction_count: u64,
    retry_count: u64,
    usage: UsageTotals,
    last_assistant: Option<AssistantMessage>,
    /// An auto-retry started after this run's
    /// failed attempt: the retried attempt's `AgentStart` continues this
    /// run instead of starting a new one (TS: one run per turn, retries
    /// counted in `retry_count`).
    retry_pending: bool,
    // v2 (#2117) per-run tracking:
    /// The run's uuid.
    run_id: String,
    /// 1-based ordinal of this run in the session.
    run_index: u64,
    /// The trigger is still undecided: a prompt-run emits its user
    /// `MessageStart` right after `AgentStart`; a continuation-run goes
    /// straight to model events.
    trigger_pending: bool,
    trigger: RunTrigger,
    first_reasoning_ms: Option<u64>,
    run_to_first_text_ms: Option<u64>,
    /// Sum of tool execution durations (the `tool` timing stage).
    tool_duration_ms: u64,
    /// Sum of auto-retry delays (the `retry_wait` timing stage).
    retry_wait_ms: u64,
    /// The largest gap between consecutive model stream events.
    max_stream_gap_ms: Option<u64>,
    last_stream_event_at: Option<u64>,
    successful_model_call_count: u64,
    /// False once a model call ended in an error (#2117: pending or
    /// failed calls make usage incomplete).
    usage_complete: bool,
    /// Summed usage cost in USD (estimated; null when incomplete or
    /// pricing was unknown - the conservative direction).
    cost_usd: f64,
    /// Each model call's latency (the p50 on the run event).
    model_latencies: Vec<u64>,
    /// Failed model calls in the run (every retried attempt included).
    model_error_count: u64,
    /// Failed model calls by TS `error_category`.
    error_category_counts: std::collections::BTreeMap<&'static str, u64>,
    /// Summed compaction durations inside the run.
    compaction_duration_ms: u64,
    /// Per-tool aggregates, keyed by the fixed tool category (built-in
    /// tools by name, every MCP and custom tool folded into `mcp` /
    /// `custom`: no raw tool names leave the machine).
    tool_summary: HashMap<ToolCategory, ToolCategoryStats>,
    /// The stream loop guard's interventions in the run.
    loop_guard: LoopGuardCounts,
    /// The run's loop requests that changed bytes inside the previous
    /// request's prefix (compaction included), and the previous requests'
    /// bytes they did not keep (`session_engine::cache_prefix`).
    cache_prefix_breaks: u64,
    cache_prefix_break_bytes: u64,
}

/// One run's stream loop guard interventions (`session_engine::loop_guard`).
#[derive(Debug, Default)]
struct LoopGuardCounts {
    /// Model calls the guard cut for repeating themselves.
    trips: u64,
    /// Retries started after such a cut.
    retries: u64,
    /// Tool results that carried the repeated-call nudge.
    tool_repeat_nudges: u64,
}

/// One tool category's per-run aggregates.
#[derive(Debug, Default)]
struct ToolCategoryStats {
    calls: u64,
    failures: u64,
    duration_ms: u64,
    max_duration_ms: u64,
}

/// Per-session counters for the frequent per-occurrence facts that ride
/// `agent session ended` instead of their own events (skill invocations,
/// MCP connector use, kernel boots, RLM child usage, feature outcomes).
/// Shared with the engine seams that report them.
#[derive(Default)]
pub struct SessionCounters {
    inner: Mutex<CounterValues>,
    /// The live opt-out switch, installed by the engine at the counters'
    /// creation, before the MCP and kernel seams that count into them
    /// capture their handles. While it answers false the counters stop
    /// recording, so a later enable never sends what happened while
    /// telemetry was off (the TUI counters' rule). `None` (the default,
    /// used by tests and one-shot paths) is always on.
    telemetry_enabled: std::sync::OnceLock<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// `agent_message.wait` outcomes for one session (counts only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AgentMessageWaitCounts {
    message: u64,
    timeout: u64,
    cycle: u64,
    no_sender: u64,
}

#[derive(Default)]
struct CounterValues {
    skill_use_count: u64,
    mcp_connector_use_count: u64,
    kernel_bootstrap_count: u64,
    kernel_bootstrap_cold_count: u64,
    kernel_bootstrap_failed_count: u64,
    kernel_bootstrap_max_ms: u64,
    rlm_child_usage_count: u64,
    rlm_child_input_tokens: u64,
    rlm_child_output_tokens: u64,
    rlm_child_cache_read_tokens: u64,
    rlm_child_cache_write_tokens: u64,
    rlm_child_cost: f64,
    web_search_count: u64,
    web_search_failed_count: u64,
    web_search_fallback_count: u64,
    web_search_keyless_count: u64,
    web_search_result_count: u64,
    web_search_max_ms: u64,
    /// Requests whose view thinned aged tool results, the results thinned
    /// across them, and the characters (and chars/4 tokens) not re-sent.
    context_thinned_request_count: u64,
    context_thinned_result_count: u64,
    context_thinned_chars_saved: u64,
    context_thinned_tokens_saved: u64,
    cache_warm_sent_count: u64,
    cache_warm_skipped_count: u64,
    cache_warm_cost_usd: f64,
    /// Subdirectory context files appended to the conversation (`session_engine::nested_context`).
    nested_context_file_count: u64,
    /// Net prompt-cache saving over the session's real requests: what the
    /// cache reads saved against uncached input, less the write premium.
    cache_savings_usd: f64,
    /// `feature_<name>_<outcome>_count` over the fixed feature vocabulary.
    feature_outcomes: std::collections::BTreeMap<String, u64>,
    /// Every kernel capability's adoption counts, by the catalog property they report as (the
    /// capability rule files in pa-telemetry): one generic counter for every capability event.
    capabilities: CapabilityCounts,
    /// The `agent goal verification` counters.
    goal: GoalVerificationCounts,
    /// The provider layer's Responses transport counts, reported by `agent responses transport`.
    responses_transport: pa_ai::responses_transport::ResponsesTransportCounts,
    /// The worker process's endpoint discovery counts, reported by `agent endpoint discovery`.
    endpoint_discovery: pa_ai::responses_transport::DiscoveryCounts,
    /// Recorded assistant calls by billing mode (only the calls count), reported by
    /// `agent billing modes`.
    billing_modes: pa_types::billing::BillingSplit,
    /// `agent_message.send` deliveries the per-sender rate bucket or a receiver's wake budget
    /// refused, reported by `agent messages`.
    agent_message_refusals: super::agent_messaging::AgentMessageRefusals,
    /// `agent_message.wait` outcomes, reported by `agent message waits`.
    agent_message_waits: AgentMessageWaitCounts,
    /// Failed cells and commands given their error locations' source lines,
    /// reported by `agent error enrichment`.
    error_enrichment: error_enrichment::ErrorEnrichmentCounts,
}

/// One goal verification or budget fact, counted into `agent goal verification`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalTelemetryFact {
    /// A `check` command ran.
    CheckRun,
    /// A `check` command ran and failed.
    CheckFailed,
    /// A failed `check` was not rerun over an unchanged workspace.
    CheckSkipped,
    /// A judge call ran.
    JudgeCall,
    /// A judge answered NOT MET (or cited no verifiable evidence).
    JudgeNotMet,
    /// The token budget stopped the goal.
    TokenBudgetStop,
    /// The dollar cap stopped the goal.
    CostBudgetStop,
}

/// One `/better` fact, counted into `agent better prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BetterTelemetryFact {
    /// A run started.
    Run,
    Completed,
    Cancelled,
    Failed,
    /// The user sent an inserted rewrite as is.
    SentAsIs,
    /// The user sent an inserted rewrite after editing it.
    Edited,
    /// The user never sent an inserted rewrite (or replaced most of it).
    Discarded,
    /// A brief-mode run started (`better(mode="brief")`, `rlm.spawn(better=True)`).
    BriefRun,
    /// A brief-mode run found nothing to fix.
    BriefUnchanged,
    /// A verification found unresolved references and ran a refine pass.
    VerificationPass,
    /// An acceptance-check round started at a run's end.
    CheckRound,
    /// One acceptance check of a round passed.
    CheckPassed,
    /// One acceptance check of a round failed.
    CheckFailed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct GoalVerificationCounts {
    check_runs: u64,
    check_failures: u64,
    check_skips: u64,
    judge_calls: u64,
    judge_not_met: u64,
    token_budget_stops: u64,
    cost_budget_stops: u64,
}

impl SessionCounters {
    /// Install the live opt-out switch. The engine calls this at the
    /// counters' creation, ahead of the MCP and kernel counting seams,
    /// so no event can count before the switch is in place. A second
    /// call is ignored (the first switch wins).
    pub fn set_telemetry_enabled(&self, telemetry_enabled: Arc<dyn Fn() -> bool + Send + Sync>) {
        let _ = self.telemetry_enabled.set(telemetry_enabled);
    }

    /// Count only while telemetry is on, so turning it on later never
    /// sends what happened while it was off.
    fn with(&self, update: impl FnOnce(&mut CounterValues)) {
        if self
            .telemetry_enabled
            .get()
            .is_some_and(|telemetry_enabled| !telemetry_enabled())
        {
            return;
        }
        update(
            &mut self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// One provider request whose view thinned aged tool results (counts
    /// only; no result text uploads).
    pub fn note_context_thinning(&self, stats: &super::context_thinning::ThinningStats) {
        self.with(|values| {
            values.context_thinned_request_count += 1;
            values.context_thinned_result_count += stats.thinned_results;
            values.context_thinned_chars_saved += stats.chars_saved;
            values.context_thinned_tokens_saved += stats.tokens_saved();
        });
    }

    /// A kernel `todo.set`/`todo.update` committed `next` over `previous`: a set counts once,
    /// and each item that became `done` counts once (text never uploads).
    pub fn note_todo_change(&self, previous: &TodoList, next: &TodoList) {
        let newly_done = next
            .items
            .iter()
            .filter(|item| {
                item.status == TodoStatus::Done
                    && !previous
                        .items
                        .iter()
                        .any(|old| old.id == item.id && old.status == TodoStatus::Done)
            })
            .count() as u64;
        let replaced = previous.items.len() != next.items.len()
            || previous
                .items
                .iter()
                .zip(&next.items)
                .any(|(old, new)| old.id != new.id || old.text != new.text);
        self.with(|values| {
            values
                .capabilities
                .add("todo_list_set_count", u64::from(replaced));
            values.capabilities.add("todo_item_done_count", newly_done);
        });
    }

    /// A kernel `ask(...)` settled with `status` (the question never uploads).
    pub fn note_question(&self, status: QuestionStatus) {
        let outcome = match status {
            QuestionStatus::Answered => "question_answered_count",
            QuestionStatus::Declined => "question_declined_count",
            QuestionStatus::TimedOut => "question_timed_out_count",
            QuestionStatus::NoUser => "question_no_user_count",
            QuestionStatus::Cancelled => "question_cancelled_count",
        };
        self.with(|values| {
            values.capabilities.add("question_asked_count", 1);
            values.capabilities.add(outcome, 1);
        });
    }

    /// Add `count` to one capability adoption property (a name its pa-telemetry rule file
    /// declares). Counts only: never a name, a path, or content.
    pub(crate) fn note_capability(&self, property: &'static str, count: u64) {
        self.with(|values| values.capabilities.add(property, count));
    }

    /// One goal verification or budget fact (counts only: never a command or judge text).
    pub fn note_goal(&self, fact: GoalTelemetryFact) {
        self.with(|values| {
            let counts = &mut values.goal;
            let counter = match fact {
                GoalTelemetryFact::CheckRun => &mut counts.check_runs,
                GoalTelemetryFact::CheckFailed => &mut counts.check_failures,
                GoalTelemetryFact::CheckSkipped => &mut counts.check_skips,
                GoalTelemetryFact::JudgeCall => &mut counts.judge_calls,
                GoalTelemetryFact::JudgeNotMet => &mut counts.judge_not_met,
                GoalTelemetryFact::TokenBudgetStop => &mut counts.token_budget_stops,
                GoalTelemetryFact::CostBudgetStop => &mut counts.cost_budget_stops,
            };
            *counter += 1;
        });
    }

    /// One `/better` fact (counts only: never a prompt, a rewrite, or a path).
    pub fn note_better(&self, fact: BetterTelemetryFact) {
        self.note_capability(
            match fact {
                BetterTelemetryFact::Run => "better_run_count",
                BetterTelemetryFact::Completed => "better_completed_count",
                BetterTelemetryFact::Cancelled => "better_cancelled_count",
                BetterTelemetryFact::Failed => "better_failed_count",
                BetterTelemetryFact::SentAsIs => "better_sent_as_is_count",
                BetterTelemetryFact::Edited => "better_edited_count",
                BetterTelemetryFact::Discarded => "better_discarded_count",
                BetterTelemetryFact::BriefRun => "better_brief_run_count",
                BetterTelemetryFact::BriefUnchanged => "better_brief_unchanged_count",
                BetterTelemetryFact::VerificationPass => "better_verification_pass_count",
                BetterTelemetryFact::CheckRound => "better_check_round_count",
                BetterTelemetryFact::CheckPassed => "better_check_passed_count",
                BetterTelemetryFact::CheckFailed => "better_check_failed_count",
            },
            1,
        );
    }

    /// One kernel `web.read` (counts only: never the URL or the content).
    /// The session's Responses transport counts (the provider layer keeps them; session end
    /// snapshots them here).
    pub(crate) fn note_responses_transport(
        &self,
        counts: pa_ai::responses_transport::ResponsesTransportCounts,
    ) {
        self.with(|values| values.responses_transport = counts);
    }

    /// The worker process's endpoint discovery counts (one worker serves one session; session end
    /// snapshots them here).
    pub(crate) fn note_endpoint_discovery(
        &self,
        counts: pa_ai::responses_transport::DiscoveryCounts,
    ) {
        self.with(|values| values.endpoint_discovery = counts);
    }

    pub(crate) fn note_web_read(
        &self,
        outcome: crate::web_read::ReadOutcome,
        elapsed: std::time::Duration,
    ) {
        use crate::web_read::{ReadKind, ReadOutcome};
        self.with(|values| {
            let counts = &mut values.capabilities;
            counts.add("web_read_count", 1);
            match outcome {
                ReadOutcome::Read { kind, truncated } => {
                    counts.add(
                        match kind {
                            ReadKind::Site => "web_read_site_count",
                            ReadKind::Pdf => "web_read_pdf_count",
                            ReadKind::Page => "web_read_page_count",
                        },
                        1,
                    );
                    counts.add("web_read_truncated_count", u64::from(truncated));
                }
                ReadOutcome::Failed => counts.add("web_read_failed_count", 1),
            }
            counts.raise(
                "web_read_max_ms",
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            );
        });
    }

    /// One kernel `fs.grep`/`fs.glob`/`fs.find` (counts only: never a pattern or a path).
    pub(crate) fn note_fs_search(&self, fact: crate::fs_search::SearchFact) {
        use crate::fs_search::FsOperation;
        self.with(|values| {
            let counts = &mut values.capabilities;
            counts.add(
                match fact.operation {
                    FsOperation::Grep => "fs_grep_count",
                    FsOperation::Glob => "fs_glob_count",
                    FsOperation::Find => "fs_find_count",
                },
                1,
            );
            match fact.results {
                Some(results) => counts.add("fs_search_result_count", results as u64),
                None => counts.add("fs_search_failed_count", 1),
            }
            counts.add("fs_search_truncated_count", u64::from(fact.truncated));
            counts.raise(
                "fs_search_max_ms",
                u64::try_from(fact.elapsed.as_millis()).unwrap_or(u64::MAX),
            );
        });
    }

    /// One kernel `fs.read`/`fs.edit` (counts only: never a path or content).
    /// One whole-file re-read of a seen file and how it was shown (never a path or content).
    pub(crate) fn note_reread(&self, fact: crate::fs_edit::RereadFact) {
        use crate::fs_edit::RereadFact;
        self.note_capability(
            match fact {
                RereadFact::Unchanged => "fs_reread_unchanged_count",
                RereadFact::Diff => "fs_reread_diff_count",
                RereadFact::Full => "fs_reread_full_count",
            },
            1,
        );
    }

    pub(crate) fn note_fs_edit(&self, fact: crate::fs_edit::EditFact) {
        use crate::fs_edit::{EditFact, FsEditOperation};
        self.with(|values| {
            let counts = &mut values.capabilities;
            match fact {
                EditFact::Read { lines } => {
                    counts.add("fs_read_count", 1);
                    counts.add("fs_read_line_count", lines as u64);
                }
                EditFact::Applied { edits } => {
                    counts.add("fs_edit_count", 1);
                    counts.add("fs_edit_op_count", edits as u64);
                }
                EditFact::Rejected { stale } => {
                    counts.add("fs_edit_rejected_count", 1);
                    counts.add("fs_edit_stale_anchor_count", stale as u64);
                }
                EditFact::Failed {
                    operation: FsEditOperation::Read,
                } => counts.add("fs_read_failed_count", 1),
                EditFact::Failed {
                    operation: FsEditOperation::Edit,
                } => counts.add("fs_edit_failed_count", 1),
                EditFact::Written { created } => {
                    counts.add("fs_write_count", 1);
                    counts.add("fs_write_created_count", u64::from(created));
                }
                EditFact::Failed {
                    operation: FsEditOperation::Write,
                } => counts.add("fs_write_failed_count", 1),
            }
        });
    }

    /// One kernel `schedule.add/list/cancel` call (counts only: never a prompt or a schedule).
    pub(crate) fn note_repl_schedule(&self, fact: super::runtime::ScheduleFact) {
        use super::runtime::ScheduleFact;
        self.note_capability(
            match fact {
                ScheduleFact::AddedOnce => "schedule_once_added_count",
                ScheduleFact::AddedRecurring => "schedule_recurring_added_count",
                ScheduleFact::Listed => "schedule_list_count",
                ScheduleFact::Cancelled => "schedule_cancelled_count",
                ScheduleFact::Rejected => "schedule_rejected_count",
            },
            1,
        );
    }

    /// One kernel `checkpoint.*` call (counts only: never a label or a path).
    pub(crate) fn note_repl_checkpoint(&self, fact: super::checkpoint_host::CheckpointFact) {
        use super::checkpoint_host::CheckpointFact;
        self.note_capability(
            match fact {
                CheckpointFact::Created => "checkpoint_created_count",
                CheckpointFact::Listed => "checkpoint_list_count",
                CheckpointFact::Restored => "checkpoint_restored_count",
                CheckpointFact::RestoreDeclined => "checkpoint_restore_declined_count",
                CheckpointFact::RestoreDisabled => "checkpoint_restore_disabled_count",
                CheckpointFact::Failed => "checkpoint_failed_count",
            },
            1,
        );
    }

    /// One step in a backgrounded cell's life (counts only: never code, output, or an id).
    pub(crate) fn note_background_cell(&self, fact: crate::kernel::shared::BackgroundCellFact) {
        use crate::kernel::shared::BackgroundCellFact;
        self.note_capability(
            match fact {
                BackgroundCellFact::Backgrounded => "cell_backgrounded_count",
                BackgroundCellFact::EndedOk { .. } => "cell_background_ok_count",
                BackgroundCellFact::EndedWithError { .. } => "cell_background_error_count",
                BackgroundCellFact::Interrupted => "cell_background_interrupted_count",
            },
            1,
        );
    }

    /// One `wait-until` wait, by how far it got (counts only: never a condition or a duration).
    pub(crate) fn note_cell_wait(&self, fact: crate::kernel::shared::CellWaitFact) {
        use crate::kernel::shared::CellWaitFact;
        self.note_capability(
            match fact {
                CellWaitFact::Started => "wait_until_count",
                CellWaitFact::Met => "wait_until_met_count",
                CellWaitFact::TimedOut => "wait_until_timed_out_count",
            },
            1,
        );
    }

    /// One cell armed with a yield window, by the window's source (a count: never a duration).
    pub(crate) fn note_cell_window(&self, source: crate::kernel::cell_window::CellWindowSource) {
        use crate::kernel::cell_window::CellWindowSource;
        self.note_capability(
            match source {
                CellWindowSource::Learned => "cell_window_learned_count",
                CellWindowSource::ModelTimeout => "cell_window_timeout_count",
                CellWindowSource::Setting => "cell_window_setting_count",
                CellWindowSource::ColdDefault => "cell_window_default_count",
            },
            1,
        );
    }

    /// One spill-store event of this session (counts and bytes: never a handle or text).
    pub(crate) fn note_spill(&self, fact: crate::kernel::spill::SpillFact) {
        use crate::kernel::spill::SpillFact;
        match fact {
            SpillFact::Queued { bytes } => {
                self.note_capability("spill_count", 1);
                // The raw bytes stay in the session: `agent context paging` reports their bucket.
                self.note_capability(capability_counts::SPILL_BYTES, bytes);
            }
            SpillFact::NotStored => self.note_capability("spill_not_stored_count", 1),
            SpillFact::Evicted => self.note_capability("spill_evicted_count", 1),
        }
    }

    /// One kernel `ctx.*` read (a count: never a handle or a pattern).
    pub(crate) fn note_ctx_read(&self, read: crate::kernel::spill::host::CtxRead) {
        use crate::kernel::spill::host::CtxRead;
        self.note_capability(
            match read {
                CtxRead::Page => "ctx_page_count",
                CtxRead::Grep => "ctx_grep_count",
                CtxRead::Tail => "ctx_tail_count",
                CtxRead::Info => "ctx_info_count",
            },
            1,
        );
    }

    /// One kernel `worktree` operation (counts only: never a name, a path, or a diff).
    pub(crate) fn note_worktree(&self, fact: crate::worktree_host::WorktreeFact) {
        use crate::worktree_host::WorktreeFact;
        self.note_capability(
            match fact {
                WorktreeFact::Created => "worktree_created_count",
                WorktreeFact::Diffed => "worktree_diff_count",
                WorktreeFact::Merged => "worktree_merged_count",
                WorktreeFact::Branched => "worktree_branched_count",
                WorktreeFact::Conflicted => "worktree_conflict_count",
                WorktreeFact::Removed => "worktree_removed_count",
                WorktreeFact::Failed => "worktree_failed_count",
            },
            1,
        );
    }

    /// One kernel `usage.get()` read (a count: never a cost or a token number).
    pub(crate) fn note_usage_read(&self) {
        self.note_capability("usage_read_count", 1);
    }

    /// One recorded assistant call's billing mode (a count: never its cost or provider).
    pub(crate) fn note_billing_mode(&self, mode: pa_types::billing::BillingMode) {
        self.with(|values| values.billing_modes.add(Some(&mode), 0.0));
    }

    /// One settled `agent_message.wait`, by outcome name (counts only: never a sender or body).
    pub(crate) fn note_agent_message_wait(&self, outcome: &str) {
        self.with(|values| {
            let waits = &mut values.agent_message_waits;
            match outcome {
                "message" => waits.message += 1,
                "timeout" => waits.timeout += 1,
                "cycle" => waits.cycle += 1,
                "no_sender" => waits.no_sender += 1,
                // The wait handler renders only the four outcomes above.
                _ => {}
            }
        });
    }

    /// `agent_message.send` deliveries the rate bucket or a receiver's wake budget refused
    /// (counts: never a target or body).
    pub(crate) fn note_agent_message_refusals(
        &self,
        refusals: super::agent_messaging::AgentMessageRefusals,
    ) {
        self.with(|values| {
            values.agent_message_refusals.throttled += refusals.throttled;
            values.agent_message_refusals.wake_refused += refusals.wake_refused;
        });
    }

    /// One kernel `rlm.abort` (counts only): its outcome, `None` when it failed.
    pub(crate) fn note_rlm_abort(&self, outcome: Option<super::rlm_host::RlmAbortOutcome>) {
        use super::rlm_host::RlmAbortOutcome;
        self.note_capability(
            match outcome {
                Some(RlmAbortOutcome::Aborted) => "rlm_abort_aborted_count",
                Some(RlmAbortOutcome::Idle) => "rlm_abort_idle_count",
                None => "rlm_abort_failed_count",
            },
            1,
        );
    }

    /// One observation of the adaptive delegation depth bound (counts only).
    pub(crate) fn note_rlm_depth(&self, change: pa_types::control::DepthChange) {
        use pa_types::control::DepthChange;
        match change {
            DepthChange::Raised => self.note_capability("rlm_depth_raised_count", 1),
            DepthChange::Lowered => self.note_capability("rlm_depth_lowered_count", 1),
            DepthChange::Unchanged => {}
        }
    }

    /// One brief-mode `better` run whose depth followed the remaining delegation bound.
    pub(crate) fn note_scaled_brief(&self) {
        self.note_capability("rlm_depth_scaled_brief_count", 1);
    }

    /// One kernel `rlm.reload()` and the count of skills it added (never a name).
    pub(crate) fn note_repl_reload(&self, skills_added: usize) {
        self.with(|values| {
            values.capabilities.add("repl_reload_count", 1);
            values
                .capabilities
                .add("repl_reload_skills_added_count", skills_added as u64);
        });
    }

    /// One kernel `settings.get()` read (never a value).
    pub(crate) fn note_repl_settings_read(&self) {
        self.note_capability("repl_settings_get_count", 1);
    }

    /// One kernel `mcp.login`: `Some(true)` logged in, `Some(false)` failed, `None` unavailable.
    pub(crate) fn note_kernel_mcp_login(&self, outcome: Option<bool>) {
        self.note_capability(
            match outcome {
                Some(true) => "kernel_mcp_login_count",
                Some(false) => "kernel_mcp_login_failed_count",
                None => "kernel_mcp_login_unavailable_count",
            },
            1,
        );
    }

    /// One kernel past-session recall call (counts only: never a query or a session).
    pub(crate) fn note_session_recall(&self, fact: super::session_search::SessionRecallFact) {
        use super::session_search::SessionRecallFact;
        self.with(|values| {
            let counts = &mut values.capabilities;
            match fact {
                SessionRecallFact::Searched { hits } => {
                    counts.add("session_search_count", 1);
                    counts.add("session_search_hit_count", hits as u64);
                }
                SessionRecallFact::Read => counts.add("session_search_read_count", 1),
                SessionRecallFact::Failed => counts.add("session_search_failed_count", 1),
            }
        });
    }

    /// One kernel `notify(...)` ping and whether a user was attached to see it (never the text).
    pub(crate) fn note_notification(&self, delivered: bool) {
        self.with(|values| {
            values.capabilities.add("notify_count", 1);
            values
                .capabilities
                .add("notify_undelivered_count", u64::from(!delivered));
        });
    }

    /// One kernel `rlm.decide(...)` outcome (never the question, the options, or the choice).
    pub(crate) fn note_decision(&self, fact: super::decision::DecideFact) {
        use super::decision::DecideFact;
        self.note_capability(
            match fact {
                DecideFact::Decided => "decide_count",
                DecideFact::Limited => "decide_limited_count",
                DecideFact::Failed => "decide_failed_count",
            },
            1,
        );
    }

    /// One kernel `model.set_effort(...)`: applied, or refused (the setting is off or the
    /// level invalid). Never the level.
    pub(crate) fn note_effort_switch(&self, applied: bool) {
        self.note_capability(
            if applied {
                "effort_set_count"
            } else {
                "effort_refused_count"
            },
            1,
        );
    }

    /// An MCP connector call (the server name never uploads).
    pub fn note_mcp_connector_use(&self) {
        self.with(|values| values.mcp_connector_use_count += 1);
    }

    /// One kernel boot: cold or revived, its outcome, its duration.
    pub fn note_kernel_bootstrap(&self, cold: bool, succeeded: bool, duration_ms: u64) {
        self.with(|values| {
            values.kernel_bootstrap_count += 1;
            values.kernel_bootstrap_cold_count += u64::from(cold);
            values.kernel_bootstrap_failed_count += u64::from(!succeeded);
            values.kernel_bootstrap_max_ms = values.kernel_bootstrap_max_ms.max(duration_ms);
        });
    }

    /// One native web search: answered or failed, whether a fallback
    /// answered, whether a keyless engine did, the result count, and the
    /// latency. The query and the provider never upload.
    pub fn note_web_search(
        &self,
        outcome: &Result<crate::web_search::SearchReport, crate::web_search::SearchError>,
    ) {
        self.with(|values| {
            values.web_search_count += 1;
            let elapsed = match outcome {
                Ok(report) => {
                    values.web_search_fallback_count += u64::from(!report.failures.is_empty());
                    values.web_search_keyless_count += u64::from(matches!(
                        report.provider.access(),
                        crate::web_search::ProviderAccess::Keyless
                    ));
                    values.web_search_result_count += report.response.sources.len() as u64;
                    report.elapsed
                }
                Err(error) => {
                    values.web_search_failed_count += 1;
                    error.elapsed
                }
            };
            values.web_search_max_ms = values
                .web_search_max_ms
                .max(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        });
    }

    /// One cache-warming refresh went out and answered, costing `cost_usd`.
    pub(crate) fn note_cache_warm_sent(&self, cost_usd: f64) {
        self.with(|values| {
            values.cache_warm_sent_count += 1;
            if cost_usd.is_finite() && cost_usd > 0.0 {
                values.cache_warm_cost_usd += cost_usd;
            }
        });
    }

    /// An armed cache warmer stopped without sending (the saving no longer
    /// cleared the threshold, a cap was reached, or its refresh failed).
    pub(crate) fn note_cache_warm_skipped(&self) {
        self.with(|values| values.cache_warm_skipped_count += 1);
    }

    /// One subdirectory context file joined the conversation (a count: never its path).
    pub(crate) fn note_nested_context_file(&self) {
        self.with(|values| values.nested_context_file_count += 1);
    }

    /// One real request's prompt-cache saving in dollars (negative when its
    /// cache writes cost more than its reads saved).
    pub(crate) fn note_cache_savings(&self, savings_usd: f64) {
        if savings_usd.is_finite() {
            self.with(|values| values.cache_savings_usd += savings_usd);
        }
    }

    pub(super) fn write_into(&self, properties: &mut Properties) {
        self.with(|values| {
            for (key, value) in [
                ("skill_use_count", values.skill_use_count),
                ("mcp_connector_use_count", values.mcp_connector_use_count),
                ("kernel_bootstrap_count", values.kernel_bootstrap_count),
                (
                    "kernel_bootstrap_cold_count",
                    values.kernel_bootstrap_cold_count,
                ),
                (
                    "kernel_bootstrap_failed_count",
                    values.kernel_bootstrap_failed_count,
                ),
                ("kernel_bootstrap_max_ms", values.kernel_bootstrap_max_ms),
                ("rlm_child_usage_count", values.rlm_child_usage_count),
                ("rlm_child_input_tokens", values.rlm_child_input_tokens),
                ("rlm_child_output_tokens", values.rlm_child_output_tokens),
                (
                    "rlm_child_cache_read_tokens",
                    values.rlm_child_cache_read_tokens,
                ),
                (
                    "rlm_child_cache_write_tokens",
                    values.rlm_child_cache_write_tokens,
                ),
            ] {
                properties.set(key, Value::from(value));
            }
            if values.rlm_child_usage_count > 0 {
                properties.set("rlm_child_cost", Value::from(values.rlm_child_cost));
            }
            // Sessions that never searched keep their ended event unchanged.
            if values.web_search_count > 0 {
                for (key, value) in [
                    ("web_search_count", values.web_search_count),
                    ("web_search_failed_count", values.web_search_failed_count),
                    (
                        "web_search_fallback_count",
                        values.web_search_fallback_count,
                    ),
                    ("web_search_keyless_count", values.web_search_keyless_count),
                    ("web_search_result_count", values.web_search_result_count),
                    ("web_search_max_ms", values.web_search_max_ms),
                ] {
                    properties.set(key, Value::from(value));
                }
            }
            // Sessions that never thinned keep their ended event unchanged.
            if values.context_thinned_request_count > 0 {
                for (key, value) in [
                    (
                        "context_thinned_request_count",
                        values.context_thinned_request_count,
                    ),
                    (
                        "context_thinned_result_count",
                        values.context_thinned_result_count,
                    ),
                    (
                        "context_thinned_chars_saved",
                        values.context_thinned_chars_saved,
                    ),
                    (
                        "context_thinned_tokens_saved",
                        values.context_thinned_tokens_saved,
                    ),
                ] {
                    properties.set(key, Value::from(value));
                }
            }
            // Sessions that never cached or warmed keep their ended event unchanged.
            if values.cache_warm_sent_count + values.cache_warm_skipped_count > 0
                || values.cache_savings_usd != 0.0
            {
                properties.set(
                    "cache_warm_sent_count",
                    Value::from(values.cache_warm_sent_count),
                );
                properties.set(
                    "cache_warm_skipped_count",
                    Value::from(values.cache_warm_skipped_count),
                );
                properties.set(
                    "cache_warm_cost_usd",
                    Value::from(values.cache_warm_cost_usd),
                );
                // The cost kind is non-negative: a session whose writes outweighed
                // its reads reports no saving.
                properties.set(
                    "cache_savings_usd",
                    Value::from(values.cache_savings_usd.max(0.0)),
                );
            }
            // Sessions that loaded no nested context file keep their ended event unchanged.
            if values.nested_context_file_count > 0 {
                properties.set(
                    "nested_context_file_count",
                    Value::from(values.nested_context_file_count),
                );
            }
            for (key, count) in &values.feature_outcomes {
                properties.set(key, Value::from(*count));
            }
        });
    }
}

/// Skills present at session start (adoption counts on `agent started`).
pub struct SkillCounts {
    pub skill_count: usize,
    pub scheme_skill_count: usize,
}

/// Install the telemetry subscriber on an agent and emit `agent started`.
///
/// # Errors
///
/// The current implementation never returns `Err`.
///
/// # Panics
///
/// Panics if the telemetry state mutex is poisoned.
pub async fn install_session_telemetry(
    agent: &Arc<pa_agent::agent::Agent>,
    wiring: &TelemetryWiring,
    skill_counts: Option<SkillCounts>,
    counters: Arc<SessionCounters>,
) -> anyhow::Result<SessionTelemetry> {
    let execution_mode = wiring
        .execution_mode
        .clone()
        .unwrap_or_else(|| EXECUTION_MODE_UNKNOWN.to_string());
    let now = wiring.now.clone().unwrap_or_else(|| Arc::new(now_millis));
    let client = wiring.client.clone();
    let state = Arc::new(Mutex::new(TelemetryState {
        session_id: uuid(),
        started_at: now(),
        totals: SessionTotals::default(),
        active_run: None,
        tool_starts: HashMap::new(),
        telemetry_enabled: wiring.telemetry_enabled.clone(),
        recording: true,
        now,
        auth_baseline: crate::auth::resolver::AuthMetrics::current(),
        meter: MetricsMeter::new(SpawnVisibility::Observed),
    }));
    // The counters arrive already gated: the engine installs the same
    // live switch on them at creation (the MCP and kernel seams count
    // long before this install runs), so nothing counts while telemetry
    // is off in any window.

    let subscriber_state = Arc::clone(&state);
    let subscriber_client = client.clone();
    let subscriber_mode = execution_mode.clone();
    let subscription = agent
        .subscribe(move |event, _signal| {
            let state = Arc::clone(&subscriber_state);
            let client = subscriber_client.clone();
            let execution_mode = subscriber_mode.clone();
            Box::pin(async move {
                handle_event(&client, &execution_mode, &state, event);
                Ok(())
            })
        })
        .await;

    let mut properties = base_properties(&execution_mode);
    {
        let state = state.lock().expect("telemetry state poisoned");
        properties.set("session_id", Value::from(state.session_id.as_str()));
        if let Some(counts) = skill_counts {
            properties.set("skill_count", Value::from(counts.skill_count as u64));
            properties.set(
                "scheme_skill_count",
                Value::from(counts.scheme_skill_count as u64),
            );
        }
    }
    client.track("agent started", properties);
    Ok(SessionTelemetry {
        client,
        state,
        execution_mode,
        counters,
        ended: std::sync::atomic::AtomicBool::new(false),
        agent_session_id: agent.session_id().map(str::to_string),
        _subscription: Some(subscription),
    })
}

impl SessionTelemetry {
    /// A compaction completed (feed from the compaction seams; TS
    /// `compaction_end` handling). Counts toward the active run when one
    /// exists, exactly like the TS subscriber — compactions outside a run
    /// never inflate session totals. The duration (measured centrally by
    /// the compaction executor) sums into the run's
    /// `compaction_duration_ms`.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_compaction(&self, duration_ms: Option<u64>) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            sever_off_period_run(&mut state);
            return;
        }
        if let Some(run) = state.active_run.as_mut() {
            run.compaction_count += 1;
            run.compaction_duration_ms += duration_ms.unwrap_or(0);
        }
        if state.active_run.is_some() {
            state.meter.compaction();
        }
    }

    /// The kernel admitted a subagent spawn (`rlm-spawn`); counts toward
    /// the active run's `subagent_spawn_count`.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_subagent_spawn(&self) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            sever_off_period_run(&mut state);
            return;
        }
        if state.active_run.is_some() {
            state.meter.subagent_spawn();
        }
    }

    /// A model call outside the agent loop billed the session (a row
    /// [`billed_row`](crate::session::billed_row) classifies: a compaction,
    /// a branch summary, a refinement or side-question call). Like
    /// [`Self::note_compaction`], its cost counts toward the active run.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_billed_call(&self, usage: &pa_types::ai::Usage) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            sever_off_period_run(&mut state);
            return;
        }
        if let Some(run) = state.active_run.as_mut() {
            run.cost_usd += usage.cost.total.as_f64();
        }
    }

    /// One auto-retry event from the retry seam (TS `auto_retry_start`):
    /// `Start` counts the retry into the active run, sums the retry wait,
    /// and keeps the run open, so the retried attempt continues the same run (one
    /// `agent run completed` per turn, like TS). `End` closes the retry:
    /// a retry that never ran (a cancelled wait) leaves the run to
    /// finalize at the next start instead of absorbing the next turn. The
    /// final attempt's message decides the outcome.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_auto_retry_event(&self, event: &AutoRetryEvent) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            // While off nothing counts, and the run severs: an off-period
            // retry never continues an on-period run across the opt-out.
            sever_off_period_run(&mut state);
            return;
        }
        let Some(run) = state.active_run.as_mut() else {
            return;
        };
        match event {
            AutoRetryEvent::Start { delay_ms, .. } => {
                run.retry_count += 1;
                if run
                    .last_assistant
                    .as_ref()
                    .and_then(provider_stream_failure_kind)
                    .as_deref()
                    == Some(REPETITION_LOOP_KIND)
                {
                    run.loop_guard.retries += 1;
                }
                run.retry_wait_ms += *delay_ms;
                run.retry_pending = true;
            }
            AutoRetryEvent::End { .. } => run.retry_pending = false,
        }
    }

    /// A tool result carried the loop guard's repeated-call nudge
    /// (`session_engine::loop_guard`); counts toward the active run.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_tool_repeat_nudge(&self) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            sever_off_period_run(&mut state);
            return;
        }
        if let Some(run) = state.active_run.as_mut() {
            run.loop_guard.tool_repeat_nudges += 1;
        }
    }

    /// A loop request broke the previous request's prompt-cache prefix,
    /// leaving `broken_bytes` of it unreused (`session_engine::cache_prefix`);
    /// counts toward the active run.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_cache_prefix_break(&self, broken_bytes: u64) {
        let mut state = self.state.lock().expect("telemetry state poisoned");
        if !state.recording {
            sever_off_period_run(&mut state);
            return;
        }
        if let Some(run) = state.active_run.as_mut() {
            run.cache_prefix_breaks += 1;
            run.cache_prefix_break_bytes += broken_bytes;
        }
    }

    /// A feature attempt's observed result at a session-engine seam,
    /// counted as `feature_<name>_<outcome>_count` on `agent session
    /// ended`. The configuration choice is not reported.
    pub fn note_feature_outcome(
        &self,
        feature_name: &'static str,
        outcome: &'static str,
        _configuration_choice: Option<&'static str>,
    ) {
        if let Some(key) = pa_telemetry::feature_outcome_key(feature_name, outcome) {
            self.counters
                .with(|values| *values.feature_outcomes.entry(key).or_default() += 1);
        }
    }

    /// Finalize any active run, emit `agent session ended`, and flush.
    /// The host calls this at session close (TUI exit, worker shutdown,
    /// kill); the `ended` flag makes a second close path a no-op, matching
    /// the TS single `registerDisposeCallback` firing. A run still open
    /// across an opt-out severs before the finalize, so an off window's
    /// run never reports after a re-enable.
    ///
    /// # Errors
    ///
    /// Returns the telemetry client's flush error, if any.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub async fn end(&self) -> anyhow::Result<()> {
        if self.ended.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut state = self.state.lock().expect("telemetry state poisoned");
            // Session close is the last recording seam: a run still open
            // when the cached decision says off severs here instead of
            // finalizing; the client's flush drops whatever the
            // mid-turn opt-out already queued.
            if !state.recording {
                sever_off_period_run(&mut state);
            }
            finalize_run(&self.client, &self.execution_mode, &mut state);
        }
        let mut properties = self.session_properties();
        {
            let state = self.state.lock().expect("telemetry state poisoned");
            let totals = &state.totals;
            properties.set(
                "duration_ms",
                Value::from((state.now)().saturating_sub(state.started_at)),
            );
            properties.set("prompt_count", Value::from(totals.prompt_count));
            properties.set("run_count", Value::from(totals.run_count));
            properties.set(
                "successful_run_count",
                Value::from(totals.successful_run_count),
            );
            properties.set("failed_run_count", Value::from(totals.failed_run_count));
            properties.set("aborted_run_count", Value::from(totals.aborted_run_count));
            properties.set("tool_call_count", Value::from(totals.tool_call_count));
            properties.set("compaction_count", Value::from(totals.compaction_count));
            properties.set(
                "model_call_count",
                Value::from(totals.usage.model_call_count),
            );
            properties.set("input_tokens", Value::from(totals.usage.input));
            properties.set("output_tokens", Value::from(totals.usage.output));
            properties.set("cache_read_tokens", Value::from(totals.usage.cache_read));
            properties.set("cache_write_tokens", Value::from(totals.usage.cache_write));
            properties.set("total_tokens", Value::from(totals.usage.total_tokens));
            // v2 (#2117): `end()` is the normal dispose path; a crash never
            // reaches it (the archive path emits `session archived` first).
            properties.set("terminal_outcome", Value::from("success"));
            properties.set("retry_count", Value::from(totals.retry_count));
            properties.set("model_error_count", Value::from(totals.model_error_count));
            // Sessions whose providers declare no auth descriptor keep their
            // ended event unchanged.
            let auth = crate::auth::resolver::AuthMetrics::current().since(state.auth_baseline);
            if auth.static_descriptor_resolves + auth.token_exchange_resolves + auth.oauth_resolves
                > 0
            {
                let strategy = if auth.token_exchange_resolves > 0 {
                    "token_exchange"
                } else if auth.oauth_resolves > 0 {
                    "oauth"
                } else {
                    "api_key"
                };
                properties.set("auth_strategy", Value::from(strategy));
                properties.set("auth_token_exchange_count", Value::from(auth.exchanges));
                properties.set(
                    "auth_refresh_failed_count",
                    Value::from(auth.exchange_failures + auth.oauth_refresh_failures),
                );
                properties.set(
                    "auth_rejected_retry_count",
                    Value::from(auth.rejected_retries),
                );
            }
        }
        // The capability events that ride ahead of the ended event (the todo/question adoption:
        // the ended event sits at the key cap); a session that used none sends none.
        self.track_capability_events(pa_telemetry::CAPABILITY_EVENTS_BEFORE_SESSION_END);
        self.counters.write_into(&mut properties);
        self.client.track("agent session ended", properties);
        // A session whose goals never ran a verifier or hit a budget sends no goal event.
        let mut goal = GoalVerificationCounts::default();
        self.counters.with(|values| goal = values.goal);
        if goal != GoalVerificationCounts::default() {
            let mut goal_properties = self.session_properties();
            for (key, value) in [
                ("goal_check_run_count", goal.check_runs),
                ("goal_check_failed_count", goal.check_failures),
                ("goal_check_skipped_count", goal.check_skips),
                ("goal_judge_call_count", goal.judge_calls),
                ("goal_judge_not_met_count", goal.judge_not_met),
                ("goal_token_budget_stop_count", goal.token_budget_stops),
                ("goal_cost_budget_stop_count", goal.cost_budget_stops),
            ] {
                goal_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent goal verification", goal_properties);
        }
        // A session whose requests never chose a Responses transport sends no transport event.
        if let Some(counts) = self
            .agent_session_id
            .as_deref()
            .and_then(pa_ai::responses_transport::session_counts)
        {
            self.counters.note_responses_transport(counts);
        }
        let mut transport = pa_ai::responses_transport::ResponsesTransportCounts::default();
        self.counters
            .with(|values| transport = values.responses_transport);
        if transport != pa_ai::responses_transport::ResponsesTransportCounts::default() {
            let mut transport_properties = self.session_properties();
            for (key, value) in [
                (
                    "responses_websocket_request_count",
                    transport.websocket_requests,
                ),
                ("responses_delta_request_count", transport.delta_requests),
                (
                    "responses_full_context_request_count",
                    transport.full_context_requests,
                ),
                ("responses_sse_fallback_count", transport.sse_fallbacks),
                (
                    "responses_stale_continuation_retry_count",
                    transport.stale_continuation_retries,
                ),
                ("responses_auto_websocket_count", transport.auto_websocket),
                (
                    "responses_auto_sse_one_shot_count",
                    transport.auto_sse_one_shot,
                ),
                (
                    "responses_auto_sse_no_websocket_wire_count",
                    transport.auto_sse_no_websocket_wire,
                ),
                (
                    "responses_auto_sse_unsupported_count",
                    transport.auto_sse_unsupported,
                ),
                (
                    "responses_auto_sse_demoted_count",
                    transport.auto_sse_demoted,
                ),
                ("responses_probe_count", transport.probes),
                ("responses_probe_failed_count", transport.probe_failures),
                (
                    "responses_websocket_without_wire_count",
                    transport.websocket_requested_without_wire,
                ),
            ] {
                transport_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent responses transport", transport_properties);
        }
        // A process whose provider requests never waited and never met pushback sends no
        // admission event. Fire-and-forget like every event here: the counts are read, never
        // awaited.
        let admission = pa_ai::admission::take_stats();
        if admission != pa_ai::admission::AdmissionStats::default() {
            let mut admission_properties = self.session_properties();
            for (key, value) in [
                ("admission_waited_count", admission.waited_count),
                ("admission_wait_ms", admission.wait_ms),
                ("provider_throttled_count", admission.throttled_count),
                ("admission_decrease_count", admission.decrease_count),
            ] {
                admission_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent provider admission", admission_properties);
        }
        // A process whose scraped search engines were never skipped or blocked sends no engine
        // event (counts only: never a query).
        let engines = crate::web_search::engine_limits::take_stats();
        if engines != crate::web_search::engine_limits::EngineLimitStats::default() {
            let mut engine_properties = self.session_properties();
            for (key, value) in [
                ("web_search_engine_skipped_count", engines.skipped_count),
                ("web_search_engine_blocked_count", engines.blocked_count),
            ] {
                engine_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent web search engines", engine_properties);
        }
        // A process whose session boots never waited and never met host pressure sends no
        // boot admission event (counts only).
        let boots = crate::kernel::boot_admission::take_stats();
        if boots != crate::kernel::boot_admission::BootAdmissionStats::default() {
            let mut boot_properties = self.session_properties();
            for (key, value) in [
                ("boot_admission_waited_count", boots.waited_count),
                ("boot_admission_wait_ms", boots.wait_ms),
                ("boot_pressure_count", boots.pressure_count),
            ] {
                boot_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent session boot admission", boot_properties);
        }
        // A process whose turns never re-opened an idle provider connection sends no re-warm
        // event (counts only: never an origin).
        let rewarm = pa_ai::utils::prewarm::take_rewarm_stats();
        if rewarm != pa_ai::utils::prewarm::RewarmStats::default() {
            let mut rewarm_properties = self.session_properties();
            for (key, value) in [
                ("connection_rewarm_count", rewarm.rewarm_count),
                ("connection_rewarm_reopened_count", rewarm.reopened_count),
                ("connection_rewarm_reopened_ms", rewarm.reopened_ms),
            ] {
                rewarm_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent connection rewarm", rewarm_properties);
        }
        // A session whose provider never armed a declared quota fallback sends no fallback
        // event (counts only: never a provider, model, or credential).
        let auth = crate::auth::resolver::AuthMetrics::current().since(
            self.state
                .lock()
                .expect("telemetry state poisoned")
                .auth_baseline,
        );
        if auth.quota_fallbacks_chosen + auth.quota_fallbacks_exhausted > 0 {
            let mut fallback_properties = self.session_properties();
            fallback_properties.set(
                "quota_fallback_chosen_count",
                Value::from(auth.quota_fallbacks_chosen),
            );
            fallback_properties.set(
                "quota_fallback_exhausted_count",
                Value::from(auth.quota_fallbacks_exhausted),
            );
            self.client
                .track("agent quota fallback", fallback_properties);
        }
        // A worker whose discovery never probed or moved an endpoint sends no discovery event.
        // Only a session installed on a live agent reads the worker's process-wide counts.
        let discovered = pa_ai::responses_transport::discovery_counts();
        if self.agent_session_id.is_some()
            && discovered != pa_ai::responses_transport::DiscoveryCounts::default()
        {
            self.counters.note_endpoint_discovery(discovered);
        }
        let mut discovery = pa_ai::responses_transport::DiscoveryCounts::default();
        self.counters
            .with(|values| discovery = values.endpoint_discovery);
        if discovery != pa_ai::responses_transport::DiscoveryCounts::default() {
            let mut discovery_properties = self.session_properties();
            for (key, value) in [
                ("endpoint_probe_count", discovery.probes),
                ("endpoint_found_responses_count", discovery.found_responses),
                ("endpoint_found_websocket_count", discovery.found_websocket),
                ("endpoint_upgrade_count", discovery.upgrades),
                ("endpoint_demotion_count", discovery.demotions),
            ] {
                discovery_properties.set(key, Value::from(value));
            }
            self.client
                .track("agent endpoint discovery", discovery_properties);
        }
        // A session that recorded no classified call sends no billing event.
        let mut billing = pa_types::billing::BillingSplit::default();
        self.counters.with(|values| billing = values.billing_modes);
        if billing != pa_types::billing::BillingSplit::default() {
            let mut billing_properties = self.session_properties();
            for (key, spend) in [
                ("billing_pay_as_you_go_count", billing.pay_as_you_go),
                ("billing_plan_count", billing.plan),
                ("billing_flat_rate_count", billing.flat_rate),
                ("billing_free_count", billing.free),
                ("billing_unknown_count", billing.unknown),
            ] {
                billing_properties.set(key, Value::from(spend.calls));
            }
            self.client.track("agent billing modes", billing_properties);
        }
        // A session whose sends were never refused sends no agent-message event.
        let mut refusals = super::agent_messaging::AgentMessageRefusals::default();
        self.counters
            .with(|values| refusals = values.agent_message_refusals);
        if refusals != super::agent_messaging::AgentMessageRefusals::default() {
            let mut message_properties = self.session_properties();
            message_properties.set(
                "agent_message_throttled_count",
                Value::from(refusals.throttled),
            );
            message_properties.set(
                "agent_message_wake_refused_count",
                Value::from(refusals.wake_refused),
            );
            self.client.track("agent messages", message_properties);
        }
        // A session that never waited for an agent message sends no wait event.
        let mut waits = AgentMessageWaitCounts::default();
        self.counters
            .with(|values| waits = values.agent_message_waits);
        if waits != AgentMessageWaitCounts::default() {
            let mut wait_properties = self.session_properties();
            for (key, count) in [
                ("agent_message_wait_message_count", waits.message),
                ("agent_message_wait_timeout_count", waits.timeout),
                ("agent_message_wait_cycle_count", waits.cycle),
                ("agent_message_wait_no_sender_count", waits.no_sender),
            ] {
                wait_properties.set(key, Value::from(count));
            }
            self.client.track("agent message waits", wait_properties);
        }
        self.track_error_enrichment();
        // Every kernel capability's standalone event (the pa-telemetry rule files), in emission
        // order; a capability the session never used sends none.
        self.track_capability_events(pa_telemetry::CAPABILITY_EVENTS_AFTER_SESSION_END);
        self.client.flush().await
    }

    /// Emit each of `rules` whose capability counted: every property in rule order.
    fn track_capability_events(&self, rules: &[&pa_telemetry::EventRule]) {
        let mut capabilities = CapabilityCounts::default();
        self.counters
            .with(|values| capabilities = values.capabilities.clone());
        for rule in rules {
            if let Some(values) = capabilities.event_properties(rule) {
                let mut properties = self.session_properties();
                for (key, value) in values {
                    properties.set(key, value);
                }
                self.client.track(rule.name, properties);
            }
        }
    }

    /// `session archived` (schema v1): the session reached the archive
    /// state (daemon `kill`); emitted before `end()` on that path.
    ///
    /// # Panics
    ///
    /// Panics if the telemetry state mutex is poisoned.
    pub fn note_archived(&self) {
        let mut properties = self.session_properties();
        {
            let state = self.state.lock().expect("telemetry state poisoned");
            properties.set(
                "duration_ms",
                Value::from((state.now)().saturating_sub(state.started_at)),
            );
        }
        self.client.track("session archived", properties);
    }

    /// A `/skill:<name>` submission expanded into its skill block (the
    /// `AgentSession::prompt_with_images` expansion seam), counted as
    /// `skill_use_count` on `agent session ended`; the skill name never
    /// uploads.
    pub fn note_skill_used(&self) {
        self.counters.with(|values| values.skill_use_count += 1);
    }

    /// One durable child-usage attribution row landed in the parent
    /// session (the RLM producer's flush), summed into the `rlm_child_*`
    /// counters on `agent session ended`.
    pub fn note_child_usage_attributed(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        cost: f64,
    ) {
        self.counters.with(|values| {
            values.rlm_child_usage_count += 1;
            values.rlm_child_input_tokens += input_tokens;
            values.rlm_child_output_tokens += output_tokens;
            values.rlm_child_cache_read_tokens += cache_read_tokens;
            values.rlm_child_cache_write_tokens += cache_write_tokens;
            if cost.is_finite() && cost > 0.0 {
                values.rlm_child_cost += cost;
            }
        });
    }

    fn session_properties(&self) -> Properties {
        let mut properties = base_properties(&self.execution_mode);
        let state = self.state.lock().expect("telemetry state poisoned");
        properties.set("session_id", Value::from(state.session_id.as_str()));
        properties
    }
}

/// One agent event → state machine step; split out so tests can drive
/// scripted sequences without a live agent.
fn handle_event(
    client: &TelemetryClient,
    execution_mode: &str,
    state: &Arc<Mutex<TelemetryState>>,
    event: AgentEvent,
) {
    let mut state = state.lock().expect("telemetry state poisoned");
    // The switch is asked at the turn boundaries only: a run or turn
    // start refreshes the cached decision, every other event consults
    // the cache, and the client's flush drops everything queued while
    // the switch is off. While recording is off nothing records, and the
    // run severs: its facts never enter the aggregates, so a later
    // enable can neither complete it nor merge the next run into it.
    if matches!(event, AgentEvent::AgentStart | AgentEvent::TurnStart) {
        observe_turn_boundary(&mut state);
        if !state.recording {
            return;
        }
    } else if !state.recording {
        sever_off_period_run(&mut state);
        return;
    }
    let now = (state.now)();
    match event {
        AgentEvent::AgentStart => {
            // A retried attempt continues its turn's run (TS keeps one run
            // across `auto_retry_start`: `activeRun ??= ...`).
            if let Some(run) = state.active_run.as_mut().filter(|run| run.retry_pending) {
                run.retry_pending = false;
                run.ended = false;
                run.ended_at = None;
                // The failed call is behind a retry now: the run's cost
                // stays reported (with the failed attempts' usage
                // included) unless the final attempt fails too.
                run.usage_complete = true;
                return;
            }
            // The previous run finalizes here (not at AgentEnd): a post-run
            // compaction drained in between must land in that run.
            finalize_run_locked(client, execution_mode, &mut state);
            let run_index = state.totals.run_count + 1;
            state.active_run = Some(ActiveRun {
                started_at: now,
                ended: false,
                ended_at: None,
                first_turn_started_at: None,
                first_model_event_ms: None,
                visible_ttft_ms: None,
                current_turn_started_at: None,
                model_latency_ms: 0,
                max_model_latency_ms: 0,
                turn_count: 0,
                tool_call_count: 0,
                tool_error_count: 0,
                compaction_count: 0,
                retry_count: 0,
                usage: UsageTotals::default(),
                last_assistant: None,
                retry_pending: false,
                run_id: uuid(),
                run_index,
                trigger_pending: true,
                trigger: RunTrigger::Unknown,
                first_reasoning_ms: None,
                run_to_first_text_ms: None,
                tool_duration_ms: 0,
                retry_wait_ms: 0,
                max_stream_gap_ms: None,
                last_stream_event_at: None,
                successful_model_call_count: 0,
                usage_complete: true,
                cost_usd: 0.0,
                model_latencies: Vec::new(),
                model_error_count: 0,
                error_category_counts: std::collections::BTreeMap::new(),
                compaction_duration_ms: 0,
                tool_summary: HashMap::new(),
                loop_guard: LoopGuardCounts::default(),
                cache_prefix_breaks: 0,
                cache_prefix_break_bytes: 0,
            });
        }
        AgentEvent::MessageStart { message } => {
            if message.role() == "user" {
                state.totals.prompt_count += 1;
                // A prompt-run's user message lands right after
                // `AgentStart`: the run's trigger is a fresh prompt.
                resolve_trigger(&mut state, RunTrigger::Prompt);
            }
        }
        AgentEvent::TurnStart => {
            if let Some(run) = state.active_run.as_mut() {
                if run.first_turn_started_at.is_none() {
                    run.first_turn_started_at = Some(now);
                }
                run.current_turn_started_at = Some(now);
                run.turn_count += 1;
            }
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            if state.active_run.is_some() {
                // A continuation-run's first event is a model event (no
                // user message ever lands inside it): the retry/goal
                // re-entry disambiguates the trigger.
                resolve_trigger(&mut state, RunTrigger::Continuation);
                let Some(run) = state.active_run.as_mut() else {
                    return;
                };
                if run.first_model_event_ms.is_none() {
                    if let Some(first_turn) = run.first_turn_started_at {
                        run.first_model_event_ms = Some(now.saturating_sub(first_turn));
                    }
                }
                if run.visible_ttft_ms.is_none() {
                    if let Some(first_turn) = run.first_turn_started_at {
                        let is_text_delta = matches!(
                            assistant_message_event.as_ref(),
                            pa_agent::stream::AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty()
                        );
                        if is_text_delta {
                            run.visible_ttft_ms = Some(now.saturating_sub(first_turn));
                        }
                    }
                }
                if run.run_to_first_text_ms.is_none() {
                    let is_text_delta = matches!(
                        assistant_message_event.as_ref(),
                        pa_agent::stream::AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty()
                    );
                    if is_text_delta {
                        run.run_to_first_text_ms = Some(now.saturating_sub(run.started_at));
                    }
                }
                if run.first_reasoning_ms.is_none() {
                    let is_reasoning_delta = matches!(
                        assistant_message_event.as_ref(),
                        pa_agent::stream::AssistantMessageEvent::ThinkingDelta { delta, .. } if !delta.is_empty()
                    );
                    if is_reasoning_delta {
                        if let Some(first_turn) = run.first_turn_started_at {
                            run.first_reasoning_ms = Some(now.saturating_sub(first_turn));
                        }
                    }
                }
                if let Some(last) = run.last_stream_event_at {
                    let gap = now.saturating_sub(last);
                    run.max_stream_gap_ms =
                        Some(run.max_stream_gap_ms.map_or(gap, |max| max.max(gap)));
                }
                run.last_stream_event_at = Some(now);
            }
        }
        AgentEvent::MessageEnd { message } => {
            if let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) = message
            {
                // A continuation-run without stream events (a non-streamed
                // response) still disambiguates at its first assistant
                // message end.
                resolve_trigger(&mut state, RunTrigger::Continuation);
                let is_error = assistant.stop_reason == StopReason::Error;
                let repetition_loop = provider_stream_failure_kind(&assistant).as_deref()
                    == Some(REPETITION_LOOP_KIND);
                let category = error_category(Some(&assistant))
                    .as_str()
                    .and_then(|category| {
                        pa_telemetry::ERROR_CATEGORIES
                            .iter()
                            .find(|known| **known == category)
                            .copied()
                    });
                let cost_total = assistant.usage.cost.total;
                if state.active_run.is_some() {
                    state.meter.request(&assistant);
                }
                if let Some(run) = state.active_run.as_mut() {
                    run.usage.add(&assistant.usage);
                    run.last_assistant = Some(assistant);
                    if let Some(turn_started) = run.current_turn_started_at.take() {
                        let latency = now.saturating_sub(turn_started);
                        run.model_latency_ms += latency;
                        run.max_model_latency_ms = run.max_model_latency_ms.max(latency);
                        run.model_latencies.push(latency);
                    }
                    if is_error {
                        run.usage_complete = false;
                        run.model_error_count += 1;
                        if repetition_loop {
                            run.loop_guard.trips += 1;
                        }
                        if let Some(category) = category {
                            *run.error_category_counts.entry(category).or_default() += 1;
                        }
                    } else {
                        run.successful_model_call_count += 1;
                    }
                    run.cost_usd += cost_total;
                }
            }
        }
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
            state.tool_starts.insert(tool_call_id, now);
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            let started_at = state.tool_starts.remove(&tool_call_id);
            let duration_ms = started_at.map_or(0, |start| now.saturating_sub(start));
            let category = ToolCategory::from_tool_name(&tool_name);
            if state.active_run.is_some() {
                state
                    .meter
                    .tool_result(&tool_name, is_error, &result.details);
            }
            if let Some(run) = state.active_run.as_mut() {
                run.tool_call_count += 1;
                if is_error {
                    run.tool_error_count += 1;
                }
                run.tool_duration_ms += duration_ms;
                let tool_stats = run.tool_summary.entry(category).or_default();
                tool_stats.calls += 1;
                tool_stats.failures += u64::from(is_error);
                tool_stats.duration_ms += duration_ms;
                tool_stats.max_duration_ms = tool_stats.max_duration_ms.max(duration_ms);
            }
        }
        AgentEvent::AgentEnd { .. } => {
            if let Some(run) = state.active_run.as_mut() {
                run.ended = true;
                run.ended_at = Some(now);
            }
        }
        // TurnEnd carries no facts the TS subscriber used (turn_count comes
        // from TurnStart); ToolExecutionUpdate is mid-execution progress.
        // Both are still run-scoped events: while the cached decision is
        // off they sever (a tool whose execution spans an opt-out observed
        // at a boundary never counts into a surviving run).
        AgentEvent::TurnEnd { .. }
        | AgentEvent::ToolExecutionUpdate { .. }
        | AgentEvent::ToolExecutionProgress { .. } => {}
    }
}

/// Record the run's trigger the first time it disambiguates.
fn resolve_trigger(state: &mut TelemetryState, trigger: RunTrigger) {
    if let Some(run) = state.active_run.as_mut().filter(|run| run.trigger_pending) {
        run.trigger_pending = false;
        run.trigger = trigger;
    }
}

/// Finalize the active run and emit `agent run completed`.
fn finalize_run(client: &TelemetryClient, execution_mode: &str, state: &mut TelemetryState) {
    finalize_run_locked(client, execution_mode, state);
}

fn finalize_run_locked(client: &TelemetryClient, execution_mode: &str, state: &mut TelemetryState) {
    let Some(mut run) = state.active_run.take() else {
        return;
    };
    let now = (state.now)();
    let run_end = run.ended_at.unwrap_or(now);
    let metrics = state
        .meter
        .finish_run(run_end.saturating_sub(run.started_at));
    let outcome = run_outcome(run.last_assistant.as_ref());
    state.totals.run_count += 1;
    state.totals.tool_call_count += run.tool_call_count;
    state.totals.compaction_count += run.compaction_count;
    match outcome {
        "success" => state.totals.successful_run_count += 1,
        "aborted" => state.totals.aborted_run_count += 1,
        _ => state.totals.failed_run_count += 1,
    }
    state.totals.usage.merge(&run.usage);

    state.totals.retry_count += run.retry_count;
    state.totals.model_error_count += run.model_error_count;

    let mut properties = base_properties(execution_mode);
    properties.set("session_id", Value::from(state.session_id.as_str()));
    properties.set("outcome", Value::from(outcome));
    properties.set(
        "duration_ms",
        Value::from(run_end.saturating_sub(run.started_at)),
    );
    properties.set("visible_ttft_ms", opt_value(run.visible_ttft_ms));
    properties.set("first_model_event_ms", opt_value(run.first_model_event_ms));
    properties.set("model_latency_ms", Value::from(run.model_latency_ms));
    properties.set(
        "max_model_latency_ms",
        Value::from(run.max_model_latency_ms),
    );
    properties.set("model_call_count", Value::from(run.usage.model_call_count));
    properties.set("turn_count", Value::from(run.turn_count));
    properties.set("tool_call_count", Value::from(run.tool_call_count));
    properties.set("tool_error_count", Value::from(run.tool_error_count));
    properties.set("input_tokens", Value::from(run.usage.input));
    properties.set("output_tokens", Value::from(run.usage.output));
    properties.set("cache_read_tokens", Value::from(run.usage.cache_read));
    properties.set("cache_write_tokens", Value::from(run.usage.cache_write));
    properties.set("total_tokens", Value::from(run.usage.total_tokens));
    properties.set("compaction_count", Value::from(run.compaction_count));
    properties.set("retry_count", Value::from(run.retry_count));
    properties.set("loop_guard_trip_count", Value::from(run.loop_guard.trips));
    properties.set(
        "loop_guard_retry_count",
        Value::from(run.loop_guard.retries),
    );
    properties.set(
        "loop_guard_tool_repeat_nudge_count",
        Value::from(run.loop_guard.tool_repeat_nudges),
    );
    properties.set(
        "cache_prefix_break_count",
        Value::from(run.cache_prefix_breaks),
    );
    properties.set(
        "cache_prefix_break_bytes",
        Value::from(run.cache_prefix_break_bytes),
    );
    properties.set(
        "provider_category",
        Value::from(provider_category(
            run.last_assistant.as_ref().map(|m| m.provider.as_str()),
        )),
    );
    properties.set(
        "model_category",
        Value::from(
            run.last_assistant
                .as_ref()
                .map_or("unknown", |m| model_category(&m.model)),
        ),
    );
    properties.set(
        "error_category",
        error_category(run.last_assistant.as_ref()),
    );
    properties.set("run_id", Value::from(run.run_id.as_str()));
    properties.set("run_index", Value::from(run.run_index));
    properties.set("trigger", Value::from(run.trigger.as_str()));
    properties.set(
        "stop_reason",
        Value::from(stop_reason(run.last_assistant.as_ref())),
    );
    properties.set("terminal_outcome", Value::from(terminal_outcome(outcome)));
    properties.set(
        "successful_model_call_count",
        Value::from(run.successful_model_call_count),
    );
    if run.usage.model_call_count > 0 {
        properties.set("usage_complete", Value::from(run.usage_complete));
    }
    if run.usage_complete && run.usage.model_call_count > 0 && run.cost_usd > 0.0 {
        // Estimated cost requires known pricing and complete usage; otherwise
        // null (the conservative direction).
        properties.set("estimated_cost_usd", Value::from(run.cost_usd));
    }
    if run.last_assistant.as_ref().map(|m| m.stop_reason) == Some(StopReason::Error) {
        properties.set(
            "error_subtype",
            Value::from(
                classify_error_message(
                    run.last_assistant
                        .as_ref()
                        .and_then(|m| m.error_message.as_deref())
                        .unwrap_or_default(),
                )
                .subtype,
            ),
        );
    }
    properties.set("first_reasoning_ms", opt_value(run.first_reasoning_ms));
    properties.set("run_to_first_text_ms", opt_value(run.run_to_first_text_ms));
    properties.set("tool_duration_ms", Value::from(run.tool_duration_ms));
    properties.set("retry_wait_ms", Value::from(run.retry_wait_ms));
    properties.set("max_stream_gap_ms", opt_value(run.max_stream_gap_ms));
    properties.set(
        "compaction_duration_ms",
        Value::from(run.compaction_duration_ms),
    );
    properties.set(
        "model_latency_p50_ms",
        opt_value(median(&mut run.model_latencies)),
    );
    properties.set("model_error_count", Value::from(run.model_error_count));
    if metrics.bash_waits_completed + metrics.bash_waits_yielded > 0 {
        properties.set(
            "bash_wait_completed_count",
            Value::from(metrics.bash_waits_completed),
        );
        properties.set(
            "bash_wait_yielded_count",
            Value::from(metrics.bash_waits_yielded),
        );
    }
    // The harness metrics (`super::run_metrics`): input_tokens above is the
    // uncached split already.
    properties.set("end_reason", Value::from(metrics.end_reason.as_str()));
    properties.set("eval_cell_count", Value::from(metrics.eval_cells));
    properties.set("eval_cell_duration_ms", Value::from(metrics.eval_cell_ms));
    if let Some(spawns) = metrics.subagent_spawns {
        properties.set("subagent_spawn_count", Value::from(spawns));
    }
    if let Some(breaks) = metrics.cache_breaks {
        properties.set("cache_read_break_count", Value::from(breaks));
    }
    if let Some(rate) = metrics.cache_hit_rate {
        properties.set("cache_hit_percent", Value::from(rate * 100.0));
    }
    let percents = metrics
        .compactions
        .iter()
        .filter_map(|point| point.context_fraction.map(|fraction| fraction * 100.0));
    if let Some(min) = percents.clone().reduce(f64::min) {
        properties.set("compaction_context_percent_min", Value::from(min));
    }
    if let Some(max) = percents.reduce(f64::max) {
        properties.set("compaction_context_percent_max", Value::from(max));
    }
    for (category, count) in &run.error_category_counts {
        properties.set(&format!("error_{category}_count"), Value::from(*count));
    }
    // Per built-in tool by name; every MCP and custom tool folds into the
    // `mcp_tool_*` / `custom_tool_*` aggregates (no raw tool names).
    for (category, stats) in &run.tool_summary {
        let prefix = match category {
            ToolCategory::Mcp => "mcp_tool".to_string(),
            ToolCategory::Custom | ToolCategory::Unknown => "custom_tool".to_string(),
            builtin => format!("tool_{}", builtin.as_str()),
        };
        for (suffix, value) in [
            ("call_count", stats.calls),
            ("error_count", stats.failures),
            ("duration_ms", stats.duration_ms),
            ("max_duration_ms", stats.max_duration_ms),
        ] {
            let key = format!("{prefix}_{suffix}");
            let total = properties.get(&key).and_then(Value::as_u64).unwrap_or(0);
            let value = if suffix == "max_duration_ms" {
                total.max(value)
            } else {
                total + value
            };
            properties.set(&key, Value::from(value));
        }
    }
    client.track("agent run completed", properties);
}

/// The median of the run's model-call latencies (the lower middle for an
/// even count); `None` without calls.
fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

/// The #2117 `terminal_outcome` vocabulary: the legacy run outcome
/// (`success`/`error`/`aborted`) onto the terminal vocabulary (the legacy
/// `aborted` is the terminal `cancelled`).
fn terminal_outcome(run_outcome: &str) -> &'static str {
    match run_outcome {
        "success" => "success",
        "error" => "error",
        "aborted" => "cancelled",
        _ => "unknown",
    }
}

/// The #2117 `stop_reason` vocabulary for the final assistant message.
fn stop_reason(last_assistant: Option<&AssistantMessage>) -> &'static str {
    match last_assistant.map(|message| message.stop_reason) {
        Some(StopReason::Stop) => "stop",
        Some(StopReason::Length) => "length",
        Some(StopReason::ToolUse) => "toolUse",
        Some(StopReason::Error) => "error",
        Some(StopReason::Aborted) => "aborted",
        None => "unknown",
    }
}

/// Build the product telemetry client from settings: the Prime Intellect
/// analytics sink (the TS endpoint and wire format; none in debug builds,
/// see [`telemetry_endpoint`]) plus the local JSONL transparency mirror
/// (default on, `telemetry.localMirror` disables it), behind the live
/// [`telemetry_switch`] re-read before every delivery pass. Never fails: a
/// broken install id falls back to a no-op client (TS parity: capture
/// disables itself when the installation identity cannot be created).
pub fn build_client(
    settings: &crate::settings::SettingsManager,
    agent_dir: &std::path::Path,
) -> TelemetryClient {
    let mut config = TelemetryClientConfig::new("disabled");
    let install_id = pa_telemetry::install_id(agent_dir);
    match install_id {
        Ok(id) => {
            config.install_id = id;
            let mut sinks: Vec<Arc<dyn pa_telemetry::TelemetrySink>> = Vec::new();
            if let Some(endpoint) = telemetry_endpoint() {
                sinks.push(Arc::new(pa_telemetry::AnalyticsSink::with_http_builder(
                    endpoint,
                    pa_ai::utils::tls::http_client_builder(),
                )));
            }
            let local_mirror = settings
                .settings()
                .telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.local_mirror)
                .unwrap_or(true);
            if local_mirror {
                sinks.push(Arc::new(pa_telemetry::FileSink::new(agent_dir)));
            }
            config.sinks = sinks;
            // `/telemetry off` (or a settings edit) applies to running
            // clients at their next delivery pass, no restart needed.
            let settings = settings.reopen();
            config.enabled = Some(Arc::new(move || {
                telemetry_switch(&settings.reopen()).enabled()
            }));
        }
        Err(error) => {
            tracing::warn!(error = %error, "telemetry install id unavailable; telemetry disabled");
            config.sinks = vec![Arc::new(pa_telemetry::NoopSink)];
        }
    }
    TelemetryClient::spawn(config).unwrap_or_else(|error| {
        // No runtime on this thread: an inert client whose tracks are
        // counted as dropped. Telemetry must never fail the session.
        tracing::warn!(error = %error, "telemetry worker unavailable; events will drop");
        TelemetryClient::inert()
    })
}

/// The recording seams' live opt-out switch: the same env-then-settings
/// resolution the delivery pass applies ([`telemetry_switch`]), resolved
/// live at the turn boundaries: an off (or an on) applies to the
/// recording seams at the next boundary, exactly like the client's
/// delivery gate applies it at the flush.
#[must_use]
pub fn telemetry_enabled_switch(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
) -> RecordingSwitch {
    let settings = crate::settings::SettingsManager::create(cwd, agent_dir);
    RecordingSwitch {
        enabled: Arc::new(move || telemetry_switch(&settings.reopen()).enabled()),
    }
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
