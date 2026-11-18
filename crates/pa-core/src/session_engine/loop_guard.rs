//! The session's side of the stream loop guard. pa-ai's shared stream entry cuts a stream that
//! starts repeating itself and settles it as a `repetition_loop` failure; the shared retry policy
//! re-issues it once (`provider_retry::REPETITION_LOOP_KIND`). This module carries the session's
//! `loopGuard` setting onto every request, tells the one retried request what went wrong, and
//! nudges a model that re-runs the same tool call with the same arguments round after round
//! (where the text guard cannot see a loop, because each round is a fresh, short stream).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pa_agent::agent_loop::AfterToolCallFn;
use pa_agent::stream::{AssistantMessageEvent, ModelStream, StreamFn};
use pa_agent::types::{
    AfterToolCallResult, AgentMessage, AssistantContent, AssistantMessage, LoopGuardLevel, Message,
    TextContent, ToolResultContent, UserContent, UserMessage, UserPart,
};

use super::provider_retry::{provider_stream_failure_kind, REPETITION_LOOP_KIND};
use super::telemetry::SessionTelemetry;

/// What the retried request is told after its first attempt was cut for repeating itself.
const REPETITION_NUDGE: &str = "[loop guard] Your previous reply was cut off because it started repeating the same text over and over. Do not repeat it: continue from where the work stands, and if you are stuck, say so briefly or take a different approach.";

/// Consecutive rounds with an identical tool call (the current round included) at which the
/// call's result carries a nudge. Polling a job can legitimately repeat a call twice; a third
/// identical round is where a stuck model usually is.
const REPEAT_CALL_ROUNDS: usize = 3;

/// Wrap the session's provider stream: every request carries `level` (`None` leaves it to the
/// model family's default), and the retry of a request whose stream the guard cut carries
/// [`REPETITION_NUDGE`]. The retry is recognized by its conversation: the failed reply leaves the
/// loop context before the retry, so the retried request has exactly the cut request's message
/// count. The nudge rides that one request only and is never persisted.
pub(crate) fn guard_stream_fn(level: Option<LoopGuardLevel>, stream_fn: StreamFn) -> StreamFn {
    let tripped_at: Arc<Mutex<Option<usize>>> = Arc::default();
    Arc::new(move |model, mut context, mut options| {
        options.loop_guard = level;
        let messages_len = context.messages.len();
        let tripped = tripped_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if tripped == Some(messages_len) {
            // Appended to the request's last turn without breaking role alternation: into the
            // last user message or tool result, or as a new user message after an assistant one.
            let text = TextContent {
                text: REPETITION_NUDGE.to_string(),
                text_signature: None,
            };
            match context.messages.last_mut() {
                Some(Message::User(UserMessage {
                    content: UserContent::Text(existing),
                    ..
                })) => {
                    existing.push_str("\n\n");
                    existing.push_str(REPETITION_NUDGE);
                }
                Some(Message::User(UserMessage {
                    content: UserContent::Parts(parts),
                    ..
                })) => parts.push(UserPart::Text(text)),
                Some(Message::ToolResult(result)) => {
                    result.content.push(ToolResultContent::Text(text));
                }
                Some(Message::Assistant(_)) | None => {
                    context.messages.push(Message::User(UserMessage {
                        content: UserContent::Text(REPETITION_NUDGE.to_string()),
                        timestamp: pa_agent::now_ms(),
                    }));
                }
            }
        }
        let started = stream_fn(model, context, options);
        let tripped_at = Arc::clone(&tripped_at);
        Box::pin(async move {
            let inner = started.await?;
            let watched: Box<dyn ModelStream> = Box::new(TripWatch {
                inner,
                tripped_at,
                messages_len,
            });
            Ok(watched)
        })
    })
}

/// A provider stream that remembers, for the next request, that the guard cut it.
struct TripWatch {
    inner: Box<dyn ModelStream>,
    tripped_at: Arc<Mutex<Option<usize>>>,
    messages_len: usize,
}

impl TripWatch {
    fn note(&self, message: &AssistantMessage) {
        if provider_stream_failure_kind(message).as_deref() == Some(REPETITION_LOOP_KIND) {
            *self
                .tripped_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(self.messages_len);
        }
    }
}

impl ModelStream for TripWatch {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(async move {
            let event = self.inner.next_event().await;
            if let Some(AssistantMessageEvent::Error { error, .. }) = &event {
                self.note(error);
            }
            event
        })
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async move {
            let result = self.inner.result().await;
            if let Ok(message) = &result {
                self.note(message);
            }
            result
        })
    }

    fn close(&mut self) {
        self.inner.close();
    }
}

/// The after-tool-call hook that nudges a model re-running the same call: when this round's call
/// repeats the previous rounds' call with identical arguments [`REPEAT_CALL_ROUNDS`] rounds in a
/// row, its result gains a note saying so. The run is not stopped: polling can be legitimate,
/// and the model decides. `telemetry` counts each nudge once the session's telemetry is
/// installed.
pub(crate) fn repeat_call_nudge(
    telemetry: Arc<OnceLock<Arc<SessionTelemetry>>>,
) -> AfterToolCallFn {
    Arc::new(move |context, _signal| {
        let telemetry = Arc::clone(&telemetry);
        Box::pin(async move {
            // Consecutive assistant rounds, newest first (this one included), holding a call to
            // the same tool with identical arguments. Tool results between rounds are skipped;
            // any other message (a user turn, a harness notice) ends the run.
            let call = &context.tool_call;
            let mut rounds = 0;
            for message in context.context.messages.iter().rev() {
                match message {
                    AgentMessage::Standard(Message::Assistant(assistant)) => {
                        let repeats = assistant.content.iter().any(|block| {
                            matches!(block, AssistantContent::ToolCall(earlier)
                                if earlier.name == call.name && earlier.arguments == call.arguments)
                        });
                        if !repeats {
                            break;
                        }
                        rounds += 1;
                    }
                    AgentMessage::Standard(Message::ToolResult(_)) => {}
                    AgentMessage::Standard(Message::User(_)) | AgentMessage::Custom(_) => break,
                }
            }
            if rounds < REPEAT_CALL_ROUNDS {
                return Ok(None);
            }
            if let Some(telemetry) = telemetry.get() {
                telemetry.note_tool_repeat_nudge();
            }
            let mut content = context.result.content;
            content.push(ToolResultContent::text(format!(
                "[loop guard] This is round {rounds} in a row calling `{}` with identical arguments. If its result is not changing, do not run it again unchanged: change the input or take a different approach.",
                context.tool_call.name
            )));
            Ok(Some(AfterToolCallResult {
                content: Some(content),
                details: None,
                is_error: None,
                terminate: None,
            }))
        })
    })
}

#[cfg(test)]
mod tests;
