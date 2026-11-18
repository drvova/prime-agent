//! Stream loop guard: a model stuck repeating itself mid-generation streams until its output
//! limit, and no idle watchdog fires on a lively stream. The guard reads what one stream emits,
//! as the shared event writer sees it, and on a loop cuts the stream: it cancels the provider
//! request through the stream's abort token and settles the stream as a `repetition_loop`
//! failure, which the shared retry policy re-issues once.
//!
//! Adapted from Nodecode's `loop-guard.lisp` (MIT), which ports oh-my-pi's thinking-loop guard
//! (MIT). The level follows the model family unless the request names one: off for Claude, whose
//! summarized thinking restates status lines while hidden work runs; semantic for the Gemini,
//! `DeepSeek` and Grok families, which drift into paraphrased loops; exact for every other model.

mod detector;

use tokio_util::sync::CancellationToken;

use crate::event_stream::AssistantMessageEventExt;
use crate::types::{AssistantMessageEvent, ErrorStopReason, LoopGuardLevel, Model, StopReason};
use crate::utils_inner::stream_failure::{
    record_stream_failure, stream_failure_message, ProviderError, StreamFailureError,
    StreamFailureInfo, StreamFailureKind,
};
use detector::LoopDetector;

/// The level a model is guarded at when the request names none.
#[must_use]
pub(crate) fn default_level(model_id: &str) -> LoopGuardLevel {
    let id = model_id.to_ascii_lowercase();
    if id.contains("claude") {
        LoopGuardLevel::Off
    } else if ["gemini", "deepseek", "grok"]
        .iter()
        .any(|family| id.contains(family))
    {
        LoopGuardLevel::Semantic
    } else {
        LoopGuardLevel::Exact
    }
}

/// One stream's watch: a detector over its thinking and one over its text, and which of them
/// still listens.
pub(crate) struct StreamLoopGuard {
    reasoning: LoopDetector,
    text: LoopDetector,
    /// Thinking is watched until the answer's text starts: a model writing its answer is past
    /// its thinking (a model that thinks again after a tool call opens is still watched).
    reasoning_watched: bool,
    answering: bool,
    /// Text is watched until a tool call opens: what follows is arguments.
    text_watched: bool,
}

impl StreamLoopGuard {
    /// A fresh guard at `level`, or `None` when the level watches nothing.
    pub(crate) fn new(level: LoopGuardLevel) -> Option<Self> {
        let detector = match level {
            LoopGuardLevel::Off => return None,
            LoopGuardLevel::Exact => LoopDetector::exact,
            LoopGuardLevel::Semantic => LoopDetector::semantic,
        };
        Some(Self {
            reasoning: detector(),
            text: detector(),
            reasoning_watched: true,
            answering: false,
            text_watched: true,
        })
    }

    /// Feed the stream's next event: the loop it completes, in words, or `None`.
    pub(crate) fn observe(&mut self, event: &AssistantMessageEvent) -> Option<String> {
        let said = |whose: &str, hit: Option<String>| hit.map(|hit| format!("the {whose} {hit}"));
        match event {
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                if self.answering {
                    return None;
                }
                self.reasoning_watched = true;
                said("thinking", self.reasoning.push(delta))
            }
            AssistantMessageEvent::ThinkingEnd { .. } => {
                if !self.reasoning_watched {
                    return None;
                }
                said("thinking", self.reasoning.flush())
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                if !delta.is_empty() {
                    self.reasoning_watched = false;
                    self.answering = true;
                }
                if !self.text_watched {
                    return None;
                }
                said("answer", self.text.push(delta))
            }
            AssistantMessageEvent::ToolcallStart { .. }
            | AssistantMessageEvent::ToolcallDelta { .. }
            | AssistantMessageEvent::ToolcallEnd { .. } => {
                self.text_watched = false;
                None
            }
            AssistantMessageEvent::Done { .. } => {
                if self.reasoning_watched {
                    if let Some(hit) = said("thinking", self.reasoning.flush()) {
                        return Some(hit);
                    }
                }
                if self.text_watched {
                    return said("answer", self.text.flush());
                }
                None
            }
            AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::TextStart { .. }
            | AssistantMessageEvent::TextEnd { .. }
            | AssistantMessageEvent::ThinkingStart { .. }
            | AssistantMessageEvent::Error { .. } => None,
        }
    }
}

/// A guard armed on one provider stream: the watch, the request's abort token, and the
/// identity the failure is recorded under.
pub(crate) struct ArmedLoopGuard {
    guard: StreamLoopGuard,
    cancel: CancellationToken,
    provider: String,
    model: String,
    api: String,
    /// Bytes of text and thinking streamed so far (reported on a trip).
    streamed_bytes: usize,
    tripped: bool,
}

impl ArmedLoopGuard {
    pub(crate) fn new(guard: StreamLoopGuard, cancel: CancellationToken, model: &Model) -> Self {
        Self {
            guard,
            cancel,
            provider: model.provider.clone(),
            model: model.id.clone(),
            api: model.api.clone(),
            streamed_bytes: 0,
            tripped: false,
        }
    }

    /// Screen one event before it is queued: the event itself, or, when it completes a loop, the
    /// terminal `repetition_loop` error that replaces it. A trip cancels the provider request; the
    /// streamed loop is voided from the settled message so it never reaches history.
    pub(crate) fn screen(&mut self, event: AssistantMessageEvent) -> AssistantMessageEvent {
        if self.tripped {
            return event;
        }
        if let AssistantMessageEvent::TextDelta { delta, .. }
        | AssistantMessageEvent::ThinkingDelta { delta, .. } = &event
        {
            self.streamed_bytes += delta.len();
        }
        let Some(detail) = self.guard.observe(&event) else {
            return event;
        };
        self.tripped = true;
        self.cancel.cancel();
        let mut message = event.partial().clone();
        message.content.clear();
        message.stop_reason = StopReason::Error;
        let info = StreamFailureInfo {
            kind: StreamFailureKind::RepetitionLoop,
            provider_error_type: Some("repetition_loop".to_string()),
            ..StreamFailureInfo::unknown()
        };
        let detail = format!("{detail}; cut after {} streamed bytes", self.streamed_bytes);
        let failure = StreamFailureError {
            message: stream_failure_message(&info, Some(&detail)),
            info,
        };
        message.error_message = Some(failure.message.clone());
        record_stream_failure(
            (&self.provider, &self.model, &self.api),
            &mut message,
            &ProviderError::StreamFailure(failure),
        );
        AssistantMessageEvent::Error {
            reason: ErrorStopReason::Error,
            error: message,
        }
    }
}

#[cfg(test)]
mod tests;
