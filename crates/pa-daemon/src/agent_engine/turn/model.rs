//! The model-turn runner: the streaming run over the built session agent,
//! and the same-provider retry policy application.
use super::{
    aborted_message, drop_trailing_assistant, retry_event_to_engine_event, AgentSessionEngine,
    EngineEvent, StopReason, TurnAdmission, TurnOnce, TurnPrompt, TurnResult,
};

impl AgentSessionEngine {
    /// Drive one admitted prompt through the retry-driver model loop and
    /// emit the turn outcome (provider-failure retries + final-row
    /// surfacing). The trailing `Done` is owned by the caller.
    pub(super) fn run_model_turn(
        &self,
        admission: TurnAdmission,
        prompt: &TurnPrompt,
        boundary_passed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> TurnResult {
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return TurnResult::Error {
                    error: error.to_string(),
                    assistant: None,
                };
            }
        };
        // The preflight validates the model SERVING the run: a model whose
        // provider has no configured credential fails the run before the
        // provider request, with the login-guidance message — a routed
        // image-model episode authenticates its own image model.
        let preflight_model = self
            .armed_image_route()
            .map_or_else(|| model.clone(), |route| route.target.model);
        if self.config.faux_script.is_none() && self.current_selection().api_key.is_none() {
            let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
            let registry = pa_core::models::ModelRegistry::create(
                auth,
                self.config.agent_dir.join("models.json"),
            );
            if !registry.has_configured_auth(&preflight_model) {
                let uses_oauth = registry
                    .auth
                    .get_all()
                    .credential(&preflight_model.provider)
                    .is_some_and(|credential| {
                        matches!(credential, pa_core::auth::AuthCredential::Oauth { .. })
                    });
                let message = if uses_oauth {
                    format!(
                        "Authentication failed for \"{}\". Credentials may have expired or network is unavailable.\n\nRun /login to update credentials.",
                        preflight_model.provider
                    )
                } else {
                    let docs = pa_core::packages::docs_path();
                    format!(
                        "No API key found for {}.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
                        preflight_model.provider,
                        docs.join("providers.md").display(),
                        docs.join("models.md").display()
                    )
                };
                return TurnResult::Error {
                    error: message,
                    assistant: None,
                };
            }
        }
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                return TurnResult::Error {
                    error: format!("{error:#}"),
                    assistant: None,
                };
            }
        };
        // The delivery's cancel flag is consulted at the admission, before
        // the agent run registers: an abort landing in the [pickup,
        // registration] window is otherwise lost. Delivery-scoped by
        // construction.
        if aborted() {
            return TurnResult::Aborted;
        }
        // A routed image-model episode applies its override BEFORE the
        // first provider call: the serving target swaps to the image
        // model (the agent state itself never swaps — per-run, like TS).
        self.apply_armed_image_route(&agent);
        let policy = self.retry_policy();
        // The pa-core retry driver owns the attempt loop; this engine
        // owns one turn. The single `emit` reference is handed through
        // a RefCell slot to whichever closure is currently running.
        let emit_cell = std::cell::RefCell::new(emit);
        // The overflow compact-and-retry re-issues the loop without a new
        // user message, so its turn starts as a continuation (TS
        // `agent.continue()`); ordinary turns start fresh.
        let first_attempt = std::cell::Cell::new(matches!(admission, TurnAdmission::FreshPrompt));
        // Retry adoption telemetry (TS `auto_retry_start` counting):
        // retries increment `retry_count`.
        // The semantic-edge recorder rides the same snapshot (TS
        // `prepareTurnRetry` / `clearTurnRetry` ride the retry events: the
        // parked id makes the retried call reuse the failed attempt's
        // idempotency key).
        let (telemetry, semantic_edges) = {
            let guard = self.session.blocking_lock();
            (
                guard.as_deref().and_then(|engine| engine.telemetry.clone()),
                guard
                    .as_deref()
                    .and_then(|engine| engine.session.semantic_edges()),
            )
        };
        // The auth-retry seam: refresh the rejected credential before the retry.
        // The weak self keeps the callback `'static`.
        let recovery_engine = self
            .self_weak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // The quota-fallback seam: a quota failure arms the serving model's
        // declared fallback credential for one immediate retry.
        let fallback_engine = recovery_engine.clone();
        let fallback_model = preflight_model;
        let quota_fallback: Option<pa_core::session_engine::auto_retry::QuotaFallbackCallback> =
            Some(&mut move |_message, retry_after_ms| {
                let engine = fallback_engine.as_ref().and_then(std::sync::Weak::upgrade);
                let model = fallback_model.clone();
                Box::pin(async move {
                    let Some(engine) = engine else {
                        return pa_core::models::QuotaFallback::NotDeclared;
                    };
                    tokio::task::spawn_blocking(move || {
                        engine.arm_quota_fallback(&model, retry_after_ms)
                    })
                    .await
                    .unwrap_or(pa_core::models::QuotaFallback::NotDeclared)
                })
            });
        let auth_recovery: Option<pa_core::session_engine::auto_retry::AuthRecoveryCallback> =
            Some(&mut move |message| {
                let engine = recovery_engine.as_ref().and_then(std::sync::Weak::upgrade);
                let provider = message.provider.clone();
                Box::pin(async move {
                    use pa_core::session_engine::auto_retry::AuthRecovery;
                    let Some(engine) = engine else {
                        return AuthRecovery::Retry;
                    };
                    let refreshed = tokio::task::spawn_blocking(move || {
                        engine.refresh_rejected_request_auth(&provider)
                    })
                    .await;
                    match refreshed {
                        Ok(Err(failure)) => AuthRecovery::EndTurn(failure.to_string()),
                        Ok(Ok(())) | Err(_) => AuthRecovery::Retry,
                    }
                })
            });
        let overflow_window = match self.armed_image_route() {
            Some(route) => route.target.model.context_window,
            None => model.context_window,
        };
        // A final reply that cannot end the turn (cut off at the output
        // limit, or no visible text where an answer is owed) becomes a
        // retryable failure for the retry driver. Custom-row turns (harness
        // notices, agent messages) owe no answer: silence acknowledges them.
        let owes_answer = matches!(prompt, TurnPrompt::User { .. });
        let settle = move |assistant: pa_agent::types::AssistantMessage| {
            use pa_core::session_engine::provider_retry::{
                incomplete_reply_failure, incomplete_reply_reason,
            };
            match incomplete_reply_reason(&assistant, owes_answer, overflow_window) {
                Some(reason) => incomplete_reply_failure(assistant, reason),
                None => assistant,
            }
        };
        let result = self.runtime.block_on(
            pa_core::session_engine::auto_retry::run_turn_with_auto_retry(
                &policy,
                overflow_window,
                None,
                || {
                    let mut emit = emit_cell.borrow_mut();
                    let first = first_attempt.get();
                    first_attempt.set(false);
                    let agent = agent.clone();
                    let prompt = prompt.clone();
                    let model = model.clone();
                    async move {
                        // A retry re-issues the failed turn: the failed
                        // assistant message leaves the loop context first
                        // (TS `messages.slice(0, -1)`), then `continue`.
                        if !first {
                            drop_trailing_assistant(&agent).await;
                        }
                        let mut admit_prompt = first;
                        loop {
                            let assistant = match self
                                .run_turn_once(
                                    &agent,
                                    &prompt,
                                    admit_prompt,
                                    boundary_passed,
                                    aborted,
                                    &mut **emit,
                                )
                                .await
                            {
                                Ok(TurnOnce::Message { assistant }) => *assistant,
                                Ok(TurnOnce::None) => {
                                    return Err(anyhow::anyhow!("No response produced."));
                                }
                                Ok(TurnOnce::Aborted) => return Ok(aborted_message(&model)),
                                Err(error) => return Err(error),
                            };
                            if !self
                                .threshold_yielded
                                .swap(false, std::sync::atomic::Ordering::SeqCst)
                            {
                                return Ok(settle(assistant));
                            }
                            // Requested boundaries stay owned by run_turns; consuming one here
                            // would lose its explicit stop and goal-continuation semantics.
                            let core = self.session.lock().await.clone();
                            let pending = match core {
                                Some(core) => {
                                    core.turn_boundary.compaction_scheduled().await
                                        || core.turn_boundary.refine_pending().await
                                }
                                None => false,
                            };
                            if aborted() {
                                return Ok(aborted_message(&model));
                            }
                            if pending
                                || self
                                    .config
                                    .queued_steering_probe
                                    .as_ref()
                                    .is_some_and(|probe| probe())
                            {
                                return Ok(settle(assistant));
                            }
                            if aborted() {
                                return Ok(aborted_message(&model));
                            }
                            // Keep the retry episode alive across this artificial
                            // yield. The engine owns a multi-thread runtime; the synchronous
                            // compaction boundary may re-enter it without holding async locks.
                            let outcome = tokio::task::block_in_place(|| {
                                self.run_auto_compaction(&mut **emit)
                            });
                            self.threshold_checks_enabled.store(
                                outcome.continues_threshold_checks(),
                                std::sync::atomic::Ordering::SeqCst,
                            );
                            if outcome == crate::auto_compaction::AutoCompactionRun::Cancelled
                                || aborted()
                            {
                                return Ok(aborted_message(&model));
                            }
                            if self
                                .config
                                .queued_steering_probe
                                .as_ref()
                                .is_some_and(|probe| probe())
                            {
                                return Ok(settle(assistant));
                            }
                            // Continue the tool run without dropping its successful assistant
                            // or inserting another copy of the admitted user prompt.
                            admit_prompt = false;
                        }
                    }
                },
                |event| {
                    let mut emit = emit_cell.borrow_mut();
                    let telemetry = telemetry.clone();
                    let semantic_edges = semantic_edges.clone();
                    async move {
                        // TS parks the id at `auto_retry_start` and clears
                        // it at the settle (`_resolveRetry`), so a
                        // body-identical retry reuses the failed
                        // attempt's id; the parked id lives until the
                        // next turn mints over it otherwise.
                        if let Some(recorder) = &semantic_edges {
                            match &event {
                                pa_core::session_engine::auto_retry::AutoRetryEvent::Start {
                                    ..
                                } => recorder.prepare_turn_retry(),
                                pa_core::session_engine::auto_retry::AutoRetryEvent::End {
                                    ..
                                } => {
                                    recorder.clear_turn_retry();
                                }
                            }
                        }
                        if let Some(telemetry) = &telemetry {
                            // One retry event in, one telemetry seam out.
                            telemetry.note_auto_retry_event(&event);
                        }
                        let engine_event = retry_event_to_engine_event(event);
                        if !emit(engine_event) {
                            anyhow::bail!("emit cancelled");
                        }
                        Ok(())
                    }
                },
                |delay| {
                    async move {
                        // Abort-aware wait: the cancel flag stops the retry sleep early (TS
                        // `_retryAbortController`).
                        let deadline = tokio::time::Instant::now() + delay;
                        loop {
                            if aborted() {
                                return false;
                            }
                            if tokio::time::Instant::now() >= deadline {
                                return true;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                },
                auth_recovery,
                quota_fallback,
            ),
        );
        match result {
            Ok(message) => match message.stop_reason {
                // The failure already reached the transcript as the final
                // assistant message; the turn error still travels to
                // headless callers through the turn result.
                StopReason::Error => TurnResult::Error {
                    error: message
                        .error_message
                        .clone()
                        .filter(|error| !error.is_empty())
                        .unwrap_or_else(|| "Assistant response failed".to_string()),
                    assistant: Some(Box::new(message)),
                },
                StopReason::Aborted => TurnResult::Aborted,
                _ => TurnResult::Message(Box::new(message)),
            },
            Err(error) => TurnResult::Error {
                error: error.to_string(),
                assistant: None,
            },
        }
    }
}
