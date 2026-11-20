//! The worker half of the TUI's `/better`: the run rides the side-question lane (one run per
//! client, abortable, streamed as `side_question_event` frames) and pa-core's `BetterHost` does
//! the work. Running events carry what the helper is reading; the final answer is the rewrite.

use pa_core::session_engine::better::{BetterMode, BetterOutcome, BetterProgress, BetterRequest};
use pa_core::session_engine::side_question::SideQuestionSink;

use super::AgentSessionEngine;
use crate::engine::SideQuestionOutcome;

impl AgentSessionEngine {
    /// One `/better` rewrite of `prompt` on the session's own model.
    pub(crate) fn run_better(
        &self,
        prompt: &str,
        signal: &pa_agent::abort::AbortSignal,
        sink: &SideQuestionSink,
    ) -> SideQuestionOutcome {
        // The run bills the engine it started on, like a side question.
        let Some(built) = self.session.blocking_lock().clone() else {
            return SideQuestionOutcome::Failed {
                answer: String::new(),
                error: "Select a model before using /better".to_string(),
            };
        };
        let progress: BetterProgress = {
            let sink = std::sync::Arc::clone(sink);
            std::sync::Arc::new(move |label: &str| {
                let _ = sink(label);
            })
        };
        let request = BetterRequest {
            task: prompt.to_string(),
            mode: BetterMode::Deepen,
            cwd: None,
        };
        let outcome = self
            .runtime
            .block_on(built.better.run(&request, signal, &progress));
        match outcome {
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
        }
    }
}
