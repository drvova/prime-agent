//! The model a daemon session starts on when nothing chose one, and the
//! notice it carries when that is not the model it was meant to run on: the
//! settings' default it could not start on, or the saved model a reopened
//! session could not restore. Both daemons resolve it here, and their session
//! summaries carry the notice as `modelFallbackMessage`, which the TUI shows
//! once as the session opens. A fallback is never silent.

use pa_types::ai::Model;

use super::registry::ModelRegistry;
use super::resolver::{
    find_initial_model, scope_patterns_select_reference, InitialModelOptions, ScopedModel,
};

/// The startup chain over `registry` (the scope's pick, the settings'
/// `default`, the featured default, the first available model; the scope
/// is skipped for a continued session), else the catalog's first model.
#[must_use]
pub fn startup_chain_model(
    registry: &ModelRegistry,
    default: Option<(&str, &str)>,
    scoped_models: &[ScopedModel],
    is_continuing: bool,
) -> Option<Model> {
    let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
    let all = registry.get_all();
    find_initial_model(&InitialModelOptions {
        cli_provider: None,
        cli_model: None,
        scoped_models,
        is_continuing,
        default_provider: default.map(|(provider, _)| provider),
        default_model_id: default.map(|(_, model)| model),
        all_models: all,
        available_models: &available,
    })
    .or_else(|| all.first().cloned())
}

/// The notice for a new session that starts on `model` while the settings'
/// default is `default`: `None` when it is the default, or when the session's
/// model scope (`patterns`, empty for the whole catalog) leaves the default
/// out, so the session was never meant to start on it.
#[must_use]
pub fn default_model_fallback_message(
    model: &Model,
    default: (&str, &str),
    patterns: &[String],
    available_models: &[Model],
    all_models: &[Model],
) -> Option<String> {
    let (provider, model_id) = default;
    if model.provider == provider && model.id == model_id {
        return None;
    }
    let wanted = patterns.is_empty()
        || scope_patterns_select_reference(
            patterns,
            provider,
            model_id,
            available_models,
            all_models,
        );
    wanted.then(|| {
        format!(
            "Default model {provider}/{model_id} is not available. Using {}/{}",
            model.provider, model.id
        )
    })
}

/// The notice for a reopened session whose saved model (`provider`/`model_id`)
/// could not be restored, and which runs on `fallback` instead (none when no
/// model was found at all).
#[must_use]
pub fn restore_fallback_message(
    provider: &str,
    model_id: &str,
    fallback: Option<&Model>,
) -> String {
    match fallback {
        Some(fallback) => format!(
            "Could not restore model {provider}/{model_id}. Using {}/{}",
            fallback.provider, fallback.id
        ),
        None => format!("Could not restore model {provider}/{model_id}"),
    }
}
