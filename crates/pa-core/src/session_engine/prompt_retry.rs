//! One user prompt driven to its settled reply under the shared provider
//! auto-retry driver ([`super::auto_retry`]): the turn seam of a host that
//! runs the session without the daemon (print mode). The daemon's model turn
//! drives its own streaming attempt through the same driver and the same
//! reply settle ([`super::provider_retry::settle_final_reply`]). Both hosts
//! continue a reply cut off at the output limit through
//! [`AgentSession::continue_after_output_limit`].

use std::future::Future;

use pa_agent::types::{AgentMessage, ImageContent, Message, StopReason};

use super::auto_retry::{run_turn_with_auto_retry, AutoRetryEvent, TurnAttempt};
use super::messages::{OUTPUT_LIMIT_CONTINUATION_CUSTOM_TYPE, OUTPUT_LIMIT_CONTINUATION_TEXT};
use super::provider_retry::{settle_final_reply, ProviderRetryPolicy};
use super::{
    session_message_to_loop, AgentSession, PromptOptions, PromptOutcome, SessionAgentMessage,
    TrailingAssistantFilter,
};

impl AgentSession {
    /// Stops the automatic retry the running prompt waits on
    /// ([`Self::prompt_with_auto_retry`]): its wait ends at once and the turn
    /// settles with the failure it retried, its retry cancelled. The next
    /// prompt starts with no retry stopped.
    ///
    /// # Panics
    ///
    /// Panics when the retry abort slot's mutex is poisoned.
    pub fn abort_retry(&self) {
        self.retry_abort.lock().expect("retry abort lock").abort();
    }

    /// Continue a run whose final reply was cut off at the output limit
    /// ([`TurnAttempt::ContinueAfterOutputLimit`]): the cut reply stays in
    /// the loop context, and the guiding row
    /// ([`OUTPUT_LIMIT_CONTINUATION_TEXT`]) is admitted after it, so the
    /// transcript keeps it and the next request carries it. The run stays
    /// one run in telemetry and counts the continuation.
    ///
    /// # Errors
    ///
    /// The agent's refusal to start the run (one is already active), or the
    /// run's failure.
    pub async fn continue_after_output_limit(&self) -> anyhow::Result<()> {
        if let Some(telemetry) = &self.skill_telemetry {
            telemetry.note_output_limit_continuation();
        }
        let row = session_message_to_loop(&SessionAgentMessage::Custom(
            pa_types::session::CustomMessage {
                custom_type: OUTPUT_LIMIT_CONTINUATION_CUSTOM_TYPE.to_string(),
                content: pa_types::ai::UserContent::Text(
                    OUTPUT_LIMIT_CONTINUATION_TEXT.to_string(),
                ),
                display: false,
                details: None,
                timestamp: super::refine::now_millis(),
                rest: serde_json::Map::default(),
            },
        ))
        .ok_or_else(|| anyhow::anyhow!("output-limit continuation row conversion failed"))?;
        self.agent
            .prompt(pa_agent::agent::AgentPromptInput::Messages(vec![row]))
            .await
    }

    /// Admit `text` (with `images`) as a user prompt and drive its run until
    /// its final reply settles. A provider failure retries per `policy`, and
    /// a reply with no visible answer is a retryable failure that gets one
    /// retry, and a reply cut off at the output limit is continued
    /// ([`Self::continue_after_output_limit`]). A retry drops the failed
    /// reply from the loop context and continues the run, so the prompt is
    /// admitted once and finished tool work never re-runs.
    /// `emit` observes the retry events.
    ///
    /// A turn that still fails leaves its failure as the loop's trailing
    /// assistant message (stop reason `error`, the reason as its error
    /// text), the same shape a provider failure leaves, so the headless
    /// terminal selection reports it.
    ///
    /// # Errors
    ///
    /// The admission's or the continuation's error, the `emit` callback's
    /// error, or an error when the run settles with no assistant reply or
    /// `text` is a session command (which never reaches the model).
    ///
    /// # Panics
    ///
    /// Panics when the retry abort slot's mutex is poisoned.
    pub async fn prompt_with_auto_retry<E, EF>(
        &self,
        text: &str,
        images: Vec<ImageContent>,
        policy: &ProviderRetryPolicy,
        context_window: u64,
        emit: E,
    ) -> anyhow::Result<()>
    where
        E: FnMut(AutoRetryEvent) -> EF,
        EF: Future<Output = anyhow::Result<()>>,
    {
        // The images ride the first attempt only; a retry continues the run.
        let mut admission = Some(images);
        // Each prompt's retries answer `abort_retry` from its start.
        let retry_abort = {
            let mut slot = self.retry_abort.lock().expect("retry abort lock");
            *slot = pa_agent::abort::AbortController::new();
            slot.signal()
        };
        let settled = run_turn_with_auto_retry(
            policy,
            context_window,
            Some(&retry_abort),
            |attempt| {
                let admission = admission.take();
                async move {
                    if let Some(images) = admission {
                        match self
                            .prompt_with_images(text, images, PromptOptions::default())
                            .await?
                        {
                            PromptOutcome::Prompt => {}
                            PromptOutcome::SessionCommand(_) => {
                                anyhow::bail!("A session command is not a model turn");
                            }
                        }
                    } else {
                        match attempt {
                            TurnAttempt::ContinueAfterOutputLimit => {
                                self.continue_after_output_limit().await?;
                            }
                            TurnAttempt::First | TurnAttempt::Retry => {
                                self.drop_trailing_assistant(TrailingAssistantFilter::Any)
                                    .await;
                                self.agent.continue_run().await?;
                            }
                        }
                    }
                    self.agent.wait_for_idle().await;
                    let reply = self
                        .agent
                        .state()
                        .await
                        .messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            AgentMessage::Standard(Message::Assistant(assistant)) => {
                                Some(assistant.clone())
                            }
                            AgentMessage::Standard(Message::User(_) | Message::ToolResult(_))
                            | AgentMessage::Custom(_) => None,
                        })
                        .ok_or_else(|| anyhow::anyhow!("No response produced."))?;
                    Ok(settle_final_reply(
                        reply,
                        /*owes_answer*/ true,
                        context_window,
                    ))
                }
            },
            emit,
            |delay| {
                let retry_abort = retry_abort.clone();
                async move {
                    tokio::select! {
                        () = tokio::time::sleep(delay) => true,
                        () = retry_abort.aborted() => false,
                    }
                }
            },
            None,
            None,
        )
        .await?;
        // A settled incomplete reply is a failure only in the driver's copy:
        // the loop still holds the reply as streamed, so the failure takes
        // its place (a provider failure's copy is the same message).
        if settled.stop_reason == StopReason::Error {
            self.agent
                .mutate_messages(|messages| {
                    if let Some(AgentMessage::Standard(Message::Assistant(last))) =
                        messages.iter_mut().rev().find(|message| {
                            matches!(message, AgentMessage::Standard(Message::Assistant(_)))
                        })
                    {
                        *last = settled;
                    }
                })
                .await;
        }
        Ok(())
    }
}
