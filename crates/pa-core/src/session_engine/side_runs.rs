//! A session's side runs, `/btw` questions and `/better` rewrites, as both daemons serve them
//! (moved from pa-daemon's `side_question.rs` and `engine/wire.rs`): the run registry with the
//! `start_side_question`/`abort_side_question` admission rules and their exact error texts, the
//! `side_question_event` payloads a run streams, the run's event order ([`drive`]), the run on a
//! built engine ([`run_side_run`]) and `record_better_outcome`'s parse.
//!
//! A run answers outside the session's history: a question on a clone of the conversation
//! ([`super::side_question::run_side_question`]), a rewrite on the session's
//! [`super::better::BetterHost`]. Its events are one `running` event as it starts, one per
//! partial answer (a rewrite's are the helper's progress labels), and one terminal event
//! (`complete`, `cancelled` with the partial answer, or `error` with its `errorMessage`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pa_agent::abort::{AbortController, AbortSignal};
use serde_json::{json, Value};

use super::better::{BetterMode, BetterOutcome, BetterProgress, BetterRequest};
use super::side_question::{SideQuestionSink, SideQuestionStatus, SideQuestionTurn};

/// How long a closing session waits for its aborted runs' terminal events to go out.
pub const SIDE_RUN_SETTLE_TIMEOUT: Duration = Duration::from_secs(3);

/// One side-question request (the `start_side_question` command fields).
#[derive(Debug, Clone)]
pub struct SideQuestionRequest {
    /// Caller-generated id; echoed on every event of the run.
    pub side_question_id: String,
    pub question: String,
    /// A `/btw` question, or a `/better` rewrite of `question` (whose running events then carry
    /// progress labels, and whose final answer is the rewrite).
    pub kind: pa_types::better::SideRunKind,
    /// Earlier `{question, answer}` exchanges replayed before the question.
    pub previous_turns: Vec<SideQuestionTurn>,
}

/// How one side-question run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideQuestionOutcome {
    /// Answered; the string is the final answer text.
    Complete { answer: String },
    /// Aborted mid-run; the string is the partial answer streamed so far.
    Aborted { answer: String },
    /// Failed; the string is the provider/engine error message.
    Failed { answer: String, error: String },
}

/// Wire form of one side-question status.
pub const SIDE_QUESTION_STATUS_RUNNING: &str = "running";
pub const SIDE_QUESTION_STATUS_COMPLETE: &str = "complete";
pub const SIDE_QUESTION_STATUS_CANCELLED: &str = "cancelled";
pub const SIDE_QUESTION_STATUS_ERROR: &str = "error";

/// Wire form of one side-question event.
#[must_use]
pub fn side_question_event_value(
    request: &SideQuestionRequest,
    answer: &str,
    status: &str,
    error_message: Option<&str>,
) -> Value {
    let mut event = json!({
        "id": request.side_question_id,
        "question": request.question,
        "answer": answer,
        "status": status,
    });
    if let Some(error_message) = error_message {
        event["errorMessage"] = json!(error_message);
    }
    event
}

impl SideQuestionOutcome {
    /// The wire status of this outcome.
    #[must_use]
    pub fn status_str(&self) -> &'static str {
        match self {
            SideQuestionOutcome::Complete { .. } => SIDE_QUESTION_STATUS_COMPLETE,
            SideQuestionOutcome::Aborted { .. } => SIDE_QUESTION_STATUS_CANCELLED,
            SideQuestionOutcome::Failed { .. } => SIDE_QUESTION_STATUS_ERROR,
        }
    }

    /// The answer text carried by the final event (partial on abort/failure).
    #[must_use]
    pub fn answer(&self) -> &str {
        match self {
            SideQuestionOutcome::Complete { answer }
            | SideQuestionOutcome::Aborted { answer }
            | SideQuestionOutcome::Failed { answer, .. } => answer,
        }
    }

    /// The error message carried by the final event, when the run failed.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            SideQuestionOutcome::Failed { error, .. } => Some(error.as_str()),
            SideQuestionOutcome::Complete { .. } | SideQuestionOutcome::Aborted { .. } => None,
        }
    }
}

/// The client a side-run command speaks for (`clientId`; `anonymous` when it names none).
fn client_of(payload: &Value) -> String {
    payload
        .get("clientId")
        .and_then(Value::as_str)
        .unwrap_or("anonymous")
        .to_string()
}

/// The side-question id a side-run command names (empty when it names none).
fn side_question_id_of(payload: &Value) -> String {
    payload
        .get("sideQuestionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// One admitted run: what it asks, and the controller that aborts it.
pub struct AdmittedRun {
    pub request: SideQuestionRequest,
    pub abort: AbortController,
}

struct SideRun {
    client_id: String,
    abort: AbortController,
}

/// One session's live side runs: at most one per client, each under its own id, aborted by its
/// own client or by the session's close.
#[derive(Default)]
pub struct SideRuns {
    runs: Mutex<HashMap<String, SideRun>>,
    /// Set (under the registry lock) once a close path begins aborting: a run admitted after
    /// would wedge on a turn no terminal event will ever settle.
    closing: AtomicBool,
}

impl SideRuns {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, SideRun>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `start_side_question`'s admission for session `active_session_id`: the request
    /// `payload` carries, registered under its id for its client (`clientId`). The error is the
    /// old daemon's refusal text.
    ///
    /// # Errors
    ///
    /// The payload names no id or question, the session is closing, the id is taken, or the
    /// client already has a run.
    pub fn admit(&self, payload: &Value, active_session_id: &str) -> Result<AdmittedRun, String> {
        let side_question_id = side_question_id_of(payload);
        if side_question_id.is_empty() {
            return Err("Side question id is required".to_string());
        }
        let question = payload
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if question.is_empty() {
            return Err("Question cannot be empty".to_string());
        }
        let client_id = client_of(payload);
        let kind: pa_types::better::SideRunKind = payload
            .get("kind")
            .cloned()
            .and_then(|kind| serde_json::from_value(kind).ok())
            .unwrap_or_default();
        let previous_turns: Vec<SideQuestionTurn> = payload
            .get("previousTurns")
            .and_then(Value::as_array)
            .map_or_default(|turns| {
                turns
                    .iter()
                    .filter_map(|turn| serde_json::from_value(turn.clone()).ok())
                    .collect()
            });
        let abort = AbortController::new();
        let mut runs = self.lock();
        // The admission check rides the registry lock: `abort_all` sets `closing` under the
        // same lock, so a start is either admitted before the close's aborts or rejected once
        // the close owns the registry.
        if self.closing.load(Ordering::SeqCst) {
            return Err(format!("Active session {active_session_id} is closing"));
        }
        if runs.contains_key(&side_question_id) {
            return Err(format!("Side question already exists: {side_question_id}"));
        }
        if runs.values().any(|run| run.client_id == client_id) {
            return Err(
                "A side question is already running for this client and session".to_string(),
            );
        }
        runs.insert(
            side_question_id.clone(),
            SideRun {
                client_id,
                abort: abort.clone(),
            },
        );
        Ok(AdmittedRun {
            request: SideQuestionRequest {
                side_question_id,
                question,
                kind,
                previous_turns,
            },
            abort,
        })
    }

    /// `abort_side_question`: aborts the run `payload` names when its own client
    /// (`clientId`) asks; whether one was aborted (the reply's `aborted`).
    pub fn abort(&self, payload: &Value) -> bool {
        let side_question_id = side_question_id_of(payload);
        let client_id = client_of(payload);
        match self.lock().get(&side_question_id) {
            Some(run) if run.client_id == client_id => {
                run.abort.abort();
                true
            }
            Some(_) | None => false,
        }
    }

    /// Aborts every run `client_id` owns. The entries stay registered: each run drops its own
    /// entry and sends its terminal cancelled event under one registry hold, so a detach racing
    /// a close cannot empty the registry.
    pub fn abort_for_client(&self, client_id: &str) {
        for run in self.lock().values() {
            if run.client_id == client_id {
                run.abort.abort();
            }
        }
    }

    /// Aborts every live run (the session's close) and closes the admission gate: the drained
    /// registry means every cancelled event was sent.
    pub fn abort_all(&self) {
        let runs = self.lock();
        self.closing.store(true, Ordering::SeqCst);
        for run in runs.values() {
            run.abort.abort();
        }
    }

    /// Aborts every live run and waits (up to `settle_timeout`) for their terminal events to
    /// be sent: the cancelled events must reach the client before the session closes.
    pub async fn abort_all_and_settle(&self, settle_timeout: Duration) {
        self.abort_all();
        let deadline = tokio::time::Instant::now() + settle_timeout;
        while !self.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether no run is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The end of run `side_question_id`: its entry drops and `emit` sends its terminal event,
    /// both under one registry hold, so a same-id restart reacting to that event reads the id
    /// as free.
    fn finish(&self, side_question_id: &str, emit: impl FnOnce()) {
        let mut runs = self.lock();
        runs.remove(side_question_id);
        emit();
    }
}

/// Runs admitted run `admitted` of `runs` to its end, sending its events through `emit` in
/// order: a `running` event first, one per partial answer the run reports to its sink, then the
/// terminal event, sent as the run leaves the registry. `run` runs the request under the abort
/// signal with the sink; its error (the run's task ended without an outcome) fails the run.
pub async fn drive<F, Fut>(
    runs: Arc<SideRuns>,
    admitted: AdmittedRun,
    emit: Arc<dyn Fn(Value) + Send + Sync>,
    run: F,
) where
    F: FnOnce(SideQuestionRequest, AbortSignal, SideQuestionSink) -> Fut,
    Fut: Future<Output = Result<SideQuestionOutcome, String>>,
{
    let AdmittedRun { request, abort } = admitted;
    // The events name the run by its id and question only.
    let named = SideQuestionRequest {
        previous_turns: Vec::new(),
        ..request.clone()
    };
    // The run opens with a running event before the engine streams (TS emits `running` at the
    // start of the done chain).
    emit(side_question_event_value(
        &named,
        "",
        SIDE_QUESTION_STATUS_RUNNING,
        None,
    ));
    let sink: SideQuestionSink = {
        let (named, emit) = (named.clone(), Arc::clone(&emit));
        Arc::new(move |answer| {
            emit(side_question_event_value(
                &named,
                answer,
                SIDE_QUESTION_STATUS_RUNNING,
                None,
            ));
            true
        })
    };
    let outcome = run(request, abort.signal(), sink)
        .await
        .unwrap_or_else(|error| SideQuestionOutcome::Failed {
            answer: String::new(),
            error: format!("side question run failed: {error}"),
        });
    let event = side_question_event_value(
        &named,
        outcome.answer(),
        outcome.status_str(),
        outcome.error_message(),
    );
    // The entry drops BEFORE the terminal event goes, both under ONE registry hold: a same-id
    // restart reacting to the cancelled event reads the id as free (with the emit first, TS's
    // own order, a fast restart could read a stale "Side question already exists").
    runs.finish(&named.side_question_id, || emit(event));
}

/// Runs `request` on built session `engine`: a `/btw` question on a clone of its conversation,
/// billed to the session, or a `/better` rewrite on its [`super::better::BetterHost`] (whose
/// progress labels reach `sink`). Neither enters the session's history.
pub async fn run_side_run(
    engine: &super::engine::SessionEngine,
    request: SideQuestionRequest,
    signal: &AbortSignal,
    sink: &SideQuestionSink,
) -> SideQuestionOutcome {
    if request.kind == pa_types::better::SideRunKind::Better {
        let progress: BetterProgress = {
            let sink = Arc::clone(sink);
            Arc::new(move |label: &str| {
                let _ = sink(label);
            })
        };
        let rewrite = BetterRequest {
            task: request.question,
            mode: BetterMode::Deepen,
            cwd: None,
        };
        return match engine.better.run(&rewrite, signal, &progress).await {
            BetterOutcome::Rewritten { rewrite, .. } => {
                SideQuestionOutcome::Complete { answer: rewrite }
            }
            BetterOutcome::Cancelled => SideQuestionOutcome::Aborted {
                answer: String::new(),
            },
            failed @ BetterOutcome::Failed { .. } => SideQuestionOutcome::Failed {
                answer: String::new(),
                error: failed.error_text().unwrap_or_default(),
            },
        };
    }
    // TS `unwrapSemanticEdgeStreamFn`: the side call runs on the session's pre-semantic stream
    // fn, so it carries no request id and the ledger records nothing. The engine build always
    // wires it; no fallback to the agent's own fn, which would leak the id-carrying wrapper
    // into the side call.
    let Some(side_stream_fn) = engine.session.side_question_stream_fn() else {
        return SideQuestionOutcome::Failed {
            answer: String::new(),
            error: "Select a model before asking a side question".to_string(),
        };
    };
    let retry_policy = super::provider_retry::DEFAULT_PROVIDER_RETRY_POLICY;
    let result = super::side_question::run_side_question(
        engine.session.agent(),
        side_stream_fn,
        &request.question,
        &request.previous_turns,
        &retry_policy,
        signal,
        sink,
    )
    .await;
    // The answer never enters the history, but the run's model calls bill the session.
    engine
        .session
        .record_model_usage(result.usage.clone())
        .await;
    match result.status {
        SideQuestionStatus::Complete => SideQuestionOutcome::Complete {
            answer: result.answer,
        },
        SideQuestionStatus::Cancelled => SideQuestionOutcome::Aborted {
            answer: result.answer,
        },
        SideQuestionStatus::Error => SideQuestionOutcome::Failed {
            answer: result.answer,
            error: result
                .error_message
                .unwrap_or_else(|| "Side question failed".to_string()),
        },
    }
}

/// `record_better_outcome`'s `outcome`; the error is the old daemon's refusal text.
///
/// # Errors
///
/// The payload's `outcome` is not one the composer reports.
pub fn better_outcome(payload: &Value) -> Result<pa_types::better::ComposerOutcome, &'static str> {
    payload
        .get("outcome")
        .cloned()
        .and_then(|outcome| serde_json::from_value(outcome).ok())
        .ok_or("outcome must be sent_as_is, edited, or discarded")
}

#[cfg(test)]
mod tests;
