//! A daemon session's model scope, resolved at its open as the old worker's
//! create resolves it: the `models` patterns the session was created with
//! (else the settings' `enabledModels`) against the signed-in catalog. The
//! scope is the list `session.cycle_model` steps through (the old
//! `set_scoped_models` shape, `{model, thinkingLevel?}`), and a new session
//! that chose no model starts on the old worker's startup chain
//! (`pa_core::models::startup_chain_model`): with a scope, the saved default
//! when it is in scope, else the first scoped model, at that entry's thinking
//! level unless the session chose one; without one, the saved default, else
//! the featured default, else the first available model. When that is not
//! the saved default the session was meant to start on, the open carries the
//! notice (`modelFallbackMessage`) its clients show.

use std::path::Path;

use pa_types::ai::ModelThinkingLevel;
use serde_json::{json, Value};

use crate::SessionOptions;

/// A session's resolved scope.
pub(super) struct ModelScope {
    /// The scoped list, in the old daemon's `scopedModels` shape; empty for
    /// the whole catalog.
    pub(super) entries: Vec<Value>,
    /// The model a new session starts on, when the startup chain picks one.
    pub(super) startup: Option<StartupModel>,
    /// Why the new session does not start on the saved default
    /// (`pa_core::models::default_model_fallback_message`).
    pub(super) fallback_message: Option<String>,
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
    let new_without_model = fresh && options.model.is_none();
    if patterns.is_empty() && !new_without_model {
        return ModelScope {
            entries: Vec::new(),
            startup: None,
            fallback_message: None,
        };
    }
    let default = settings
        .get_default_provider()
        .zip(settings.get_default_model())
        .map(|(provider, model)| (provider.to_string(), model.to_string()));
    let default = default
        .as_ref()
        .map(|(provider, model)| (provider.as_str(), model.as_str()));
    let mut registry = pa_core::models::ModelRegistry::create(
        pa_core::auth::AuthStorage::create(agent_dir),
        agent_dir.join("models.json"),
    );
    let scoped = if patterns.is_empty() {
        Vec::new()
    } else {
        pa_core::models::resolve_model_scope_from_models_with_readiness(
            &mut registry,
            &patterns,
            default,
            !fresh,
            pa_core::models::SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
        )
        .await
    };
    // A new session that chose no model starts on the startup chain's pick:
    // its scope's, whatever provider it named; without one, only when it
    // named no provider either (one it named is the build's to resolve).
    let picks = new_without_model && (!scoped.is_empty() || options.provider.is_none());
    let picked = picks
        .then(|| {
            pa_core::models::startup_chain_model(
                &registry, default, &scoped, /*is_continuing*/ false,
            )
        })
        .flatten();
    let available: Vec<_> = registry.get_available().into_iter().cloned().collect();
    let fallback_message = picked.as_ref().zip(default).and_then(|(model, default)| {
        pa_core::models::default_model_fallback_message(
            model,
            default,
            &patterns,
            &available,
            registry.get_all(),
        )
    });
    // The build resolves a chosen model among the signed-in ones: a pick no
    // provider is signed in for (the catalog's first, with none signed in) is
    // left to the build, which starts on that same model.
    let startup = picked
        .filter(|model| !scoped.is_empty() || available.contains(model))
        .map(|model| StartupModel {
            thinking: scoped
                .iter()
                .find(|entry| entry.model.provider == model.provider && entry.model.id == model.id)
                .and_then(|entry| entry.thinking_level)
                .and_then(|level| serde_json::from_value(json!(level)).ok()),
            provider: model.provider,
            id: model.id,
        });
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
    ModelScope {
        entries,
        startup,
        fallback_message,
    }
}
