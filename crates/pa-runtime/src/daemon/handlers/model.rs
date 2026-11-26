//! The model catalog and model changes: the `models` and `session.model`
//! queries, `session.set_model` and `session.set_effort`, the old daemon's
//! `get_available_models`, `get_connection_state` (its model, level and
//! levels), `set_model` and `set_thinking_level` contracts.
//!
//! `session.model {session}` answers an open session's model (the whole
//! model in its wire shape), its thinking level, and the levels the model
//! supports (`["off"]` for a model that does not think).
//! `session.set_effort {session, level}` on an open session clamps the level
//! to what the model supports, records the `thinking_level_change` row only
//! when the effective level changes, and lets the settings' default level
//! follow when the model thinks or the level is not `off`; it answers the
//! effective level. A saved session has the row appended under its lease.
//!
//! A model is resolved through the agent dir's model registry: a selector
//! (`model`) as the command line reads one, or a `provider` and `modelId`
//! among the models whose provider is signed in. The settings'
//! `allowedModels` allowlist is enforced on the result through pa-core's
//! gate, the one the old daemon calls, and fails closed: a global settings
//! document that cannot be read, or an `allowedModels` that is not a list
//! of strings, refuses every change. An
//! open session switches its running agent (the next request uses the
//! model) and records the `model_change` row through its engine, and the
//! settings' default model follows, as a switch in the old daemon does; a
//! saved one has the row appended under its lease.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::models::ModelRegistry;
use pa_types::ai::ModelThinkingLevel;
use serde_json::{json, Value};

use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

/// The agent dir's model registry: its sign-ins and `models.json`.
fn registry(agent_dir: &Path) -> ModelRegistry {
    let auth = pa_core::auth::AuthStorage::create(agent_dir);
    ModelRegistry::create(auth, agent_dir.join("models.json"))
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `models`: the models whose provider is signed in (`{"models": [...]}`,
    /// each in the model's wire shape).
    pub(super) async fn models(&self) -> anyhow::Result<Value> {
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || {
            let registry = registry(&agent_dir);
            let models = registry
                .get_available()
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<Value>, _>>()?;
            Ok(json!({ "models": models }))
        })
        .await
        .context("the catalog's thread panicked")?
    }

    /// `session.set_model`: switches session `args.session` to the model
    /// `args` names (see the module documentation) and answers it.
    pub(super) async fn set_model(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let cwd = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
        let model = {
            let (agent_dir, args, cwd) = (self.agent_dir.clone(), args.clone(), cwd.clone());
            tokio::task::spawn_blocking(move || {
                let registry = registry(&agent_dir);
                let provider = optional_string_arg(&args, "provider")?;
                let model_id = optional_string_arg(&args, "modelId")?;
                let model = match (provider, model_id, optional_string_arg(&args, "model")?) {
                    (Some(provider), Some(model_id), None) => registry
                        .resolve_set_model_selection(&provider, &model_id)?
                        .clone(),
                    (None, None, Some(selector)) => {
                        let resolved = pa_core::models::resolve_cli_model(
                            /*cli_provider*/ None,
                            &selector,
                            registry.get_all(),
                        );
                        if let Some(error) = resolved.error {
                            anyhow::bail!(error);
                        }
                        resolved
                            .model
                            .with_context(|| format!("no model matches {selector:?}"))?
                    }
                    _ => anyhow::bail!(
                        "give session.set_model a `model`, or a `provider` and a `modelId`"
                    ),
                };
                // The old daemon's gate: an unreadable or malformed
                // allowlist refuses every model.
                pa_core::models::assert_model_allowed(
                    &pa_core::models::load_model_allowlist(&cwd, &agent_dir),
                    &format!("{}/{}", model.provider, model.id),
                )?;
                anyhow::Ok(model)
            })
            .await
            .context("the model's thread panicked")??
        };
        let (provider, model_id) = (model.provider.clone(), model.id.clone());
        if open {
            let live = self.runtime.built_engine(&id).await?;
            let switched = model.clone();
            live.runtime
                .spawn(async move {
                    live.engine
                        .session
                        .set_model(&switched, &switched.provider, &switched.id)
                        .await
                })
                .await
                .context("the session's runtime ended during the change")??;
            // The next session starts on the switched model.
            let (agent_dir, provider, model_id) =
                (self.agent_dir.clone(), provider.clone(), model_id.clone());
            tokio::task::spawn_blocking(move || {
                pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
                    .set_default_model_and_provider(&provider, &model_id)
            })
            .await
            .context("the settings' thread panicked")??;
        } else {
            let (provider, model_id) = (provider.clone(), model_id.clone());
            self.change_saved(path, move |manager| {
                manager.append_model_change(&provider, &model_id)?;
                Ok(())
            })
            .await?;
        }
        Ok(json!({
            "session": id,
            "provider": provider,
            "modelId": model_id,
            "model": model,
        }))
    }

    /// `session.model`: the open session's model, thinking level and the
    /// levels its model supports (see the module documentation).
    pub(super) async fn model(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let state = live
            .runtime
            .spawn(async move { live.engine.session.agent().state().await })
            .await
            .context("the session's runtime ended during the read")?;
        let model = self.full_model(&state.model).await?;
        let levels: Vec<&str> = pa_types::ai::get_supported_thinking_levels(&model)
            .into_iter()
            .map(ModelThinkingLevel::wire_name)
            .collect();
        Ok(json!({
            "session": id,
            "model": model,
            "thinkingLevel": state.thinking_level,
            "availableThinkingLevels": levels,
        }))
    }

    /// The whole model a session's agent runs: the registry's entry for its
    /// provider and id (with its thinking-level map), else the agent's own
    /// descriptor, which carries no map (a model the registry does not list,
    /// such as one an engine registered for itself).
    pub(super) async fn full_model(
        &self,
        running: &pa_agent::types::Model,
    ) -> anyhow::Result<pa_types::ai::Model> {
        let agent_dir = self.agent_dir.clone();
        let (provider, model_id) = (running.provider.clone(), running.id.clone());
        let listed = tokio::task::spawn_blocking(move || {
            registry(&agent_dir)
                .get_all()
                .iter()
                .find(|model| model.provider == provider && model.id == model_id)
                .cloned()
        })
        .await
        .context("the catalog's thread panicked")?;
        Ok(listed.unwrap_or_else(|| pa_types::ai::Model {
            id: running.id.clone(),
            name: running.name.clone(),
            api: running.api.clone(),
            provider: running.provider.clone(),
            base_url: running.base_url.clone(),
            reasoning: running.reasoning,
            thinking_level_map: None,
            input: Vec::new(),
            cost: pa_types::ai::ModelCost {
                input: running.cost.input.into(),
                output: running.cost.output.into(),
                cache_read: running.cost.cache_read.into(),
                cache_write: running.cost.cache_write.into(),
            },
            context_window: running.context_window,
            max_tokens: running.max_tokens,
            max_tokens_explicit: running.max_tokens_explicit,
            featured: None,
            serving: None,
            headers: None,
            compat: None,
        }))
    }

    /// `session.set_effort`: sets session `args.session`'s thinking level
    /// (see the module documentation) and answers the effective one.
    pub(super) async fn set_effort(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let name = string_arg(args, "level")?;
        let requested = pa_types::ai::thinking_levels::thinking_level_from_str(&name)
            .with_context(|| {
                format!(
                    "unknown thinking level {name:?} (off, minimal, low, medium, high, xhigh, max)"
                )
            })?;
        if !open {
            self.change_saved(path, move |manager| {
                manager.append_thinking_level_change(&name)?;
                Ok(())
            })
            .await?;
            return Ok(json!({ "session": id, "level": requested.wire_name() }));
        }
        let live = self.runtime.built_engine(&id).await?;
        let running = {
            let engine = Arc::clone(&live.engine);
            live.runtime
                .spawn(async move { engine.session.agent().state().await.model })
                .await
                .context("the session's runtime ended during the read")?
        };
        let model = self.full_model(&running).await?;
        let effective = pa_types::ai::clamp_thinking_level(&model, requested);
        let level = pa_core::session_engine::provider_adapter::map_thinking_level(effective);
        live.runtime
            .spawn(async move {
                let agent = live.engine.session.agent();
                // The row only when the level changes, as the old daemon.
                if agent.state().await.thinking_level == level {
                    agent.set_thinking_level(level).await;
                    anyhow::Ok(())
                } else {
                    live.engine.session.set_thinking_level(level).await
                }
            })
            .await
            .context("the session's runtime ended during the change")??;
        let reasoning = model.reasoning;
        // The old daemon's settings rule: the default follows a model that
        // thinks, or a level that is not `off`, as the clamped level.
        if reasoning || effective != ModelThinkingLevel::Off {
            let cwd = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
            let agent_dir = self.agent_dir.clone();
            tokio::task::spawn_blocking(move || {
                pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
                    .set_default_thinking_level(
                        pa_core::settings::ThinkingLevelSetting::from_model_level(effective),
                    )
            })
            .await
            .context("the settings' thread panicked")??;
        }
        Ok(json!({ "session": id, "level": effective.wire_name() }))
    }
}
