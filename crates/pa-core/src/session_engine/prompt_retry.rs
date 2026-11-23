//! One user prompt driven to its settled reply under the shared provider
//! auto-retry driver ([`super::auto_retry`]): the turn seam of a host that
//! runs the session without the daemon (print mode). The daemon's model turn
//! drives its own streaming attempt through the same driver and the same
//! reply settle ([`super::provider_retry::settle_final_reply`]).

use std::future::Future;

use pa_agent::types::{AgentMessage, ImageContent, Message, StopReason};

use super::auto_retry::{run_turn_with_auto_retry, AutoRetryEvent};
use super::provider_retry::{settle_final_reply, ProviderRetryPolicy};
use super::{AgentSession, PromptOptions, PromptOutcome, TrailingAssistantFilter};

impl AgentSession {
    /// Admit `text` (with `images`) as a user prompt and drive its run until
    /// its final reply settles. A provider failure retries per `policy`, and
    /// a reply that cannot end the turn (cut off at the output limit, or no
    /// visible answer) is a retryable failure that gets one retry. A retry
    /// drops the failed reply from the loop context and continues the run,
    /// so the prompt is admitted once and finished tool work never re-runs.
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
        let settled = run_turn_with_auto_retry(
            policy,
            context_window,
            None,
            || {
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
                        self.drop_trailing_assistant(TrailingAssistantFilter::Any)
                            .await;
                        self.agent.continue_run().await?;
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
            |delay| async move {
                tokio::time::sleep(delay).await;
                true
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
