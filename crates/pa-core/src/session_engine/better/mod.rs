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
//! The reply contract lives in [`reply`], the wording in [`instructions`].

mod instructions;
mod read_only;
mod reply;
#[cfg(test)]
mod tests;

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

/// Reads the helper may make in one pass: enough to open the files a prompt names and follow
/// a few references, small enough that a one-line prompt costs seconds, not minutes.
const READ_BUDGET: u32 = 12;

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
}

impl BetterHost {
    #[must_use]
    pub fn new(
        cwd: PathBuf,
        counters: Option<Arc<SessionCounters>>,
        spill: Option<crate::kernel::spill::SpillStore>,
    ) -> Self {
        Self {
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

    /// Count one outcome the composer reported for an inserted rewrite.
    pub fn note_composer_outcome(&self, outcome: pa_types::better::ComposerOutcome) {
        self.note(match outcome {
            pa_types::better::ComposerOutcome::SentAsIs => BetterTelemetryFact::SentAsIs,
            pa_types::better::ComposerOutcome::Edited => BetterTelemetryFact::Edited,
            pa_types::better::ComposerOutcome::Discarded => BetterTelemetryFact::Discarded,
        });
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
        let outcome = self.run_counted(request, signal, progress).await;
        self.note(match &outcome {
            BetterOutcome::Rewritten { .. } => BetterTelemetryFact::Completed,
            BetterOutcome::Cancelled => BetterTelemetryFact::Cancelled,
            BetterOutcome::Failed { .. } => BetterTelemetryFact::Failed,
        });
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
        let run = run_helper(
            &agent,
            stream_fn,
            request,
            &cwd,
            &DEFAULT_PROVIDER_RETRY_POLICY,
            signal,
            progress,
        )
        .await;
        if let (Some(usage), Some(recorder)) = (run.usage, self.usage.get()) {
            recorder(usage).await;
        }
        match run.end {
            HelperEnd::Cancelled => BetterOutcome::Cancelled,
            HelperEnd::Failed(error) => failed(&error),
            HelperEnd::Replied(text) => match reply::parse_reply(&text) {
                Ok(parsed) => {
                    let task = request.task.trim();
                    match parsed.rewrite {
                        Some(rewrite) if rewrite != task => BetterOutcome::Rewritten {
                            rewrite,
                            unchanged: false,
                        },
                        _ => BetterOutcome::Rewritten {
                            rewrite: task.to_string(),
                            unchanged: true,
                        },
                    }
                }
                Err(error) => BetterOutcome::Failed {
                    error: error.to_string(),
                    reply: Some(self.keep(text)),
                },
            },
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

/// How the helper's loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelperEnd {
    /// Its final text (not yet parsed).
    Replied(String),
    Cancelled,
    Failed(String),
}

/// One helper run's end and its summed bill.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HelperRun {
    pub(crate) end: HelperEnd,
    pub(crate) usage: Option<ModelUsage>,
}

/// Run the helper loop against `parent`'s model, prompt, and tools (see the module docs).
pub(crate) async fn run_helper(
    parent: &Arc<Agent>,
    stream_fn: pa_agent::stream::StreamFn,
    request: &BetterRequest,
    cwd: &Path,
    retry_policy: &ProviderRetryPolicy,
    signal: &AbortSignal,
    progress: &BetterProgress,
) -> HelperRun {
    let parent_state = parent.state().await;
    if parent_state.model == pa_agent::types::Model::unknown() {
        return HelperRun {
            end: HelperEnd::Failed("select a model before using /better".to_string()),
            usage: None,
        };
    }
    let budget = Arc::new(ReadBudget::new(READ_BUDGET));
    let context = ReadContext {
        cwd: cwd.to_path_buf(),
        budget: Arc::clone(&budget),
        progress: Arc::clone(progress),
    };
    let messages = match request.mode {
        BetterMode::Deepen => parent_state.messages.clone(),
        BetterMode::Brief => Vec::new(),
    };
    let prefix_len = messages.len();
    let turns = Arc::new(AtomicU32::new(0));
    let should_stop_after_turn: ShouldStopAfterTurnFn = {
        let turns = Arc::clone(&turns);
        let budget = Arc::clone(&budget);
        Arc::new(move |context| {
            let turns = Arc::clone(&turns);
            let budget = Arc::clone(&budget);
            Box::pin(async move {
                let count = turns.fetch_add(1, Ordering::SeqCst) + 1;
                let calls_tools = context
                    .message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)));
                Ok(!calls_tools || count >= budget.limit() + GRACE_TURNS)
            })
        })
    };
    let helper = Arc::new(Agent::new(AgentOptions {
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
    let observer = SideRunObserver::subscribe(&helper).await;
    // The run's own abort: the caller's cancel, or the wall clock.
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
    // Partial text is not progress here: the user sees what the helper reads, not its draft.
    let sink: SideQuestionSink = Arc::new(|_| true);
    let prompt = instructions::first_pass(request.mode, &request.task, cwd, budget.limit());
    let result = run_attempts(
        &helper,
        &prompt,
        prefix_len,
        retry_policy,
        &run_abort.signal(),
        &sink,
        &observer.updates,
    )
    .await;
    watchdog.abort();
    let billed = observer.finish().await;
    let end = match result.status {
        SideQuestionStatus::Complete => HelperEnd::Replied(result.answer),
        SideQuestionStatus::Cancelled if timed_out.load(Ordering::SeqCst) => HelperEnd::Failed(
            format!("the helper timed out after {} s", WALL_CLOCK.as_secs()),
        ),
        SideQuestionStatus::Cancelled => HelperEnd::Cancelled,
        SideQuestionStatus::Error => HelperEnd::Failed(
            result
                .error_message
                .unwrap_or_else(|| "the helper failed".to_string()),
        ),
    };
    HelperRun {
        end,
        usage: ModelUsage::new(
            USAGE_SOURCE,
            &parent_state.model.provider,
            &parent_state.model.id,
            billed,
        ),
    }
}
