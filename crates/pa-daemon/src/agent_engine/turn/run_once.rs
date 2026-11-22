//! The once-runner: one model-turn attempt with its retry policy and the wire-shape serializers for stream events, tool
//! results, and agent messages.
use crate::engine::{session_wire_value, AssistantSnapshot};

use super::{json, json_round_trip, AgentSessionEngine, EngineEvent, TurnOnce, TurnPrompt, Value};

/// The wire value of a settled loop message; an assistant message carries the session's
/// billing stamp, like the core's own record of it.
async fn stamped_wire_value(
    billing: Option<&pa_core::session_engine::billing::SessionBilling>,
    agent_message: &pa_agent::types::AgentMessage,
) -> Option<Value> {
    let mut value = session_wire_value(agent_message)?;
    if let (
        Some(billing),
        pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(assistant)),
    ) = (billing, agent_message)
    {
        let message = json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)?;
        if let Some(mode) = billing.mode_of(&message).await {
            value["usage"]["billing"] = json!(mode);
        }
    }
    Some(value)
}

impl AgentSessionEngine {
    pub(super) fn retry_policy(
        &self,
    ) -> pa_core::session_engine::provider_retry::ProviderRetryPolicy {
        pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir)
            .get_provider_retry_policy()
    }

    /// Run one turn, streaming assistant updates through `emit` as they
    /// arrive. The first attempt prompts the session; retries continue the
    /// parked turn. Returns the final assistant message, `None` when none
    /// was produced, or `Aborted` when the emit cancelled or the cancel flag
    /// raced the admission.
    pub(super) async fn run_turn_once(
        &self,
        agent: &std::sync::Arc<pa_agent::agent::Agent>,
        prompt: &TurnPrompt,
        first_attempt: bool,
        boundary_passed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<TurnOnce> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
        // Goal usage accounting at the message_end hook: each settled non-error
        // assistant message records its token delta, from the engine mirror.
        let goal_runtime = self.live_handles().map(|live| live.goal.clone());
        let goal_budget_crossed = std::sync::Arc::clone(&self.goal_budget_crossed);
        // The run-opening boundary frames are the worker's own for the
        // item's first run; this subscription forwards them only once a
        // boundary frame already passed.
        let boundary_passed = std::sync::Arc::clone(boundary_passed);
        // The worker records its own copy of each settled assistant message: it carries the
        // session's billing stamp like the core's record.
        let billing = self
            .session
            .lock()
            .await
            .as_ref()
            .map(|engine| engine.session.billing());
        let subscription = {
            let tx = tx.clone();
            let boundary_passed = std::sync::Arc::clone(&boundary_passed);
            let autonomous_state = std::sync::Arc::clone(&self.autonomous);
            let autonomous_driver = std::sync::Arc::clone(
                &*self
                    .autonomous_driver
                    .read()
                    .expect("autonomous driver lock"),
            );
            agent
                .subscribe(move |event, _signal| {
                    let tx = tx.clone();
                    let billing = billing.clone();
                    let boundary_passed = std::sync::Arc::clone(&boundary_passed);
                    let autonomous_state = std::sync::Arc::clone(&autonomous_state);
                    let autonomous_driver = std::sync::Arc::clone(&autonomous_driver);
                    let goal_runtime = goal_runtime.clone();
                    let goal_budget_crossed = goal_budget_crossed.clone();
                    Box::pin(async move {
                        use pa_agent::types::AgentEvent;
                        if let AgentEvent::MessageEnd {
                            message:
                                pa_agent::types::AgentMessage::Standard(
                                    pa_agent::types::Message::Assistant(assistant),
                                ),
                        } = &event
                        {
                            if let Some(message) =
                                json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)
                            {
                                let mut state = autonomous_state.lock().await;
                                autonomous_driver.account_message(&mut state, &message);
                                // Goal accounting: only turns neither errors
                                // nor aborted spend the budget, only while
                                // active.
                                if let Some(handles) = goal_runtime.as_ref() {
                                    if !matches!(
                                        message.stop_reason,
                                        pa_types::ai::StopReason::Error
                                            | pa_types::ai::StopReason::Aborted
                                    ) {
                                        let mut driver = handles.driver.lock().await;
                                        let mut session = handles.session.lock().await;
                                // The timestamp is the double-counting guard identity (no
                                // in-process ids).
                                        let message_id = format!("a-{}", message.timestamp);
                                        // A budget crossing moves the goal to
                                        // `budget_limited` (the wrapper publishes
                                        // the `goal_update`; the boundary mints
                                        // the wrap-up steer); a failed persist
                                        // only warns.
                                        match driver
                                            .record_assistant_usage(&mut session, &message_id, &message.usage)
                                        {
                                            Ok(
                                                pa_core::session_engine::goal_driver::UsageOutcome::BudgetReached,
                                            ) => {
                                                goal_budget_crossed
                                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                                            }
                                            Ok(_) => {}
                                            Err(error) => {
                                                eprintln!(
                                                    "pa-daemon: goal usage accounting persist failed: {error:#}"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        match &event {
                            AgentEvent::MessageStart {
                                message: agent_message,
                            } => {
                                if matches!(
                                    agent_message,
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                    )
                                ) {
                                    if let Some(value) = session_wire_value(agent_message) {
                                        let _ = tx.send(EngineEvent::AssistantUpdate {
                                            message: AssistantSnapshot::Wire(value),
                                            stream_event: Some(json!({ "type": "start" })),
                                        });
                                    }
                                }
                            }
                            AgentEvent::MessageUpdate {
                                message: agent_message,
                                assistant_message_event: stream_event,
                            } => {
                                if matches!(
                                    agent_message.as_ref(),
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                    )
                                ) {
                                    let _ = tx.send(EngineEvent::AssistantUpdate {
                                        message: AssistantSnapshot::Loop(std::sync::Arc::clone(
                                            agent_message,
                                        )),
                                        stream_event: stream_event_value(stream_event),
                                    });
                                }
                            }
                            AgentEvent::MessageEnd {
                                message: agent_message,
                            } => {
                                // Settled messages persist as entries and reach
                                // clients: every assistant and tool-result
                                // message.
                                match agent_message {
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                            | pa_agent::types::Message::ToolResult(_),
                                    ) => {
                                        if let Some(value) =
                                            stamped_wire_value(billing.as_ref(), agent_message)
                                                .await
                                        {
                                            let event = if matches!(
                                                agent_message,
                                                pa_agent::types::AgentMessage::Standard(
                                                    pa_agent::types::Message::ToolResult(_)
                                                )
                                            ) {
                                                EngineEvent::ToolResultMessage(value)
                                            } else {
                                                EngineEvent::AssistantMessage(value)
                                            };
                                            let _ = tx.send(event);
                                        }
                                    }
                                    // An in-run continuation's user row: the
                                    // `boundary_passed` gate separates it from
                                    // the admitted prompt's row, which the loop
                                    // already emitted.
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::User(_),
                                    ) if boundary_passed
                                        .load(std::sync::atomic::Ordering::SeqCst) =>
                                    {
                                        if let Some(value) = session_wire_value(agent_message) {
                                            let _ = tx.send(EngineEvent::UserMessage(value));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            // The loop-boundary frames: the run-opening frames stay with the
                            // worker's first run (the `boundary_passed` gate); `turn_end` carries
                            // the terminal message plus tool results, `agent_end` the whole set.
                            AgentEvent::TurnStart => {
                                if boundary_passed.load(std::sync::atomic::Ordering::SeqCst) {
                                    let _ = tx.send(EngineEvent::TurnStart);
                                }
                            }
                            AgentEvent::AgentStart => {
                                if boundary_passed.load(std::sync::atomic::Ordering::SeqCst) {
                                    let _ = tx.send(EngineEvent::AgentStart);
                                }
                            }
                            AgentEvent::AgentEnd { messages } => {
                                let messages = messages
                                    .iter()
                                    .filter_map(session_wire_value)
                                    .collect::<Vec<Value>>();
                                boundary_passed.store(true, std::sync::atomic::Ordering::SeqCst);
                                let _ = tx.send(EngineEvent::AgentEnd { messages });
                            }
                            AgentEvent::TurnEnd {
                                message,
                                tool_results,
                            } => {
                                if let Some(message) =
                                    stamped_wire_value(billing.as_ref(), message).await
                                {
                                    let tool_results = tool_results
                                        .iter()
                                        .filter_map(|result| {
                                            session_wire_value(&pa_agent::types::AgentMessage::from(
                                                result.clone(),
                                            ))
                                        })
                                        .collect::<Vec<Value>>();
                                    boundary_passed
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    let _ = tx.send(EngineEvent::TurnEnd {
                                        message,
                                        tool_results,
                                    });
                                }
                            }
                            AgentEvent::ToolExecutionStart {
                                tool_call_id,
                                tool_name,
                                args,
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionStart {
                                    tool_call_id: tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    args: args.clone(),
                                });
                            }
                            AgentEvent::ToolExecutionProgress { .. } => {}
                            AgentEvent::ToolExecutionUpdate {
                                tool_call_id,
                                partial_result,
                                ..
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionUpdate {
                                    tool_call_id: tool_call_id.clone(),
                                    partial_result: tool_result_wire_value(partial_result),
                                });
                            }
                            AgentEvent::ToolExecutionEnd {
                                tool_call_id,
                                result,
                                is_error,
                                ..
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionEnd {
                                    tool_call_id: tool_call_id.clone(),
                                    result: tool_result_wire_value(result),
                                    is_error: *is_error,
                                });
                            }
                        }
                        Ok(())
                    })
                })
                .await
        };
        // Admit the turn without blocking the forwarding loop: the admission
        // future settles only when the whole turn settles, while the loop
        // hands each streamed event to `emit` the moment it arrives (buffering
        // made clients render a turn as one final batch).
        let prompt = prompt.clone();
        // The same admission consult as [`Self::run_model_turn`]'s, at the
        // admission future's head: the whole [pickup, registration] window
        // honours the delivery's cancel flag.
        let abort_raced_admission = std::sync::atomic::AtomicBool::new(false);
        let mut admitted = std::pin::pin!(async {
            if aborted() {
                abort_raced_admission.store(true, std::sync::atomic::Ordering::SeqCst);
                return Ok(());
            }
            if first_attempt {
                // The session lock covers the clone only: holding it across
                // the turn serialized every client read seam (the 2026-09-22
                // dogfood failure); the Arc clone keeps reads free.
                let session = self.session.lock().await.clone();
                let engine = session.expect("session built");
                if pa_types::session_scope::var_os("PA_COMPACTION_TRACE").is_some() {
                    let state = engine.session.agent().state().await;
                    pa_core::session_engine::compaction_trace::trace(
                        "daemon.before_prompt_admission",
                        &json!({
                            "liveUserRows": state.messages.iter().filter(|message|
                                matches!(message, pa_agent::types::AgentMessage::Standard(
                                    pa_agent::types::Message::User(_)))).count(),
                        }),
                    );
                }
                match &prompt {
                    // A plain turn admits a user prompt; an injected turn admits the
                    // custom row itself — ONE representation.
                    TurnPrompt::User {
                        text,
                        images,
                        batch,
                    } => {
                        // The batched co-delivery rows ride the same admission: one run over
                        // the primary plus every batched row.
                        let options = pa_core::session_engine::PromptOptions {
                            batch: batch
                                .iter()
                                .map(|row| pa_core::session_engine::PromptBatchRow {
                                    text: row.text.clone(),
                                    images: row.images.clone(),
                                })
                                .collect(),
                            ..Default::default()
                        };
                        engine
                            .session
                            .prompt_with_images(text, images.clone(), options)
                            .await
                            .map(|_| ())
                    }
                    TurnPrompt::Injected(message) => engine
                        .session
                        .prompt_injected_message(message)
                        .await
                        .map(|_| ()),
                }
            } else {
                agent.continue_run().await
            }
        });
        let mut aborted = false;
        let mut admission_error: Option<anyhow::Error> = None;
        let mut settled = false;
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(event) => {
                            if !emit(event) {
                                aborted = true;
                            }
                        }
                        None => break,
                    }
                }
                outcome = &mut admitted => {
                    settled = true;
                    match outcome {
                        Ok(()) => {}
                        Err(error) => admission_error = Some(error),
                    }
                    while let Ok(event) = rx.try_recv() {
                        if !emit(event) {
                            aborted = true;
                            break;
                        }
                    }
                }
            }
            if aborted || settled {
                break;
            }
        }
        if aborted && !settled {
            // The emit callback cancelled the turn: stop the still-running
            // admission and wait out its abort path. A settled admission
            // must not be re-polled (pinned futures panic after completion).
            agent.abort();
            let _ = (&mut admitted).await;
        }
        // The settled run's tail still holds the aborted assistant row: drain
        // every queued event.
        while let Ok(event) = rx.try_recv() {
            let _ = emit(event);
        }
        let () = subscription.unsubscribe().await;
        if aborted {
            return Ok(TurnOnce::Aborted);
        }
        // The admission consult fired: the turn never started — the aborted
        // outcome, never an admission error the retry driver would re-issue.
        if abort_raced_admission.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(TurnOnce::Aborted);
        }
        if let Some(error) = admission_error {
            // A Stop that lands inside the admitted run settles it on the typed abort: the
            // aborted outcome, never a turn error that fails an active goal (the re-wrap
            // below drops the type).
            if pa_agent::abort::is_abort_error(&error) {
                return Ok(TurnOnce::Aborted);
            }
            return Err(anyhow::anyhow!("{error:#}"));
        }
        let state = agent.state().await;
        for message in state.messages.iter().rev() {
            if let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) = message
            {
                // A round-trip failure means no usable outcome.
                if json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant).is_none() {
                    return Ok(TurnOnce::None);
                }
                return Ok(TurnOnce::Message {
                    assistant: Box::new(assistant.clone()),
                });
            }
        }
        Ok(TurnOnce::None)
    }
}

/// Wire form of one provider stream event: the event `type` plus the
/// `delta` when the event carries one.
fn stream_event_value(event: &pa_agent::stream::AssistantMessageEvent) -> Option<Value> {
    use pa_agent::stream::AssistantMessageEvent;
    let (kind, delta) = match event {
        AssistantMessageEvent::Start { .. } => ("start", None),
        AssistantMessageEvent::TextStart { .. } => ("text_start", None),
        AssistantMessageEvent::TextDelta { delta, .. } => ("text_delta", Some(delta.as_str())),
        AssistantMessageEvent::TextEnd { .. } => ("text_end", None),
        AssistantMessageEvent::ThinkingStart { .. } => ("thinking_start", None),
        AssistantMessageEvent::ThinkingDelta { delta, .. } => {
            ("thinking_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ThinkingEnd { .. } => ("thinking_end", None),
        AssistantMessageEvent::ToolCallStart { .. } => ("toolcall_start", None),
        AssistantMessageEvent::ToolCallDelta { delta, .. } => {
            ("toolcall_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ToolCallEnd { .. } => ("toolcall_end", None),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => return None,
    };
    match delta {
        Some(delta) => Some(json!({ "type": kind, "delta": delta })),
        None => Some(json!({ "type": kind })),
    }
}

/// Wire form of one tool result (the TS tool-execution event payload).
fn tool_result_wire_value(result: &pa_agent::types::AgentToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|block| serde_json::to_value(block).unwrap_or(Value::Null))
        .collect();
    json!({ "content": content, "details": result.details })
}
