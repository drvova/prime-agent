//! The goal judge (`goal.create(..., verify="judge")`): one read-only model call that decides a
//! `goal.complete()` claim. The call forks the live conversation exactly as the main loop last
//! sent it (same system prompt, tools, model, thinking level, and session identity), so the
//! provider serves the whole prefix from its prompt cache and only the rubric is new input.
//!
//! The rubric and the MET / NOT MET protocol are adapted from Nodecode's goal judge
//! (nodecode-ai/cells `tools/nodecode-goal`, MIT). Unlike it, a MET verdict passes only when it
//! cites evidence the host can find in the conversation's tool results or user turns: a cited
//! `path:line` whose path appears there, or a quoted output snippet that appears there verbatim.
//! A judge that asserts without a verifiable citation counts as NOT MET.

use std::sync::{Arc, Weak};

use pa_agent::agent::Agent;
use pa_agent::agent_loop::ConvertToLlmFn;
use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions, ToolDefinition};
use pa_agent::types::{
    AgentMessage, AssistantContent, Message, ToolResultContent, UserContent, UserPart,
};

use super::model_usage::ModelUsage;
use super::rlm_usage::RlmChildUsageStore;

/// Output cap of the judge call: the verdict format needs a few hundred tokens.
pub const GOAL_JUDGE_MAX_OUTPUT_TOKENS: u64 = 1024;
/// Wall-clock cap of the judge call.
pub const GOAL_JUDGE_TIMEOUT_MS: u64 = 120_000;
/// Shortest quoted snippet accepted as evidence (shorter quotes match by accident).
const MIN_QUOTED_EVIDENCE_CHARS: usize = 8;
/// Longest gap text the judge contributes.
const MAX_JUDGE_GAP_CHARS: usize = 1500;
/// The `model_usage` source of a judge call.
const GOAL_JUDGE_USAGE_SOURCE: &str = "goal_judge";

/// The judge's decision on one completion claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeVerdict {
    /// MET with at least one citation found in the conversation.
    Met,
    /// NOT MET, an uncited MET, or a failed call: the gap for the next turn.
    NotMet { gap: String },
}

/// The forked-request judge bound to one session's agent. Holds the agent weakly: the kernel's
/// host handlers reach the judge, and a strong edge back to the agent would keep a dropped
/// session alive.
pub struct GoalJudge {
    agent: Weak<Agent>,
    /// The parent's provider function without the semantic-edge wrapper (like side questions):
    /// the judge call carries no request id and leaves the request ledger untouched.
    stream_fn: StreamFn,
    /// The main loop's message conversion, so the forked prefix serializes identically.
    convert_to_llm: ConvertToLlmFn,
    usage_store: Arc<dyn RlmChildUsageStore>,
}

impl GoalJudge {
    #[must_use]
    pub fn new(
        agent: Weak<Agent>,
        stream_fn: StreamFn,
        convert_to_llm: ConvertToLlmFn,
        usage_store: Arc<dyn RlmChildUsageStore>,
    ) -> Self {
        Self {
            agent,
            stream_fn,
            convert_to_llm,
            usage_store,
        }
    }

    /// Judge one completion claim of `objective`: exactly one capped provider call, billed to the
    /// session as a `model_usage` row (the goal budget charges it like any billed call).
    #[tracing::instrument(name = "goal_judge", skip_all)]
    pub async fn judge(&self, objective: &str) -> JudgeVerdict {
        let Some(agent) = self.agent.upgrade() else {
            return not_met("the judge could not run: the session is closed");
        };
        let state = agent.state().await;
        let mut messages = state.messages;
        // The claim arrives mid-turn from a kernel cell: the assistant turn that called
        // `goal.complete()` has no tool results yet, and providers reject a dangling tool call.
        // The loop's last request ended before that turn, so dropping it keeps the cached prefix.
        if let Some(AgentMessage::Standard(Message::Assistant(last))) = messages.last() {
            if last
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
            {
                messages.pop();
            }
        }
        let llm_messages = match (self.convert_to_llm)(messages).await {
            Ok(llm_messages) => llm_messages,
            Err(error) => return not_met(&format!("the judge could not run: {error}")),
        };
        let evidence = evidence_corpus(&llm_messages);
        let mut llm_messages = llm_messages;
        llm_messages.push(Message::User(pa_agent::types::UserMessage {
            content: UserContent::Text(judge_rubric(objective)),
            timestamp: pa_agent::now_ms(),
        }));
        let context = LlmContext {
            system_prompt: Some(state.system_prompt),
            messages: llm_messages,
            tools: state
                .tools
                .iter()
                .map(|tool| ToolDefinition {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters().clone(),
                })
                .collect(),
        };
        let options = StreamRequestOptions {
            max_tokens: Some(GOAL_JUDGE_MAX_OUTPUT_TOKENS),
            reasoning: state.thinking_level,
            session_id: agent.session_id().map(str::to_string),
            // Like side questions: never take the session's Codex socket or its continuation.
            transport: Some(pa_agent::types::Transport::Sse),
            ..StreamRequestOptions::default()
        };
        let call = async {
            let mut stream = (self.stream_fn)(state.model, context, options).await?;
            while stream.next_event().await.is_some() {}
            stream.result().await
        };
        let reply = match tokio::time::timeout(
            std::time::Duration::from_millis(GOAL_JUDGE_TIMEOUT_MS),
            call,
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) => return not_met(&format!("the judge call failed: {error}")),
            Err(_) => return not_met("the judge call timed out"),
        };
        let usage = serde_json::to_value(&reply.usage)
            .and_then(serde_json::from_value::<pa_types::ai::Usage>)
            .ok()
            .and_then(|usage| {
                ModelUsage::new(
                    GOAL_JUDGE_USAGE_SOURCE,
                    &reply.provider,
                    &reply.model,
                    usage,
                )
            });
        if let Some(usage) = usage {
            let session_id = agent.session_id().unwrap_or_default().to_string();
            if let Err(error) = self.usage_store.append_model_usage(session_id, usage).await {
                tracing::warn!("goal judge usage row not persisted: {error}");
            }
        }
        if reply.stop_reason == pa_agent::types::StopReason::Error {
            let error = reply.error_message.unwrap_or_default();
            return not_met(&format!("the judge call failed: {error}"));
        }
        let text: String = reply
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        parse_judge_reply(&text, &evidence)
    }
}

fn not_met(gap: &str) -> JudgeVerdict {
    JudgeVerdict::NotMet {
        gap: gap.to_string(),
    }
}

/// The fixed judge instruction appended after the forked conversation.
fn judge_rubric(objective: &str) -> String {
    let objective = objective
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<goal_judge>\nYou are a read-only verifier of the claim that the goal below is complete. Do not call tools; answer from the conversation above.\n<objective>\n{objective}\n</objective>\n\n1. List every requirement the objective states.\n2. For each, find concrete evidence already in this conversation: a file path with a line number (`path/to/file.rs:42`), or command output quoted verbatim in backticks.\n3. Plans, intentions, and the agent's own claims of success are not evidence.\n4. A requirement without evidence is unmet; a test or build that was not run is unmet.\n5. Decide MET only when every requirement has evidence.\n6. Reply in exactly this format, under 200 words:\nVERDICT: MET or VERDICT: NOT MET\nEVIDENCE:\n- <path:line or `verbatim output`> supports <requirement>\nGAP: <what remains; NOT MET only>\n</goal_judge>"
    )
}

/// The text a citation may point into: tool results and user turns, never the assistant's own
/// words (an agent cannot cite its own claim of success).
fn evidence_corpus(messages: &[Message]) -> String {
    let mut corpus = String::new();
    for message in messages {
        match message {
            Message::ToolResult(result) => {
                for block in &result.content {
                    match block {
                        ToolResultContent::Text(text) => {
                            corpus.push_str(&text.text);
                            corpus.push('\n');
                        }
                        ToolResultContent::Image(_) => {}
                    }
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    corpus.push_str(text);
                    corpus.push('\n');
                }
                UserContent::Parts(parts) => {
                    for part in parts {
                        match part {
                            UserPart::Text(text) => {
                                corpus.push_str(&text.text);
                                corpus.push('\n');
                            }
                            UserPart::Image(_) => {}
                        }
                    }
                }
            },
            Message::Assistant(_) => {}
        }
    }
    corpus
}

/// Parse the judge reply: MET passes only with a citation found in `corpus`.
fn parse_judge_reply(reply: &str, corpus: &str) -> JudgeVerdict {
    let verdict = reply
        .lines()
        .find_map(|line| line.trim().strip_prefix("VERDICT:"))
        .map(|verdict| verdict.trim().to_ascii_uppercase());
    let gap: String = reply
        .split_once("GAP:")
        .map(|(_, gap)| gap.trim().chars().take(MAX_JUDGE_GAP_CHARS).collect())
        .unwrap_or_default();
    match verdict.as_deref() {
        Some("MET") => {
            let evidence = reply
                .split_once("EVIDENCE:")
                .map(|(_, rest)| rest.split("GAP:").next().unwrap_or_default())
                .unwrap_or_default();
            if evidence
                .lines()
                .any(|line| citation_is_verified(line, corpus))
            {
                JudgeVerdict::Met
            } else {
                not_met(
                    "the judge answered MET without citing evidence found in the conversation (a file:line or verbatim command output); run the verifying command so its output is in context, then claim completion again",
                )
            }
        }
        Some("NOT MET") => not_met(if gap.is_empty() {
            "the judge answered NOT MET without naming the gap"
        } else {
            &gap
        }),
        Some(_) | None => not_met("the judge reply had no VERDICT line"),
    }
}

/// Whether one evidence line cites something present in `corpus`: a backticked or double-quoted
/// snippet found verbatim, or a `path:line` reference whose path appears.
fn citation_is_verified(line: &str, corpus: &str) -> bool {
    let quoted = ['`', '"'].into_iter().any(|quote| {
        line.split(quote).skip(1).step_by(2).any(|snippet| {
            let snippet = snippet.trim();
            snippet.chars().count() >= MIN_QUOTED_EVIDENCE_CHARS && corpus.contains(snippet)
        })
    });
    quoted
        || line
            .split(|ch: char| {
                ch.is_whitespace() || ch == '`' || ch == '(' || ch == ')' || ch == ','
            })
            .filter_map(|token| token.split_once(':'))
            .any(|(path, line_number)| {
                line_number.starts_with(|ch: char| ch.is_ascii_digit())
                    && (path.contains('/') || path.contains('.'))
                    && corpus.contains(path)
            })
}

#[cfg(test)]
mod tests;
