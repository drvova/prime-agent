//! API provider registry: providers register stream functions per `api` identifier;
//! `stream()`/`complete()` resolve the provider for a model and forward.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::event_stream::AssistantMessageEventStream;
use crate::types::{Context, LoopGuardLevel, Model, SimpleStreamOptions, StreamOptions};

/// A provider implementation for one API.
pub trait Provider: Send + Sync {
    /// The `api` identifier this provider serves, e.g. "openai-completions".
    fn api(&self) -> &str;

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;

    /// The stream loop guard's level for `model` when a request names none
    /// (`StreamOptions::loop_guard`). The default follows the model family: off for Claude,
    /// semantic for the Gemini, `DeepSeek` and Grok families, exact for every other model. A
    /// provider that replays scripted fixtures rather than model output returns `Off`, so its
    /// fixtures stream as written.
    fn default_loop_guard(&self, model: &Model) -> LoopGuardLevel {
        crate::loop_guard::default_level(&model.id)
    }

    /// Whether this provider's requests wait for a provider permit
    /// ([`crate::admission`]). Every network provider is paced; a provider
    /// that replays scripted fixtures returns `false`, since it has no limits to learn.
    fn paced(&self) -> bool {
        true
    }
}

struct RegisteredProvider {
    provider: Arc<dyn Provider>,
    source_id: Option<String>,
}

fn registry() -> &'static RwLock<HashMap<String, RegisteredProvider>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, RegisteredProvider>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register (or replace) the provider for `provider.api()`.
pub fn register_api_provider(provider: Arc<dyn Provider>, source_id: Option<&str>) {
    let mut registry = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.insert(
        provider.api().to_string(),
        RegisteredProvider {
            provider,
            source_id: source_id.map(std::string::ToString::to_string),
        },
    );
}

/// Look up the provider for an api identifier.
#[must_use]
pub fn get_api_provider(api: &str) -> Option<Arc<dyn Provider>> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(api)
        .map(|entry| entry.provider.clone())
}

/// All registered providers.
#[must_use]
pub fn get_api_providers() -> Vec<Arc<dyn Provider>> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .map(|entry| entry.provider.clone())
        .collect()
}

/// Unregister providers installed with the given source id (extension teardown).
pub fn unregister_api_providers(source_id: &str) {
    let mut registry = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|_, entry| entry.source_id.as_deref() != Some(source_id));
}

/// Remove all registered providers.
pub fn clear_api_providers() {
    registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Register the built-in providers (the explicit Rust equivalent of the TS side-effect import).
pub fn register_builtin_api_providers() {
    let mut registry = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Each entry mirrors the corresponding import in `providers/register-builtins.ts`.
    let builtins: Vec<Arc<dyn Provider>> = vec![
        Arc::new(crate::providers::anthropic::AnthropicMessagesProvider),
        Arc::new(crate::providers::openai_completions::OpenAICompletionsProvider),
        Arc::new(crate::providers::openai_responses::OpenAIResponsesProvider),
        Arc::new(crate::providers::azure_openai_responses::AzureOpenAIResponsesProvider),
        Arc::new(crate::providers::google::GoogleGenerativeAiProvider),
        Arc::new(crate::providers::google_vertex::GoogleVertexProvider),
        Arc::new(crate::providers::mistral::MistralConversationsProvider),
        Arc::new(crate::providers::bedrock::BedrockConverseStreamProvider),
        Arc::new(crate::providers::openai_codex_responses::OpenAICodexResponsesProvider),
    ];
    for provider in builtins {
        registry.insert(
            provider.api().to_string(),
            RegisteredProvider {
                provider,
                source_id: None,
            },
        );
    }
}

/// Clear then re-register built-ins (test helper parity with `resetApiProviders`).
pub fn reset_api_providers() {
    clear_api_providers();
    register_builtin_api_providers();
}

/// Ensure built-ins are present (idempotent; matches the TS import side effect).
pub fn ensure_builtins() {
    static ENSURED: OnceLock<()> = OnceLock::new();
    ENSURED.get_or_init(register_builtin_api_providers);
}

/// Registry facade over the process-wide provider map (the free functions below remain for parity
/// with the TS module surface).
pub struct ProviderRegistry;

impl ProviderRegistry {
    /// Register (or replace) the provider for `provider.api()`.
    pub fn register(&self, provider: Arc<dyn Provider>, source_id: Option<&str>) {
        register_api_provider(provider, source_id);
    }

    /// Look up the provider for an api identifier.
    #[must_use]
    pub fn get(&self, api: &str) -> Option<Arc<dyn Provider>> {
        get_api_provider(api)
    }

    /// All registered providers.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<dyn Provider>> {
        get_api_providers()
    }

    /// Unregister providers installed with the given source id.
    pub fn unregister(&self, source_id: &str) {
        unregister_api_providers(source_id);
    }

    /// Remove all registered providers.
    pub fn clear(&self) {
        clear_api_providers();
    }

    /// Clear then re-register built-ins.
    pub fn reset(&self) {
        reset_api_providers();
    }
}

#[cfg(test)]
mod tests {
    /// A thread that panics while it holds the process's provider registry
    /// (one session's fault) leaves the registry usable for every other.
    #[test]
    fn a_panic_under_the_registry_lock_leaves_it_usable() {
        let panicked = std::thread::spawn(|| {
            let _held = super::registry()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            panic!("a session panics while it holds the registry");
        })
        .join();
        assert_eq!(
            (
                panicked.is_err(),
                super::registry().is_poisoned(),
                super::get_api_provider("no-provider-has-this-api").is_none(),
            ),
            (true, true, true)
        );
    }
}
