//! `ModelRegistry`: composes built-in, custom (models.json), and Prime Inference
//! catalogs; resolves request auth per provider/model.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pa_types::ai::{
    CompatKind, Model, ModelCompat, ModelThinkingLevel, OpenAiResponsesCompat, ThinkingLevelMap,
};

use crate::auth::descriptor::{AuthStrategy, ProviderAuthDescriptor};
use crate::auth::descriptor_oauth::DescriptorOAuth;
use crate::auth::manager::AuthStorage;
use crate::auth::resolver::{AuthCounter, AuthResolveContext, ProviderAuthResolver, StaticKey};
use crate::auth::token_exchange::TokenExchange;
use crate::auth::types::{AuthCredential, AuthSource, PRIME_INFERENCE_PROVIDER_ID};

use super::catalog_chain;
use super::custom::{apply_model_override, load_custom_models, merge_compat, CustomModelsResult};

/// Request-auth bits a provider can configure in models.json.
#[derive(Debug, Clone, Default)]
pub struct ProviderRequestConfig {
    pub api_key: Option<String>,
    /// Ordered (`BTreeMap`): merged into request-header maps that
    /// providers iterate deterministically.
    pub headers: Option<BTreeMap<String, String>>,
    pub auth_header: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRequestAuth {
    pub ok: bool,
    pub api_key: Option<String>,
    /// Ordered (`BTreeMap`): unordered iteration would order the
    /// composed request headers randomly.
    pub headers: Option<BTreeMap<String, String>>,
    pub error: Option<String>,
    /// The per-call request model (TS `requestModel`): for xAI, the model
    /// shaped for the credential the lookup serves — the stored
    /// subscription's openai-responses shape or the configured API-key
    /// shape. `None` keeps the caller's model.
    pub request_model: Option<Model>,
}

/// Why a `set_model` selection failed to resolve: the provider is not
/// signed in (the client offers the sign-in flow and retries) or the model
/// is not in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetModelSelectionError {
    /// The model exists, but its provider has no credential and none is
    /// stale: sign in, then retry the switch.
    ProviderUnauthenticated { provider: String },
    /// No such model in the catalog.
    NotFound { provider: String, model_id: String },
}

impl SetModelSelectionError {
    /// The refused provider of the sign-in variant; `None` on not-found.
    #[must_use]
    pub fn unauthenticated_provider(&self) -> Option<&str> {
        match self {
            SetModelSelectionError::ProviderUnauthenticated { provider } => Some(provider),
            SetModelSelectionError::NotFound { .. } => None,
        }
    }
}

impl std::fmt::Display for SetModelSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetModelSelectionError::ProviderUnauthenticated { provider } => write!(
                f,
                "Provider \"{provider}\" is not signed in. Sign in to the provider (the TUI's /login command), then set the model again."
            ),
            SetModelSelectionError::NotFound { provider, model_id } => {
                write!(f, "Model not found: {provider}/{model_id}")
            }
        }
    }
}

impl std::error::Error for SetModelSelectionError {}

/// Composed model catalog with auth-aware availability.
pub struct ModelRegistry {
    pub auth: AuthStorage,
    models_json_path: Option<PathBuf>,
    models: Vec<Model>,
    load_error: Option<String>,
    provider_request_configs: HashMap<String, ProviderRequestConfig>,
    /// The models.json provider `auth` descriptors (validated at load).
    provider_auth: HashMap<String, ProviderAuthDescriptor>,
    model_request_headers: HashMap<String, BTreeMap<String, String>>,
    /// TS `xaiModelSources`: every xAI model id's configured
    /// (pre-adaptation) model, re-recorded at every load. The per-call
    /// request model in [`Self::get_api_key_and_headers`] re-resolves
    /// against this map, so it follows the credential that serves the
    /// call, not the credential the load saw.
    xai_model_sources: HashMap<String, Model>,
    /// The process-shared live catalog chain (`catalog_chain::catalog_for`);
    /// `resolve()` sources built-ins from it.
    catalog: std::sync::Arc<pa_models::ModelCatalog>,
    /// `provider/id` of every models.json model at the last load: their declared API never
    /// moves, so background discovery never checks their route.
    custom_model_keys: std::collections::HashSet<(String, String)>,
}

/// Under a stored xAI subscription the provider's models switch onto the
/// openai-responses API with the subscription thinking maps and
/// `supportsLongCacheRetention: false` compat.
fn xai_subscription_model(model: &Model) -> Model {
    let mut adapted = model.clone();
    adapted.api = "openai-responses".to_string();
    adapted.base_url = "https://api.x.ai/v1".to_string();
    // TS fills only an absent map: a model that already declares its
    // levels keeps them.
    if adapted.thinking_level_map.is_none() {
        adapted.thinking_level_map = Some(xai_subscription_thinking_map(&model.id));
    }
    adapted.compat = Some(ModelCompat::from_kind(CompatKind::OpenAiResponses(
        OpenAiResponsesCompat {
            send_session_id_header: None,
            supports_long_cache_retention: Some(false),
        },
    )));
    adapted
}

/// The TS flow's explicit thinking maps for the grok models it names,
/// and the all-levels-null default for every other xAI model (reasoning
/// stays; unverified effort controls never send).
fn xai_subscription_thinking_map(model_id: &str) -> ThinkingLevelMap {
    // The value mirrors the TS table: `None` for the null mapping
    // (unsupported), the wire string for a named level; an absent level
    // keeps its default support.
    let null = || None;
    let value = |text: &str| Some(text.to_string());
    let mut map = ThinkingLevelMap::new();
    match model_id {
        "grok-4.3" => {
            map.insert(ModelThinkingLevel::Off, value("none"));
            map.insert(ModelThinkingLevel::Minimal, null());
        }
        "grok-4.5" => {
            map.insert(ModelThinkingLevel::Off, null());
            map.insert(ModelThinkingLevel::Minimal, null());
        }
        "grok-4.6" | "grok-4.7" => {
            map.insert(ModelThinkingLevel::Off, null());
            map.insert(ModelThinkingLevel::Minimal, null());
            map.insert(ModelThinkingLevel::Xhigh, value("xhigh"));
        }
        _ => {
            for level in [
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
                ModelThinkingLevel::Max,
            ] {
                map.insert(level, null());
            }
        }
    }
    map
}

impl ModelRegistry {
    pub fn create(auth: AuthStorage, models_json_path: impl Into<PathBuf>) -> Self {
        let models_json_path = models_json_path.into();
        // Learned endpoint capabilities live beside models.json in the agent dir; the built-in
        // models' wire upgrade below reads them.
        if let Some(agent_dir) = models_json_path.parent() {
            pa_ai::responses_transport::set_capability_cache_path(
                agent_dir.join("responses-transport.json"),
            );
        }
        Self::new(auth, Some(models_json_path))
    }

    pub fn in_memory(auth: AuthStorage) -> Self {
        Self::new(auth, None)
    }

    fn new(auth: AuthStorage, models_json_path: Option<PathBuf>) -> Self {
        let catalog = catalog_chain::catalog_for(models_json_path.as_deref());
        let mut registry = Self {
            auth,
            models_json_path,
            models: Vec::new(),
            load_error: None,
            provider_request_configs: HashMap::new(),
            provider_auth: HashMap::new(),
            model_request_headers: HashMap::new(),
            xai_model_sources: HashMap::new(),
            catalog,
            custom_model_keys: std::collections::HashSet::new(),
        };
        registry.load_models();
        registry
    }

    /// The Prime Inference credentials for the credentialed catalog layer
    /// (Bearer + team headers, as the TS refresh sends).
    fn prime_credentials(&mut self) -> Option<pa_models::PrimeCredentials> {
        let api_key = self.auth.get_api_key(PRIME_INFERENCE_PROVIDER_ID)?;
        let team_id = self
            .auth
            .get_provider_headers(PRIME_INFERENCE_PROVIDER_ID)
            .and_then(|headers| headers.get("X-Prime-Team-ID").cloned());
        Some(pa_models::PrimeCredentials { api_key, team_id })
    }

    pub fn get_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Built-in + custom models (auth not filtered).
    pub fn get_all(&self) -> &[Model] {
        &self.models
    }

    /// Models whose provider has configured auth. Private Prime Inference
    /// models need no extra gate: only the account's scoped snapshot and
    /// models.json list them. The per-model auth probe is answered once per
    /// provider: each probe rebuilds the auth-source candidates.
    pub fn get_available(&self) -> Vec<&Model> {
        let mut auth_by_provider: HashMap<&str, bool> = HashMap::new();
        self.models
            .iter()
            .filter(|model| {
                *auth_by_provider
                    .entry(model.provider.as_str())
                    .or_insert_with(|| self.has_configured_auth(model))
            })
            .collect()
    }

    /// Models `rlm.find_models` may search: auth-configured, and not on a
    /// stale or expired provider credential (probe answered once per provider).
    pub fn get_rlm_searchable_models(&self) -> Vec<&Model> {
        let mut status_by_provider: HashMap<&str, bool> = HashMap::new();
        self.get_available()
            .into_iter()
            .filter(|model| {
                *status_by_provider
                    .entry(model.provider.as_str())
                    .or_insert_with(|| {
                        let status = self.auth.get_auth_status(&model.provider);
                        status.source != Some(crate::auth::types::AuthSource::Stale)
                            && status.label.as_deref() != Some("expired")
                    })
            })
            .collect()
    }

    pub fn has_configured_auth(&self, model: &Model) -> bool {
        self.auth.has_auth(&model.provider)
            || self.has_configured_provider_request_auth(&model.provider)
    }

    /// The provider's auth status without credential values: callers
    /// classifying unauthenticated vs stale providers read this instead of
    /// probing for keys.
    pub fn get_provider_auth_status(&self, provider: &str) -> crate::auth::types::AuthStatus {
        self.auth.get_auth_status(provider)
    }

    /// `set_model`'s model resolution: an available model resolves directly; a
    /// model whose provider has no credential (none stale) is the typed sign-in
    /// refusal; stale-auth providers keep the switch (the lookup never mutates
    /// stale state).
    ///
    /// # Errors
    ///
    /// `ProviderUnauthenticated` when the provider has no credential;
    /// `NotFound` when no catalog model matches.
    pub fn resolve_set_model_selection(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<&Model, SetModelSelectionError> {
        let matches = |model: &Model| model.provider == provider && model.id == model_id;
        if let Some(model) = self
            .get_available()
            .into_iter()
            .find(|model| matches(model))
        {
            return Ok(model);
        }
        // A model can exist while its provider is not signed in — the
        // picker's discovery path. No credential at all is the sign-in refusal;
        // the unauthorized private Prime Inference model keeps the not-found
        // refusal — never a switch to an unentitled model.
        let Some(model) = self.get_all().iter().find(|model| matches(model)) else {
            return Err(SetModelSelectionError::NotFound {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            });
        };
        if self.get_provider_auth_status(provider).source
            == Some(crate::auth::types::AuthSource::Stale)
        {
            return Ok(model);
        }
        if !self.has_configured_auth(model) {
            return Err(SetModelSelectionError::ProviderUnauthenticated {
                provider: provider.to_string(),
            });
        }
        Err(SetModelSelectionError::NotFound {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    fn has_configured_provider_request_auth(&self, provider: &str) -> bool {
        let Some(config) = self.provider_request_configs.get(provider) else {
            return false;
        };
        if config.headers.is_some() || config.auth_header.is_some() {
            return true;
        }
        config.api_key.as_deref().is_some_and(|key| {
            crate::auth::resolve_config_value::resolve_config_value(key).is_some()
        })
    }

    pub fn refresh(&mut self) {
        self.provider_request_configs.clear();
        self.provider_auth.clear();
        self.model_request_headers.clear();
        self.load_error = None;
        self.auth.reload();
        self.load_models();
    }

    fn load_models(&mut self) {
        let path = self.models_json_path.clone();
        let result = match &path {
            Some(path) => self.load_custom_models_file(path),
            None => CustomModelsResult::default(),
        };
        if let Some(error) = &result.error {
            self.load_error = Some(error.clone());
        }
        // The store leaves an `oauth` descriptor provider's refresh to the
        // descriptor's resolver.
        self.auth.set_descriptor_oauth_providers(
            self.provider_auth
                .iter()
                .filter(|(_, descriptor)| matches!(descriptor.strategy, AuthStrategy::Oauth(_)))
                .map(|(provider, _)| provider.clone())
                .collect(),
        );
        let credentials = self.prime_credentials();
        let built_in = self.load_built_in_models(&result, credentials.as_ref());
        self.custom_model_keys = result
            .models
            .iter()
            .map(|model| (model.provider.clone(), model.id.clone()))
            .collect();
        self.models = Self::merge_custom_models(built_in, result.models);
        self.apply_subscription_model_adaptations();
    }

    /// Per-model base-URL/API overrides plus the xAI subscription switch,
    /// applied at load: stored credentials rewrite the affected models, and
    /// the registry reloads after every credential change.
    fn apply_subscription_model_adaptations(&mut self) {
        // Re-record the configured (pre-adaptation) model for every xAI id
        // at every load (TS `xaiModelSources`): the per-call request model
        // resolves against this map, so it follows the credential that
        // serves the call, never the credential the load saw.
        self.xai_model_sources.clear();
        for model in &self.models {
            if model.provider == crate::auth::XAI_PROVIDER_ID {
                self.xai_model_sources
                    .insert(model.id.clone(), model.clone());
            }
        }
        let copilot_credential = self
            .auth
            .get_all()
            .credential(crate::auth::GITHUB_COPILOT_PROVIDER_ID);
        if let Some(AuthCredential::Oauth {
            access,
            enterprise_url,
            ..
        }) = copilot_credential
        {
            let base_url =
                pa_ai::oauth::get_github_copilot_base_url(Some(&access), enterprise_url.as_deref());
            for model in &mut self.models {
                if model.provider == crate::auth::GITHUB_COPILOT_PROVIDER_ID {
                    model.base_url.clone_from(&base_url);
                }
            }
        }
        // The stored credential is OAuth and the store is the winning
        // source (a stale credential does not serve it).
        let xai_stored = matches!(
            self.auth.get_all().credential(crate::auth::XAI_PROVIDER_ID),
            Some(AuthCredential::Oauth { .. })
        ) && self
            .auth
            .get_auth_status(crate::auth::XAI_PROVIDER_ID)
            .source
            == Some(crate::auth::types::AuthSource::Stored);
        if xai_stored {
            for model in &mut self.models {
                if model.provider == crate::auth::XAI_PROVIDER_ID {
                    *model = xai_subscription_model(model);
                }
            }
        }
    }

    fn load_custom_models_file(&mut self, path: &Path) -> CustomModelsResult {
        if !path.exists() {
            return CustomModelsResult::default();
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return CustomModelsResult {
                error: Some(format!(
                    "Failed to load models.json\n\nFile: {}",
                    path.display()
                )),
                ..Default::default()
            };
        };
        let mut result = load_custom_models(
            &content,
            &|provider| !pa_ai::models_generated::get_models(provider).is_empty(),
            &|provider| {
                pa_ai::models_generated::get_models(provider)
                    .first()
                    .map(|model| (model.api.clone(), model.base_url.clone()))
            },
        );
        // Provider/model request config from the parsed document.
        if result.error.is_none() {
            if let Ok(config) =
                super::custom::parse_models_config(&super::custom::strip_json_comments(&content))
            {
                for (provider, provider_config) in config.providers {
                    // Validated by `load_custom_models` above.
                    if let Some(descriptor) = provider_config
                        .auth
                        .as_ref()
                        .and_then(|block| ProviderAuthDescriptor::parse(&provider, block).ok())
                    {
                        self.provider_auth.insert(provider.clone(), descriptor);
                    }
                    if provider_config.api_key.is_some()
                        || provider_config.headers.is_some()
                        || provider_config.auth_header.is_some()
                    {
                        self.provider_request_configs.insert(
                            provider.clone(),
                            ProviderRequestConfig {
                                api_key: provider_config.api_key,
                                headers: provider_config.headers,
                                auth_header: provider_config.auth_header,
                            },
                        );
                    }
                    if let Some(model_overrides) = provider_config.model_overrides {
                        for (model_id, model_override) in model_overrides {
                            self.store_model_headers(
                                &provider,
                                &model_id,
                                model_override.headers.clone(),
                            );
                        }
                    }
                    if let Some(model_defs) = provider_config.models {
                        for model_def in model_defs {
                            self.store_model_headers(
                                &provider,
                                &model_def.id,
                                model_def.headers.clone(),
                            );
                        }
                    }
                }
            }
        }
        if let Some(error) = &result.error {
            // Keep built-ins only; message mirrors the TS load failure format.
            if !error.contains("models.json") {
                result.error = Some(format!("{error}\n\nFile: {}", path.display()));
            }
        }
        result
    }

    fn store_model_headers(
        &mut self,
        provider: &str,
        model_id: &str,
        headers: Option<BTreeMap<String, String>>,
    ) {
        let key = format!("{provider}:{model_id}");
        match headers {
            Some(headers) if !headers.is_empty() => {
                self.model_request_headers.insert(key, headers);
            }
            _ => {
                self.model_request_headers.remove(&key);
            }
        }
    }

    /// Built-ins through the no-cold-start chain (validated disk snapshot |
    /// bundled asset | compiled fallback), merged with the credentialed
    /// live Prime Inference snapshot for `credentials`.
    fn load_built_in_models(
        &self,
        custom: &CustomModelsResult,
        credentials: Option<&pa_models::PrimeCredentials>,
    ) -> Vec<Model> {
        self.catalog
            .resolve(credentials)
            .into_iter()
            .map(|mut model| {
                if let Some(provider_override) = custom.provider_overrides.get(&model.provider) {
                    if let Some(base_url) = &provider_override.base_url {
                        model.base_url = base_url.clone();
                        // The live catalog's serving class labels the
                        // official endpoint; a user-replaced endpoint voids
                        // it (the label must not claim a serving path the
                        // route no longer uses).
                        if model.provider == PRIME_INFERENCE_PROVIDER_ID {
                            model.serving = None;
                        }
                    }
                    if provider_override.compat.is_some() {
                        model.compat =
                            merge_compat(model.compat.as_ref(), provider_override.compat.clone());
                    }
                }
                if let Some(model_override) = custom
                    .model_overrides
                    .get(&model.provider)
                    .and_then(|overrides| overrides.get(&model.id))
                {
                    model = apply_model_override(&model, model_override);
                }
                // A built-in Chat Completions model whose endpoint is known to serve the
                // Responses route moves to it when the registry loads (a session's model is
                // resolved once, so a running session never switches); models.json models keep
                // their declared API.
                pa_ai::responses_transport::apply_responses_wire_upgrade(&mut model);
                model
            })
            .collect()
    }

    /// The credentialed endpoints background discovery checks: one per base URL of the
    /// auth-configured models on the Responses wire, or of built-in models on a plain Chat
    /// Completions wire (only those can move wires: a compat quirk or a models.json API pins a
    /// model, so its route needs no check), with the headers its requests carry. Loopback
    /// endpoints (local servers, test mocks) are never checked in the background: a request's
    /// own `auto` attempt learns them. Resolving the credentials may refresh a token, so this runs on the
    /// discovery task, never on a user-facing path.
    pub fn discovery_targets(&mut self) -> Vec<pa_ai::responses_transport::DiscoveryTarget> {
        use pa_ai::responses_transport::{DiscoveryTarget, DiscoveryWire};
        let mut seen = std::collections::HashSet::new();
        let candidates: Vec<(Model, DiscoveryWire)> = self
            .get_available()
            .into_iter()
            .filter_map(|model| {
                let loopback = url::Url::parse(&model.base_url)
                    .ok()
                    .and_then(|url| url.host().map(|host| host.to_owned()))
                    .is_none_or(|host| match host {
                        url::Host::Domain(domain) => domain == "localhost",
                        url::Host::Ipv4(address) => address.is_loopback(),
                        url::Host::Ipv6(address) => address.is_loopback(),
                    });
                if loopback {
                    return None;
                }
                let custom = self
                    .custom_model_keys
                    .contains(&(model.provider.clone(), model.id.clone()));
                let wire = match model.api.as_str() {
                    "openai-responses" => DiscoveryWire::Responses,
                    "openai-completions" if model.compat.is_none() && !custom => {
                        DiscoveryWire::ChatCompletions
                    }
                    _ => return None,
                };
                seen.insert(model.base_url.clone())
                    .then(|| (model.clone(), wire))
            })
            .collect();
        candidates
            .into_iter()
            .filter_map(|(model, wire)| {
                let auth = self.get_api_key_and_headers(&model, None);
                let api_key = auth.api_key.filter(|_| auth.ok)?;
                let mut headers = vec![("Authorization".to_string(), format!("Bearer {api_key}"))];
                for (name, value) in auth.headers.unwrap_or_default() {
                    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
                    headers.push((name, value));
                }
                Some(DiscoveryTarget {
                    base_url: model.base_url,
                    model_id: model.id,
                    wire,
                    headers,
                })
            })
            .collect()
    }

    /// Custom models win on provider+id conflicts.
    fn merge_custom_models(mut built_in: Vec<Model>, custom: Vec<Model>) -> Vec<Model> {
        for custom_model in custom {
            match built_in.iter().position(|model| {
                model.provider == custom_model.provider && model.id == custom_model.id
            }) {
                Some(index) => built_in[index] = custom_model,
                None => built_in.push(custom_model),
            }
        }
        built_in
    }

    /// Reload local state and refresh the live catalog (the provider catalog
    /// and the account's scoped Prime Inference snapshot, which carries its
    /// private routes). Both fetches are awaited, so the resolved catalog
    /// reflects them when the call returns.
    pub async fn refresh_available_models(&mut self) -> Vec<Model> {
        self.refresh_available_models_forced(false).await
    }

    /// [`ModelRegistry::refresh_available_models`] with the refresh trigger's
    /// gating: forced triggers (startup, auth change) skip the hourly catalog
    /// gate, picker-open keeps it.
    pub async fn refresh_available_models_with_trigger(
        &mut self,
        trigger: pa_models::RefreshTrigger,
    ) -> Vec<Model> {
        self.refresh_available_models_forced(trigger.forced()).await
    }

    /// The awaited refresh body: reload, the gated-or-forced chain fetch,
    /// then the auth-filtered catalog.
    async fn refresh_available_models_forced(&mut self, force: bool) -> Vec<Model> {
        self.refresh();
        let credentials = self.prime_credentials();
        self.catalog
            .refresh_with_credentials(force, credentials.as_ref())
            .await;
        self.load_models();
        self.get_available().into_iter().cloned().collect()
    }

    /// Resolve request auth: API key + merged headers for a model.
    pub fn get_api_key_and_headers(
        &mut self,
        model: &Model,
        request_headers: Option<&BTreeMap<String, String>>,
    ) -> ResolvedRequestAuth {
        let stored = self
            .auth
            .get_api_key_with_source_token(&model.provider, false);
        let mut api_key = stored.api_key;
        let provider_config = self.provider_request_configs.get(&model.provider).cloned();
        if api_key.is_none() {
            if let Some(config) = &provider_config {
                if let Some(configured) = &config.api_key {
                    if let Some(resolved) =
                        crate::auth::resolve_config_value::resolve_config_value(configured)
                    {
                        api_key = Some(resolved);
                    }
                }
            }
        }
        // The one resolver seam: a provider whose `auth` descriptor declares
        // `token-exchange` resolves an exchanged token, and one that declares
        // `oauth` its signed-in access token (unless a runtime `--api-key`
        // override serves); every other provider keeps its static key, which
        // rides the provider's own key slot unchanged.
        let descriptor = self.provider_auth.get(&model.provider).cloned();
        let runtime_override = matches!(
            &stored.source_token,
            Some(token) if token.source == AuthSource::Runtime
        );
        let resolver = match descriptor.as_ref().map(|descriptor| &descriptor.strategy) {
            Some(AuthStrategy::TokenExchange(spec)) if !runtime_override => {
                ProviderAuthResolver::TokenExchange(TokenExchange::new(
                    std::sync::Arc::clone(spec),
                    self.auth.storage_backend(),
                ))
            }
            Some(AuthStrategy::Oauth(spec)) if !runtime_override => ProviderAuthResolver::Oauth(
                DescriptorOAuth::new(std::sync::Arc::clone(spec), self.auth.storage_backend()),
            ),
            Some(
                AuthStrategy::TokenExchange(_) | AuthStrategy::Oauth(_) | AuthStrategy::ApiKey,
            )
            | None => {
                if descriptor.is_some() {
                    AuthCounter::StaticDescriptorResolve.note();
                }
                ProviderAuthResolver::Static(StaticKey::new(api_key))
            }
        };
        let ctx = AuthResolveContext {
            provider: &model.provider,
        };
        let resolution = resolver.resolve_blocking(&ctx);
        if matches!(
            resolver,
            ProviderAuthResolver::TokenExchange(_) | ProviderAuthResolver::Oauth(_)
        ) {
            // The exchange or refresh may have written a fresh token: the
            // store's last-read copy is what a provider rejection invalidates.
            self.auth.reload();
        }
        let resolved_auth = match resolution {
            Ok(resolved_auth) => resolved_auth,
            Err(error) => {
                return ResolvedRequestAuth {
                    ok: false,
                    error: Some(error.to_string()),
                    ..Default::default()
                }
            }
        };
        let api_key = resolved_auth.api_key;
        // A failed OAuth refresh with nothing else to serve: fail with the re-login sentence.
        if let (None, Some(failure)) = (&api_key, &stored.refresh_failure) {
            return ResolvedRequestAuth {
                ok: false,
                error: Some(failure.to_string()),
                ..Default::default()
            };
        }
        // TS `getApiKeyAndHeaders` resolves the xAI request model per call
        // from the lookup's own result (v0.9.8 model-registry.ts:1604-1619):
        // the subscription shape only when the stored OAuth credential
        // serves THIS call, the configured API-key shape otherwise — a
        // failed refresh that falls back to the configured key never sends
        // that key on the subscription request shape, and no key at all
        // fails the call instead of streaming keyless. Other providers
        // keep the caller's model.
        let mut request_model = None;
        let mut xai_subscription_serving = false;
        if model.provider == crate::auth::XAI_PROVIDER_ID {
            if api_key.is_none() {
                return ResolvedRequestAuth {
                    ok: false,
                    error: Some(
                        "No usable xAI credential. Use /login and select an xAI entry to sign in again or configure an API key."
                            .to_string(),
                    ),
                    ..Default::default()
                };
            }
            let configured = self
                .xai_model_sources
                .get(&model.id)
                .cloned()
                .unwrap_or_else(|| model.clone());
            xai_subscription_serving = stored.credential_type == Some("oauth")
                && matches!(
                    &stored.source_token,
                    Some(token) if token.source == crate::auth::types::AuthSource::Stored
                );
            request_model = Some(if xai_subscription_serving {
                xai_subscription_model(&configured)
            } else {
                configured
            });
        }
        let provider_headers = provider_config
            .as_ref()
            .and_then(|config| config.headers.clone());
        let auth_storage_headers = self.auth.get_provider_headers(&model.provider);
        let model_request_key = format!("{}:{}", model.provider, model.id);
        let model_request_headers = self.model_request_headers.get(&model_request_key).cloned();

        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(model_headers) = &model.headers {
            headers.extend(model_headers.clone());
        }
        if let Some(auth_storage_headers) = auth_storage_headers {
            headers.extend(auth_storage_headers);
        }
        if let Some(provider_headers) = provider_headers {
            headers.extend(provider_headers);
        }
        if let Some(model_request_headers) = model_request_headers {
            headers.extend(model_request_headers);
        }
        if provider_config
            .as_ref()
            .and_then(|config| config.auth_header)
            .unwrap_or(false)
        {
            let Some(api_key) = &api_key else {
                return ResolvedRequestAuth {
                    ok: false,
                    error: Some(format!("No API key found for \"{}\"", model.provider)),
                    ..Default::default()
                };
            };
            headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        }
        if let Some(request_headers) = request_headers {
            headers.extend(request_headers.clone());
        }
        // The resolver's own headers are the credential: they win.
        headers.extend(resolved_auth.headers);
        // The descriptor's session header carries the harness session id:
        // pa-ai's request entry fills the template per request, from the
        // conversation's stable session id (an explicit header wins).
        if let Some(session_header) = descriptor.and_then(|descriptor| descriptor.session_header) {
            if !headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case(&session_header))
            {
                headers.insert(session_header, pa_ai::SESSION_ID_TEMPLATE.to_string());
            }
        }
        if xai_subscription_serving {
            // TS refuses a custom Authorization header on the subscription
            // shape (model-registry.ts:1653-1657): only the bearer of the
            // serving credential may ride the request.
            if let Some(bearer) = api_key.as_deref().map(|key| format!("Bearer {key}")) {
                if headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization") && *value != bearer
                }) {
                    return ResolvedRequestAuth {
                        ok: false,
                        error: Some(
                            "Grok subscription cannot use a custom Authorization header. Remove the header or use /login and select the xAI API-key entry."
                                .to_string(),
                        ),
                        ..Default::default()
                    };
                }
            }
        }
        ResolvedRequestAuth {
            ok: true,
            api_key,
            headers: (!headers.is_empty()).then_some(headers),
            error: None,
            request_model,
        }
    }
}

#[cfg(test)]
mod tests;
