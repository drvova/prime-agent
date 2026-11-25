//! Anthropic Messages request params assembly.

use serde_json::{json, Map, Value};

use crate::providers::anthropic::convert::{convert_messages, convert_tools};
use crate::providers::anthropic::{
    get_anthropic_compat, is_always_on_adaptive_thinking_model, supports_adaptive_thinking,
    AnthropicOptions, AnthropicThinkingDisplay, CacheControl,
};
use crate::types::{Context, Model};
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;

#[expect(
    clippy::too_many_lines,
    reason = "one function builds the request parameters, mirroring the TS reference"
)]
pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: Option<&AnthropicOptions>,
    cache_control: Option<&CacheControl>,
) -> Value {
    let base = options
        .map(|options| options.base.clone())
        .unwrap_or_default();
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert(
        "messages".into(),
        json!(convert_messages(
            context,
            model,
            is_oauth_token,
            cache_control
        )),
    );
    params.insert(
        "max_tokens".into(),
        json!(base.max_tokens.unwrap_or(model.max_tokens / 3)),
    );
    params.insert("stream".into(), json!(true));

    // Cache breakpoints (Anthropic allows four per request; the render order is tools -> system
    // -> messages, and a marker caches everything before it). The system prompt carries one
    // marker on its cache-stable static layers and one on its session tail, the last user message
    // carries the conversation marker, so at most three are placed. Tools render before system,
    // so the system markers already cover them: the last tool takes a marker only when no system
    // block follows it. For OAuth tokens the Claude Code identity block leads the system list; a
    // later system marker covers it too.
    let mut system: Vec<Value> = Vec::new();
    if is_oauth_token {
        system.push(json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        }));
    }
    if let Some(system_prompt) = &context.system_prompt {
        // With a cache-stable prefix length inside the prompt (on a char boundary), the static
        // layers go out as their own block, then the session-specific tail, so a new session that
        // shares the static layers reads them from the cache instead of writing them again. Only
        // the block boundary moves: the blocks concatenate to the prompt byte for byte.
        let split = base.cached_system_prefix_len.filter(|&len| {
            len > 0 && len < system_prompt.len() && system_prompt.is_char_boundary(len)
        });
        let segments: Vec<&str> = match split {
            Some(len) => {
                let (static_prefix, tail) = system_prompt.split_at(len);
                vec![static_prefix, tail]
            }
            None => vec![system_prompt.as_str()],
        };
        for segment in segments {
            let mut entry = json!({ "type": "text", "text": sanitize_surrogates(segment) });
            if let Some(cache_control) = cache_control {
                entry
                    .as_object_mut()
                    .expect("system entry is an object")
                    .insert("cache_control".into(), cache_control.to_json());
            }
            system.push(entry);
        }
    } else if let (Some(cache_control), Some(identity)) = (cache_control, system.first_mut()) {
        identity
            .as_object_mut()
            .expect("system entry is an object")
            .insert("cache_control".into(), cache_control.to_json());
    }
    let tools_cache_control = if system.is_empty() {
        cache_control
    } else {
        None
    };
    if !system.is_empty() {
        params.insert("system".into(), json!(system));
    }

    // Temperature is incompatible with extended thinking (adaptive or budget-based), and always-on
    // models reject sampling params outright.
    if let Some(temperature) = base.temperature {
        if options.map(|options| options.thinking_enabled) != Some(Some(true))
            && !is_always_on_adaptive_thinking_model(&model.id)
        {
            params.insert("temperature".into(), json!(temperature));
        }
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            params.insert(
                "tools".into(),
                json!(convert_tools(
                    tools,
                    is_oauth_token,
                    get_anthropic_compat(model).supports_eager_tool_input_streaming,
                    tools_cache_control,
                )),
            );
        }
    }

    // Configure thinking mode: adaptive, budget-based, or explicitly disabled.
    if model.reasoning {
        if options.map(|options| options.thinking_enabled) == Some(Some(true)) {
            let display = options
                .and_then(|options| options.thinking_display)
                .unwrap_or(AnthropicThinkingDisplay::Summarized);
            if supports_adaptive_thinking(&model.id) {
                params.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": display.as_str() }),
                );
                if let Some(effort) = options.and_then(|options| options.effort) {
                    params.insert("output_config".into(), json!({ "effort": effort.as_str() }));
                }
            } else {
                params.insert(
                    "thinking".into(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": options.and_then(|options| options.thinking_budget_tokens).unwrap_or(1024),
                        "display": display.as_str(),
                    }),
                );
            }
        } else if options.map(|options| options.thinking_enabled) == Some(Some(false))
            && !is_always_on_adaptive_thinking_model(&model.id)
        {
            params.insert("thinking".into(), json!({ "type": "disabled" }));
        }
    }

    if let Some(metadata) = &base.metadata {
        if let Some(user_id) = metadata.get("user_id").and_then(|value| value.as_str()) {
            params.insert("metadata".into(), json!({ "user_id": user_id }));
        }
    }

    if let Some(tool_choice) = options.and_then(|options| options.tool_choice.as_ref()) {
        params.insert("tool_choice".into(), tool_choice.to_json());
    }

    Value::Object(params)
}

#[cfg(test)]
mod tests {
    use super::build_params;
    use crate::providers::anthropic::{AnthropicEffort, AnthropicOptions};
    use crate::types::{
        zero_model_cost, Context, Message, Model, ModelInput, UserMessage, UserMessageContent,
    };
    use serde_json::{json, Map};

    fn thinking_params(model_id: &str) -> (serde_json::Value, serde_json::Value) {
        let model = Model {
            id: model_id.into(),
            name: model_id.into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: zero_model_cost(),
            context_window: 200_000,
            max_tokens: 64_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
            serving: None,
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("Hi".into()),
                timestamp: 1,
                rest: Map::default(),
            })],
            tools: None,
        };
        let options = AnthropicOptions {
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(2048),
            effort: Some(AnthropicEffort::Medium),
            ..Default::default()
        };
        let params = build_params(&model, &context, false, Some(&options), None);
        (params["thinking"].clone(), params["output_config"].clone())
    }

    /// Discussion #3710: Haiku 5.x rejects `thinking.type: "enabled"` with a 400 and needs
    /// adaptive thinking with an effort, in both the dashed and dotted id spellings.
    #[test]
    fn haiku_5_requests_adaptive_thinking() {
        for model_id in ["claude-haiku-5-5", "claude-haiku-5.5"] {
            assert_eq!(
                thinking_params(model_id),
                (
                    json!({ "type": "adaptive", "display": "summarized" }),
                    json!({ "effort": "medium" }),
                ),
                "{model_id}"
            );
        }
    }

    /// Haiku 4.5 still takes budget-based thinking.
    #[test]
    fn haiku_4_5_keeps_budget_thinking() {
        assert_eq!(
            thinking_params("claude-haiku-4-5"),
            (
                json!({ "type": "enabled", "budget_tokens": 2048, "display": "summarized" }),
                serde_json::Value::Null,
            )
        );
    }
}

#[cfg(test)]
mod cache_breakpoint_tests {
    use super::build_params;
    use crate::providers::anthropic::{get_cache_control, AnthropicOptions};
    use crate::types::{
        zero_model_cost, CacheRetention, Context, Model, ModelInput, StreamOptions,
    };
    use serde_json::{json, Value};

    const STATIC_LAYERS: &str = "# Core harness\n\nUsage rules\n\nGuidelines";
    const SESSION_TAIL: &str = "\n\nPre-installed Python packages: numpy.\n\ncwd: /tmp/é";

    fn claude() -> Model {
        Model {
            id: "claude-sonnet-4-5".into(),
            name: "Claude Sonnet 4.5".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: zero_model_cost(),
            context_window: 200_000,
            max_tokens: 64_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
            serving: None,
        }
    }

    fn context(last_user_content: &Value) -> Context {
        serde_json::from_value(json!({
            "systemPrompt": format!("{STATIC_LAYERS}{SESSION_TAIL}"),
            "messages": [
                { "role": "user", "content": "first", "timestamp": 1 },
                { "role": "user", "content": last_user_content, "timestamp": 2 },
            ],
            "tools": [
                { "name": "bash", "description": "Run a command", "parameters": { "type": "object" } },
                { "name": "edit", "description": "Edit a file", "parameters": { "type": "object" } },
            ],
        }))
        .expect("context fixture parses")
    }

    fn params(context: &Context, oauth: bool, thinking: bool, prefix: Option<usize>) -> Value {
        let model = claude();
        let (_, cache_control) = get_cache_control(&model, Some(CacheRetention::Short));
        let options = AnthropicOptions {
            base: StreamOptions {
                cached_system_prefix_len: prefix,
                ..Default::default()
            },
            thinking_enabled: Some(thinking),
            thinking_budget_tokens: thinking.then_some(2048),
            ..Default::default()
        };
        build_params(
            &model,
            context,
            oauth,
            Some(&options),
            cache_control.as_ref(),
        )
    }

    fn count_markers(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count_markers).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_markers).sum(),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 0,
        }
    }

    /// The static layers and the session tail go out as two system blocks, each with its own
    /// marker, and the text they carry concatenates to the prompt byte for byte.
    #[test]
    fn the_static_prefix_is_its_own_cached_system_block() {
        let context = context(&json!("go"));
        let params = params(&context, false, false, Some(STATIC_LAYERS.len()));
        let marker = json!({ "type": "ephemeral" });
        assert_eq!(
            params["system"],
            json!([
                { "type": "text", "text": STATIC_LAYERS, "cache_control": marker },
                { "type": "text", "text": SESSION_TAIL, "cache_control": marker },
            ])
        );
        let joined: String = params["system"]
            .as_array()
            .expect("system is a list")
            .iter()
            .map(|block| block["text"].as_str().expect("text block"))
            .collect();
        assert_eq!(Some(joined.as_str()), context.system_prompt.as_deref());
    }

    /// A boundary that is not a char boundary inside the prompt keeps the single block.
    #[test]
    fn an_unusable_boundary_keeps_one_system_block() {
        let context = context(&json!("go"));
        let prompt = context.system_prompt.clone().expect("prompt");
        let inside_e_acute = prompt.len() - 1;
        for prefix in [None, Some(0), Some(prompt.len()), Some(inside_e_acute)] {
            let params = params(&context, false, false, prefix);
            assert_eq!(
                params["system"],
                json!([{ "type": "text", "text": prompt, "cache_control": { "type": "ephemeral" } }]),
                "{prefix:?}"
            );
        }
    }

    /// Anthropic rejects a request with more than four `cache_control` markers. Every request
    /// shape (tools, OAuth identity, budget thinking, image and text tails, with and without the
    /// static split) stays within the limit, and the tools are covered by the system markers
    /// instead of carrying their own.
    #[test]
    fn every_request_shape_stays_within_four_breakpoints() {
        let tails = [
            json!("go"),
            json!([{ "type": "image", "data": "aGk=", "mimeType": "image/png" }]),
            json!([
                { "type": "text", "text": "look" },
                { "type": "image", "data": "aGk=", "mimeType": "image/png" },
            ]),
        ];
        for tail in tails {
            let context = context(&tail);
            for oauth in [false, true] {
                for thinking in [false, true] {
                    for prefix in [None, Some(STATIC_LAYERS.len())] {
                        let params = params(&context, oauth, thinking, prefix);
                        let expected = if prefix.is_some() { 3 } else { 2 };
                        assert_eq!(
                            (count_markers(&params), count_markers(&params["tools"])),
                            (expected, 0),
                            "tail={tail} oauth={oauth} thinking={thinking} prefix={prefix:?}"
                        );
                    }
                }
            }
        }
        // With no system prompt the last tool keeps its marker (nothing after it covers the
        // tools); an OAuth request's identity block takes it instead.
        let mut bare = context(&json!("go"));
        bare.system_prompt = None;
        assert_eq!(
            count_markers(&params(&bare, false, false, None)["tools"]),
            1
        );
        let oauth = params(&bare, true, false, None);
        assert_eq!(
            (
                count_markers(&oauth["system"]),
                count_markers(&oauth["tools"])
            ),
            (1, 0)
        );
    }
}
