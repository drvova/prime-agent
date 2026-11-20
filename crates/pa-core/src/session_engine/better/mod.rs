//! `/better`: rewrite a prompt into the deeper version a top professional would ask for,
//! grounded in what the helper reads from the user's own repository.
//!
//! The helper is a side run on the side-question machinery (`side_question.rs`): a fresh agent
//! loop on the session's own model, thinking level, credential, and pre-semantic stream fn
//! (so its calls carry no request id and pass the same provider admission as every session
//! call), with the session's system prompt and tool definitions byte for byte so its requests
//! reuse the session's cached prefix. Deepen mode clones the live conversation the way `/btw`
//! does (the prompt may lean on it); brief mode starts from no messages (a delegation brief is
//! checked against the repository, not the conversation). Its tools only read (see
//! [`read_only`]); nothing it does enters the session's history.
//!
//! A run adapts the way a serverless worker does, with nothing to configure:
//!
//! - Its read budget starts where this project's learning puts it ([`learning`]: rewrites that
//!   get discarded start deeper) and grows only while the helper still lists references it has
//!   not located, by [`GROWTH_STEP`] reads per pass, up to [`READ_CEILING`] and inside the run's
//!   [`WALL_CLOCK`].
//! - Its result is checked mechanically ([`verify`]): every path, symbol, and command the rewrite
//!   cites must resolve in the repository. Unverified ones go back to the helper for one refine
//!   pass; what still does not resolve is named under the rewrite.
//!
//! The reply contract lives in [`reply`], the wording in [`instructions`].

mod instructions;
mod learning;
mod read_only;
mod reply;
#[cfg(test)]
mod tests;
mod verify;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use pa_agent::abort::{AbortController, AbortSignal};
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::agent_loop::ShouldStopAfterTurnFn;
use pa_agent::types::AssistantContent;

use super::model_usage::ModelUsage;
use super::provider_retry::{ProviderRetryPolicy, DEFAULT_PROVIDER_RETRY_POLICY};
use super::side_question::{run_attempts, SideQuestionSink, SideQuestionStatus, SideRunObserver};
use super::telemetry::{BetterTelemetryFact, SessionCounters};
pub use read_only::ReadProgress as BetterProgress;
use read_only::{ReadBudget, ReadContext};

/// The most reads one run may make, every pass together: enough to chase a prompt's references
/// through a large repository, small enough that a run stays a quick step before sending.
const READ_CEILING: u32 = 24;

/// Reads each grounding pass (and the refine pass) adds.
const GROWTH_STEP: u32 = 4;

/// A grounding pass starts only with at least this much of the wall clock left (a pass reads
/// and then answers).
const PASS_RESERVE: Duration = Duration::from_secs(30);

/// Model turns past the read budget: the turn that meets the spent budget, and one to answer.
const GRACE_TURNS: u32 = 2;

/// The longest one run may take, reads and model calls together. A rewrite that takes longer
/// than this has stopped being a quick step before sending.
const WALL_CLOCK: Duration = Duration::from_secs(180);

/// The usage-row source the helper's model calls bill under.
const USAGE_SOURCE: &str = "better_prompt";

/// What the helper writes: a deeper prompt for the user, or a checked delegation brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BetterMode {
    /// `/better <prompt>`: the deeper, repo-grounded version of a prompt.
    Deepen,
    /// `better(task, mode="brief")`: a delegation brief checked against the repository; returned
    /// unchanged when nothing needs fixing.
    Brief,
}

impl BetterMode {
    /// The wire name (`deepen`, `brief`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "deepen" => Some(Self::Deepen),
            "brief" => Some(Self::Brief),
            _ => None,
        }
    }
}

/// One run's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BetterRequest {
    pub task: String,
    pub mode: BetterMode,
    /// Where the helper reads (absolute, or relative to the session cwd); the session cwd when
    /// `None`.
    pub cwd: Option<PathBuf>,
}

/// The full reply of a run whose reply did not parse, kept for the user to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptReply {
    pub text: String,
    /// Where the session's spill store keeps it (`out:<id>` and its file), when it has one.
    pub stored: Option<(String, PathBuf)>,
}

/// How one run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BetterOutcome {
    /// The rewrite (the input itself when `unchanged`).
    Rewritten { rewrite: String, unchanged: bool },
    /// Cancelled by the caller; nothing was produced.
    Cancelled,
    /// The model or provider failed, the run timed out, or the reply had no rewrite (`reply`
    /// keeps the whole reply then).
    Failed {
        error: String,
        reply: Option<KeptReply>,
    },
}

impl BetterOutcome {
    /// The user-facing error line, naming where a kept reply lives.
    #[must_use]
    pub fn error_text(&self) -> Option<String> {
        let BetterOutcome::Failed { error, reply } = self else {
            return None;
        };
        Some(match reply {
            None => error.clone(),
            Some(KeptReply {
                stored: Some((handle, path)),
                ..
            }) => format!(
                "{error}; the full reply is kept as {handle} at {}",
                path.display()
            ),
            Some(KeptReply { text, stored: None }) => {
                format!("{error}; the full reply was:\n{text}")
            }
        })
    }
}

/// Records one billed helper run into the session's usage rows.
pub type UsageRecorder = Arc<dyn Fn(ModelUsage) -> pa_agent::BoxFut<'static, ()> + Send + Sync>;

/// One session's `/better` runner: what the TUI command, the session command, and the kernel's
/// `better(...)` all call. The session's agent and stream fn bind once the engine assembles.
pub struct BetterHost {
    cwd: PathBuf,
    agent: OnceLock<Weak<Agent>>,
    stream_fn: OnceLock<pa_agent::stream::StreamFn>,
    usage: OnceLock<UsageRecorder>,
    counters: Option<Arc<SessionCounters>>,
    spill: Option<crate::kernel::spill::SpillStore>,
    learning: learning::ProjectLearning,
}

impl BetterHost {
    /// `agent_dir` keeps this project's learning across sessions (`None`: in memory only).
    #[must_use]
    pub fn new(
        cwd: PathBuf,
        agent_dir: Option<&Path>,
        counters: Option<Arc<SessionCounters>>,
        spill: Option<crate::kernel::spill::SpillStore>,
    ) -> Self {
        Self {
            learning: learning::ProjectLearning::new(agent_dir, &cwd),
            cwd,
            agent: OnceLock::new(),
            stream_fn: OnceLock::new(),
            usage: OnceLock::new(),
            counters,
            spill,
        }
    }

    /// Bind the assembled session (first bind wins). The agent is held weakly: the kernel's
    /// handlers, which hold this host, live on that agent's session.
    pub fn bind(
        &self,
        agent: &Arc<Agent>,
        stream_fn: pa_agent::stream::StreamFn,
        usage: UsageRecorder,
    ) {
        let _ = self.agent.set(Arc::downgrade(agent));
        let _ = self.stream_fn.set(stream_fn);
        let _ = self.usage.set(usage);
    }

    fn note(&self, fact: BetterTelemetryFact) {
        if let Some(counters) = &self.counters {
            counters.note_better(fact);
        }
    }

    /// Count one outcome the composer reported for an inserted rewrite, and learn from it.
    pub fn note_composer_outcome(&self, outcome: pa_types::better::ComposerOutcome) {
        let (fact, signal) = match outcome {
            pa_types::better::ComposerOutcome::SentAsIs => {
                (BetterTelemetryFact::SentAsIs, learning::Signal::SentAsIs)
            }
            pa_types::better::ComposerOutcome::Edited => {
                (BetterTelemetryFact::Edited, learning::Signal::Edited)
            }
            pa_types::better::ComposerOutcome::Discarded => {
                (BetterTelemetryFact::Discarded, learning::Signal::Discarded)
            }
        };
        self.note(fact);
        self.learning.observe(signal);
    }

    /// Run the helper once. `progress` hears a short label per read; `signal` cancels.
    #[tracing::instrument(skip_all, fields(mode = ?request.mode))]
    pub async fn run(
        &self,
        request: &BetterRequest,
        signal: &AbortSignal,
        progress: &BetterProgress,
    ) -> BetterOutcome {
        self.note(BetterTelemetryFact::Run);
        let brief = request.mode == BetterMode::Brief;
        if brief {
            self.note(BetterTelemetryFact::BriefRun);
        }
        let outcome = self.run_counted(request, signal, progress).await;
        self.note(match &outcome {
            BetterOutcome::Rewritten { .. } => BetterTelemetryFact::Completed,
            BetterOutcome::Cancelled => BetterTelemetryFact::Cancelled,
            BetterOutcome::Failed { .. } => BetterTelemetryFact::Failed,
        });
        if brief
            && matches!(
                outcome,
                BetterOutcome::Rewritten {
                    unchanged: true,
                    ..
                }
            )
        {
            self.note(BetterTelemetryFact::BriefUnchanged);
        }
        outcome
    }

    async fn run_counted(
        &self,
        request: &BetterRequest,
        signal: &AbortSignal,
        progress: &BetterProgress,
    ) -> BetterOutcome {
        let failed = |error: &str| BetterOutcome::Failed {
            error: error.to_string(),
            reply: None,
        };
        if request.task.trim().is_empty() {
            return failed("there is no prompt to improve");
        }
        let cwd = match &request.cwd {
            Some(cwd) => self.cwd.join(cwd),
            None => self.cwd.clone(),
        };
        if !cwd.is_dir() {
            return failed(&format!("cwd {} is not a directory", cwd.display()));
        }
        let (Some(agent), Some(stream_fn)) = (
            self.agent.get().and_then(Weak::upgrade),
            self.stream_fn.get().cloned(),
        ) else {
            return failed("the session is still starting; try again");
        };
        let first_pass_reads = self.learning.estimate().await.first_pass_budget();
        let run = run_helper(
            &agent,
            stream_fn,
            &HelperInput {
                request,
                cwd: &cwd,
                first_pass_reads,
            },
            &DEFAULT_PROVIDER_RETRY_POLICY,
            signal,
            progress,
        )
        .await;
        if let (Some(usage), Some(recorder)) = (run.usage, self.usage.get()) {
            recorder(usage).await;
        }
        for _ in 0..run.verification_passes {
            self.note(BetterTelemetryFact::VerificationPass);
        }
        match run.end {
            HelperEnd::Cancelled => BetterOutcome::Cancelled,
            HelperEnd::Failed(error) => failed(&error),
            HelperEnd::Unparsed { error, text } => BetterOutcome::Failed {
                error: error.to_string(),
                reply: Some(self.keep(text)),
            },
            HelperEnd::Replied {
                rewrite,
                unverified,
            } => {
                let task = request.task.trim();
                let (rewrite, unchanged) = match rewrite {
                    Some(rewrite) if rewrite != task => (rewrite, false),
                    _ => (task.to_string(), true),
                };
                if unverified.is_empty() {
                    BetterOutcome::Rewritten { rewrite, unchanged }
                } else {
                    // What still does not resolve is named under the rewrite, so whoever sends it
                    // sees it first.
                    let cited = unverified
                        .iter()
                        .map(|reference| format!("`{reference}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    BetterOutcome::Rewritten {
                        rewrite: format!("{rewrite}\n\n{UNVERIFIED_LABEL} {cited}"),
                        unchanged: false,
                    }
                }
            }
        }
    }

    /// Keep an unparsed reply in the session's spill store (its write runs on the store's
    /// writer thread, never here).
    fn keep(&self, text: String) -> KeptReply {
        let stored = self.spill.as_ref().and_then(|store| {
            let handle = store.spill(crate::kernel::spill::SpillCopy::of_text(&text));
            matches!(handle.outcome, crate::kernel::spill::SpillOutcome::Queued).then(|| {
                (
                    format!("{}{}", crate::kernel::spill::HANDLE_PREFIX, handle.id),
                    store.stored_path(&handle.id),
                )
            })
        });
        KeptReply { text, stored }
    }
}

/// The kernel's `better(task, *, cwd=None, mode=...)` request (`better.run`): one run on the
/// session's helper, its rewrite as `{rewrite, unchanged}`, or its raw error. A cell that stops
/// waiting (interrupted, or its kernel gone) cancels the run.
pub(crate) const BETTER_REQUEST_TYPE: &str = "better.run";

/// Register [`BETTER_REQUEST_TYPE`] on `host`.
pub(crate) fn register_better_handler(
    handlers: &mut crate::kernel::shared::HostRequestHandlers,
    host: &Arc<BetterHost>,
) {
    let host = Arc::clone(host);
    handlers.register(
        BETTER_REQUEST_TYPE,
        crate::kernel::shared::host_handler(move |payload| {
            let host = Arc::clone(&host);
            async move {
                let data = &payload.data;
                let Some(task) = data.get("task").and_then(serde_json::Value::as_str) else {
                    anyhow::bail!("better task must be a string");
                };
                let mode = match data.get("mode").and_then(serde_json::Value::as_str) {
                    None => BetterMode::Deepen,
                    Some(name) => BetterMode::parse(name).ok_or_else(|| {
                        anyhow::anyhow!("better mode must be \"deepen\" or \"brief\", got {name:?}")
                    })?,
                };
                let cwd = match data.get("cwd") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(cwd)) => Some(PathBuf::from(cwd)),
                    Some(_) => anyhow::bail!("better cwd must be a string"),
                };
                let request = BetterRequest {
                    task: task.to_string(),
                    mode,
                    cwd,
                };
                // Dropping this future (the cell stopped waiting) aborts the run.
                let controller = AbortController::new();
                let _cancel_on_drop = CancelOnDrop(controller.clone());
                let progress: BetterProgress = Arc::new(|_| {});
                match host.run(&request, &controller.signal(), &progress).await {
                    BetterOutcome::Rewritten { rewrite, unchanged } => {
                        Ok(serde_json::json!({ "rewrite": rewrite, "unchanged": unchanged }))
                    }
                    BetterOutcome::Cancelled => anyhow::bail!("the better run was cancelled"),
                    failed @ BetterOutcome::Failed { .. } => {
                        anyhow::bail!(failed.error_text().unwrap_or_default())
                    }
                }
            }
        }),
    );
}

/// Aborts its controller when dropped.
struct CancelOnDrop(AbortController);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The line under a rewrite that names the references verification could not resolve.
pub(crate) const UNVERIFIED_LABEL: &str = "Unverified (not found in the repository):";

/// How the helper's loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelperEnd {
    /// The final parsed reply: its rewrite (`None`: unchanged) and the cited references that
    /// still do not resolve.
    Replied {
        rewrite: Option<String>,
        unverified: Vec<String>,
    },
    /// A reply without the contract's blocks, kept whole.
    Unparsed {
        error: &'static str,
        text: String,
    },
    Cancelled,
    Failed(String),
}

/// One helper run's end, its summed bill, and the verification passes it took.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HelperRun {
    pub(crate) end: HelperEnd,
    pub(crate) usage: Option<ModelUsage>,
    pub(crate) verification_passes: u32,
}

/// What one run works from.
pub(crate) struct HelperInput<'a> {
    pub(crate) request: &'a BetterRequest,
    /// Where reads resolve.
    pub(crate) cwd: &'a Path,
    /// The first pass's reads (this project's learned start).
    pub(crate) first_pass_reads: u32,
}

/// How one pass ended short of a reply.
enum PassStop {
    Cancelled,
    Failed(String),
}

/// The helper agent and what its passes share.
struct Helper {
    agent: Arc<Agent>,
    observer: SideRunObserver,
    budget: Arc<ReadBudget>,
    /// The current pass's model turns and its granted reads (the turn cap is their sum plus
    /// [`GRACE_TURNS`]).
    pass_turns: Arc<AtomicU32>,
    pass_reads: Arc<AtomicU32>,
}

impl Helper {
    /// Run one pass: send `prompt` with `reads` more reads granted; the pass's final text.
    async fn pass(
        &self,
        prompt: &str,
        reads: u32,
        retry_policy: &ProviderRetryPolicy,
        signal: &AbortSignal,
        timed_out: &AtomicBool,
    ) -> Result<String, PassStop> {
        self.budget.grow(reads);
        self.pass_reads.store(reads, Ordering::SeqCst);
        self.pass_turns.store(0, Ordering::SeqCst);
        let prefix_len = self.agent.state().await.messages.len();
        // Partial text is not progress here: the user sees what the helper reads, not its draft.
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run_attempts(
            &self.agent,
            prompt,
            prefix_len,
            retry_policy,
            signal,
            &sink,
            &self.observer.updates,
        )
        .await;
        match result.status {
            SideQuestionStatus::Complete => Ok(result.answer),
            SideQuestionStatus::Cancelled if timed_out.load(Ordering::SeqCst) => {
                Err(PassStop::Failed(format!(
                    "the helper timed out after {} s",
                    WALL_CLOCK.as_secs()
                )))
            }
            SideQuestionStatus::Cancelled => Err(PassStop::Cancelled),
            SideQuestionStatus::Error => Err(PassStop::Failed(
                result
                    .error_message
                    .unwrap_or_else(|| "the helper failed".to_string()),
            )),
        }
    }
}

/// Run the helper against `parent`'s model, prompt, and tools (see the module docs): the first
/// pass, grounding passes while references stay unresolved, then verification with at most one
/// refine pass.
pub(crate) async fn run_helper(
    parent: &Arc<Agent>,
    stream_fn: pa_agent::stream::StreamFn,
    input: &HelperInput<'_>,
    retry_policy: &ProviderRetryPolicy,
    signal: &AbortSignal,
    progress: &BetterProgress,
) -> HelperRun {
    let parent_state = parent.state().await;
    if parent_state.model == pa_agent::types::Model::unknown() {
        return HelperRun {
            end: HelperEnd::Failed("select a model before using /better".to_string()),
            usage: None,
            verification_passes: 0,
        };
    }
    let request = input.request;
    let budget = Arc::new(ReadBudget::new(0));
    let context = ReadContext {
        cwd: input.cwd.to_path_buf(),
        budget: Arc::clone(&budget),
        progress: Arc::clone(progress),
    };
    let messages = match request.mode {
        BetterMode::Deepen => parent_state.messages.clone(),
        BetterMode::Brief => Vec::new(),
    };
    let pass_turns = Arc::new(AtomicU32::new(0));
    let pass_reads = Arc::new(AtomicU32::new(0));
    let should_stop_after_turn: ShouldStopAfterTurnFn = {
        let (pass_turns, pass_reads) = (Arc::clone(&pass_turns), Arc::clone(&pass_reads));
        Arc::new(move |context| {
            let (pass_turns, pass_reads) = (Arc::clone(&pass_turns), Arc::clone(&pass_reads));
            Box::pin(async move {
                let count = pass_turns.fetch_add(1, Ordering::SeqCst) + 1;
                let calls_tools = context
                    .message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)));
                Ok(!calls_tools || count >= pass_reads.load(Ordering::SeqCst) + GRACE_TURNS)
            })
        })
    };
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(parent_state.system_prompt.clone()),
            model: Some(parent_state.model.clone()),
            thinking_level: Some(parent_state.thinking_level),
            tools: Some(read_only::read_only_tools(&parent_state.tools, &context)),
            messages: Some(messages),
        },
        stream_fn: Some(stream_fn),
        should_stop_after_turn: Some(should_stop_after_turn),
        // Like the side question: the parent's identity reads the session's prompt cache, and
        // SSE never takes the session's socket or its continuation.
        session_id: parent.session_id().map(str::to_string),
        transport: Some(pa_agent::types::Transport::Sse),
        ..Default::default()
    }));
    let helper = Helper {
        observer: SideRunObserver::subscribe(&agent).await,
        agent,
        budget,
        pass_turns,
        pass_reads,
    };
    // The run's own abort: the caller's cancel, or the wall clock across every pass.
    let started = std::time::Instant::now();
    let run_abort = AbortController::new();
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (run_abort, timed_out, signal) =
            (run_abort.clone(), Arc::clone(&timed_out), signal.clone());
        tokio::spawn(async move {
            tokio::select! {
                () = signal.aborted() => {}
                () = tokio::time::sleep(WALL_CLOCK) => timed_out.store(true, Ordering::SeqCst),
            }
            run_abort.abort();
        })
    };
    let run_signal = run_abort.signal();
    let mut verification_passes = 0;
    let end = async {
        let pass = |prompt: String, reads: u32| {
            let (helper, run_signal, timed_out) = (&helper, &run_signal, &timed_out);
            async move {
                helper
                    .pass(&prompt, reads, retry_policy, run_signal, timed_out)
                    .await
            }
        };
        let stopped = |stop: PassStop| match stop {
            PassStop::Cancelled => HelperEnd::Cancelled,
            PassStop::Failed(error) => HelperEnd::Failed(error),
        };
        let parse = |text: String| {
            reply::parse_reply(&text).map_err(|error| HelperEnd::Unparsed { error, text })
        };
        let first = instructions::first_pass(
            request.mode,
            &request.task,
            input.cwd,
            input.first_pass_reads,
        );
        let mut parsed = match pass(first, input.first_pass_reads).await.map(parse) {
            Ok(Ok(parsed)) => parsed,
            Ok(Err(unparsed)) => return unparsed,
            Err(stop) => return stopped(stop),
        };
        // Grounding: more reads only while the helper still lists unresolved references.
        while !parsed.unresolved.is_empty()
            && helper.budget.limit() < READ_CEILING
            && started.elapsed() + PASS_RESERVE < WALL_CLOCK
        {
            let reads = GROWTH_STEP.min(READ_CEILING - helper.budget.limit());
            parsed = match pass(
                instructions::grounding_pass(&parsed.unresolved, reads),
                reads,
            )
            .await
            .map(parse)
            {
                Ok(Ok(parsed)) => parsed,
                Ok(Err(unparsed)) => return unparsed,
                Err(stop) => return stopped(stop),
            };
        }
        // Verification: every cited reference must resolve; one refine pass for those that do
        // not, then report what remains.
        let task = request.task.clone();
        let mut unverified =
            unresolved_references(parsed.rewrite.as_deref().unwrap_or(&task), input.cwd).await;
        if !unverified.is_empty() {
            verification_passes += 1;
            let reads = GROWTH_STEP.min(READ_CEILING.saturating_sub(helper.budget.limit()));
            parsed = match pass(instructions::refine_pass(&unverified, reads), reads)
                .await
                .map(parse)
            {
                Ok(Ok(parsed)) => parsed,
                Ok(Err(unparsed)) => return unparsed,
                Err(stop) => return stopped(stop),
            };
            unverified =
                unresolved_references(parsed.rewrite.as_deref().unwrap_or(&task), input.cwd).await;
        }
        HelperEnd::Replied {
            rewrite: parsed.rewrite,
            unverified,
        }
    }
    .await;
    watchdog.abort();
    let billed = helper.observer.finish().await;
    HelperRun {
        end,
        usage: ModelUsage::new(
            USAGE_SOURCE,
            &parent_state.model.provider,
            &parent_state.model.id,
            billed,
        ),
        verification_passes,
    }
}

/// The references `text` cites that do not resolve from `root`, in citation order.
async fn unresolved_references(text: &str, root: &Path) -> Vec<String> {
    let references = verify::cited_references(text);
    let cited: Vec<String> = references
        .iter()
        .map(|reference| reference.text().to_string())
        .collect();
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        references
            .into_iter()
            .filter(|reference| !verify::resolves(reference, &root))
            .map(|reference| reference.text().to_string())
            .collect()
    })
    .await
    // A check that could not finish verified nothing.
    .unwrap_or(cited)
}
