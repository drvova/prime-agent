//! A daemon session's model scope, resolved at its open as the old worker's
//! create resolves it: the `models` patterns the session was created with
//! (else the settings' `enabledModels`) against the signed-in catalog. The
//! scope is the list `session.cycle_model` steps through (the old
//! `set_scoped_models` shape, `{model, thinkingLevel?}`), and a new session
//! that chose no model starts on its pick: the saved default when it is in
//! scope, else the first scoped model, at that entry's thinking level unless
//! the session chose one.

use std::path::Path;

use pa_types::ai::ModelThinkingLevel;
use serde_json::{json, Value};

use crate::SessionOptions;

/// A session's resolved scope.
pub(super) struct ModelScope {
    /// The scoped list, in the old daemon's `scopedModels` shape; empty for
    /// the whole catalog.
    pub(super) entries: Vec<Value>,
    /// The model a new session starts on, when its scope picks one.
    pub(super) startup: Option<StartupModel>,
}

/// The scope's pick for a new session.
pub(super) struct StartupModel {
    provider: String,
    id: String,
    thinking: Option<ModelThinkingLevel>,
}

impl StartupModel {
    /// Builds the session on this model, and at its entry's level unless the
    /// session chose one. The stored options are not changed: the pick is
    /// this open's, as the old worker's startup scope was.
    pub(super) fn choose(self, options: &mut SessionOptions) {
        options.provider = Some(self.provider);
        options.model = Some(self.id);
        if options.thinking.is_none() {
            options.thinking = self.thinking;
        }
    }
}

/// The scope of a session opened with `options` in `cwd`; `fresh` for a
/// session with no transcript yet, which may start on the scope's pick.
pub(super) async fn resolve(
    agent_dir: &Path,
    cwd: &Path,
    options: &SessionOptions,
    fresh: bool,
) -> ModelScope {
    let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
    let patterns = match &options.models {
        Some(patterns) => patterns.clone(),
        None => settings.get_enabled_models().unwrap_or_default(),
    };
    if patterns.is_empty() {
        return ModelScope {
            entries: Vec::new(),
            startup: None,
        };
    }
    let default = settings
        .get_default_provider()
        .zip(settings.get_default_model())
        .map(|(provider, model)| (provider.to_string(), model.to_string()));
    let mut registry = pa_core::models::ModelRegistry::create(
        pa_core::auth::AuthStorage::create(agent_dir),
        agent_dir.join("models.json"),
    );
    let scoped = pa_core::models::resolve_model_scope_from_models_with_readiness(
        &mut registry,
        &patterns,
        default
            .as_ref()
            .map(|(provider, model)| (provider.as_str(), model.as_str())),
        !fresh,
        pa_core::models::SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
    )
    .await;
    let startup = if fresh && options.model.is_none() {
        let available: Vec<_> = registry.get_available().into_iter().cloned().collect();
        pa_core::models::find_initial_model(&pa_core::models::InitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &scoped,
            is_continuing: false,
            default_provider: default.as_ref().map(|(provider, _)| provider.as_str()),
            default_model_id: default.as_ref().map(|(_, model)| model.as_str()),
            all_models: registry.get_all(),
            available_models: &available,
        })
        .filter(|_| !scoped.is_empty())
        .map(|model| StartupModel {
            thinking: scoped
                .iter()
                .find(|entry| entry.model.provider == model.provider && entry.model.id == model.id)
                .and_then(|entry| entry.thinking_level)
                .and_then(|level| serde_json::from_value(json!(level)).ok()),
            provider: model.provider,
            id: model.id,
        })
    } else {
        None
    };
    let entries = scoped
        .iter()
        .map(|scoped| {
            let mut entry = json!({ "model": scoped.model });
            if let Some(level) = scoped.thinking_level {
                entry["thinkingLevel"] = json!(level);
            }
            entry
        })
        .collect();
    ModelScope { entries, startup }
}
