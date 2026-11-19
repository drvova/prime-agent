//! The context window's category estimate: what the next request carries, split into the system
//! prompt's static layers and dynamic tail, the tool schemas, and the message list by block kind.
//! Every figure is the chars/4 heuristic the compaction estimator uses, measured on the loop's own
//! copy of the prompt, tools, and messages after the request view's context thinning, so the
//! tool-result row and the thinning saving describe what the provider is actually sent.

use std::sync::Arc;

use pa_agent::types::{
    AgentMessage, AgentTool, AssistantContent, Message, ToolResultContent, UserContent, UserPart,
};
use pa_types::context_breakdown::{CategoryUsage, ContextCategories, ThinningSavings};

use super::context_thinning::{thin_context, ThinningSettings};
use super::messages::HARNESS_DIGEST_PREFIX;

/// Image blocks count like the compaction estimate (4,800 chars, ~1,200 tokens).
const IMAGE_CHARS: u64 = 4_800;

/// The loop state one estimate reads.
pub struct ContextSnapshot<'a> {
    pub system_prompt: &'a str,
    /// Byte length of the prompt's cache-stable static prefix.
    pub static_prefix_len: usize,
    pub tools: &'a [Arc<dyn AgentTool>],
    pub messages: Vec<AgentMessage>,
    pub context_window: u64,
    pub thinning: ThinningSettings,
}

/// Running chars for one category.
#[derive(Default)]
struct Tally {
    chars: u64,
    items: u64,
}

impl Tally {
    fn add(&mut self, chars: u64) {
        self.chars += chars;
        self.items += 1;
    }

    fn usage(&self) -> CategoryUsage {
        CategoryUsage {
            tokens: self.chars.div_ceil(4),
            items: self.items,
        }
    }
}

fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

/// Estimate the next request's categories from one loop snapshot.
#[must_use]
pub fn estimate_context_categories(snapshot: ContextSnapshot<'_>) -> ContextCategories {
    let prompt = snapshot.system_prompt;
    // The static prefix is the prompt's first bytes; a prompt rebuilt shorter than the recorded
    // prefix (never in practice) counts whole as static.
    let split = (0..=snapshot.static_prefix_len.min(prompt.len()))
        .rev()
        .find(|at| prompt.is_char_boundary(*at))
        .unwrap_or_default();
    let (static_part, dynamic_part) = prompt.split_at(split);
    let mut system_static = Tally::default();
    let mut system_dynamic = Tally::default();
    if !static_part.is_empty() {
        system_static.add(chars(static_part));
    }
    if !dynamic_part.is_empty() {
        system_dynamic.add(chars(dynamic_part));
    }
    let mut tools = Tally::default();
    for tool in snapshot.tools {
        tools.add(
            chars(tool.name()) + chars(tool.description()) + chars(&tool.parameters().to_string()),
        );
    }

    let view = thin_context(
        snapshot.messages,
        snapshot.context_window,
        &snapshot.thinning,
    );
    let mut messages = Tally::default();
    let mut tool_calls = Tally::default();
    let mut tool_results = Tally::default();
    let mut images = Tally::default();
    let mut thinking = Tally::default();
    let mut harness_digest = Tally::default();
    for message in &view.messages {
        match message {
            AgentMessage::Standard(Message::User(user)) => match &user.content {
                UserContent::Text(text) if text.starts_with(HARNESS_DIGEST_PREFIX) => {
                    harness_digest.add(chars(text));
                }
                UserContent::Text(text) => messages.add(chars(text)),
                UserContent::Parts(parts) => {
                    let digest = parts.iter().any(|part| match part {
                        UserPart::Text(text) => text.text.starts_with(HARNESS_DIGEST_PREFIX),
                        UserPart::Image(_) => false,
                    });
                    let mut text_chars = 0;
                    for part in parts {
                        match part {
                            UserPart::Text(text) => text_chars += chars(&text.text),
                            UserPart::Image(_) => images.add(IMAGE_CHARS),
                        }
                    }
                    if digest {
                        harness_digest.add(text_chars);
                    } else {
                        messages.add(text_chars);
                    }
                }
            },
            AgentMessage::Standard(Message::Assistant(assistant)) => {
                let mut text_chars = None;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            *text_chars.get_or_insert(0) += chars(&text.text);
                        }
                        AssistantContent::Thinking(block) => thinking.add(chars(&block.thinking)),
                        AssistantContent::ToolCall(call) => {
                            tool_calls.add(chars(&call.name) + chars(&call.arguments.to_string()));
                        }
                    }
                }
                if let Some(text_chars) = text_chars {
                    messages.add(text_chars);
                }
            }
            AgentMessage::Standard(Message::ToolResult(result)) => {
                let mut text_chars = 0;
                for block in &result.content {
                    match block {
                        ToolResultContent::Text(text) => text_chars += chars(&text.text),
                        ToolResultContent::Image(_) => images.add(IMAGE_CHARS),
                    }
                }
                tool_results.add(text_chars);
            }
            AgentMessage::Custom(custom) => messages.add(chars(&custom.payload.to_string())),
        }
    }
    ContextCategories {
        system_static: system_static.usage(),
        system_dynamic: system_dynamic.usage(),
        tools: tools.usage(),
        messages: messages.usage(),
        tool_calls: tool_calls.usage(),
        tool_results: tool_results.usage(),
        images: images.usage(),
        thinking: thinking.usage(),
        harness_digest: harness_digest.usage(),
        thinning: snapshot.thinning.enabled.then_some(ThinningSavings {
            thinned_results: view.stats.thinned_results,
            tokens_saved: view.stats.tokens_saved(),
        }),
    }
}

impl super::engine::SessionEngine {
    /// The next request's category estimate from the loop's current state. Pure CPU over an
    /// in-memory snapshot: no disk, no network.
    pub async fn context_categories(&self) -> ContextCategories {
        let state = self.session.agent().state().await;
        estimate_context_categories(ContextSnapshot {
            system_prompt: &state.system_prompt,
            static_prefix_len: self.system_prompt_static_len,
            tools: &state.tools,
            context_window: state.model.context_window,
            messages: state.messages,
            thinning: self.context_thinning,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::types::{
        AssistantMessage, TextContent, ThinkingContent, ToolCall, ToolResultMessage, UserMessage,
    };

    fn text(value: &str) -> TextContent {
        TextContent {
            text: value.to_string(),
            text_signature: None,
        }
    }

    fn assistant(content: Vec<AssistantContent>) -> AgentMessage {
        let mut message: AssistantMessage = serde_json::from_value(serde_json::json!({
            "content": [], "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "total": 0}}, "stopReason": "stop", "timestamp": 0,
        }))
        .expect("an empty assistant message");
        message.content = content;
        AgentMessage::Standard(Message::Assistant(message))
    }

    fn user(content: UserContent) -> AgentMessage {
        AgentMessage::Standard(Message::User(UserMessage {
            content,
            timestamp: 0,
        }))
    }

    /// Each block lands in its own category: the prompt splits at the static prefix, the digest
    /// message is its own row, images count per block, and thinning off reports no saving.
    #[test]
    fn every_block_kind_lands_in_its_category() {
        let image = pa_agent::types::ImageContent {
            data: String::new(),
            mime_type: "image/png".to_string(),
        };
        let digest = format!("{HARNESS_DIGEST_PREFIX}{}", "d".repeat(36));
        let messages = vec![
            user(UserContent::Text(digest.clone())),
            user(UserContent::Parts(vec![
                UserPart::Text(text("look at this")),
                UserPart::Image(image.clone()),
            ])),
            assistant(vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "t".repeat(40),
                    thinking_signature: None,
                    redacted: None,
                }),
                AssistantContent::Text(text("sure")),
                AssistantContent::ToolCall(ToolCall {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                    thought_signature: None,
                }),
            ]),
            AgentMessage::Standard(Message::ToolResult(ToolResultMessage {
                tool_call_id: "c1".to_string(),
                tool_name: "bash".to_string(),
                content: vec![
                    ToolResultContent::Text(text(&"r".repeat(80))),
                    ToolResultContent::Image(image),
                ],
                details: None,
                is_error: false,
                timestamp: 0,
            })),
        ];
        let categories = estimate_context_categories(ContextSnapshot {
            system_prompt: "static layers\ndynamic tail",
            static_prefix_len: "static layers\n".len(),
            tools: &[],
            messages,
            context_window: 200_000,
            thinning: ThinningSettings {
                enabled: false,
                ..ThinningSettings::default()
            },
        });
        let usage = |tokens, items| CategoryUsage { tokens, items };
        assert_eq!(
            categories,
            ContextCategories {
                system_static: usage(4, 1),
                system_dynamic: usage(3, 1),
                tools: usage(0, 0),
                // "look at this" (12) and "sure" (4) as two messages.
                messages: usage(4, 2),
                // "bash" plus `{"cmd":"ls"}`.
                tool_calls: usage(4, 1),
                tool_results: usage(20, 1),
                images: usage(2_400, 2),
                thinking: usage(10, 1),
                harness_digest: usage((chars(&digest)).div_ceil(4), 1),
                thinning: None,
            }
        );
    }
}
