//! The model catalog and model changes: the `models` query and
//! `session.set_model`, the old daemon's `get_available_models` and
//! `set_model` contract.
//!
//! A model is resolved through the agent dir's model registry: a selector
//! (`model`) as the command line reads one, or a `provider` and `modelId`
//! among the models whose provider is signed in. The settings'
//! `allowedModels` allowlist is enforced on the result, and fails closed: a
//! global settings document that cannot be read refuses every change. An
//! open session switches its running agent (the next request uses the
//! model) and records the `model_change` row through its engine, and the
//! settings' default model follows, as a switch in the old daemon does; a
//! saved one has the row appended under its lease.

use std::path::Path;

use anyhow::Context as _;
use pa_core::models::{ModelAllowlistRefusal, ModelRegistry};
use serde_json::{json, Value};

use super::{optional_string_arg, RuntimeHandlers};
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
                // The allowlist fails closed: unreadable global settings are
                // an unknown policy, never an unrestricted one.
                let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
                let selector = format!("{}/{}", model.provider, model.id);
                if let Some(error) = settings
                    .errors()
                    .iter()
                    .find(|error| error.scope == pa_core::settings::SettingsScope::Global)
                {
                    anyhow::bail!(
                        "The daemon model allowlist could not be read ({}); refusing to resolve \
                         model \"{selector}\" — the daemon fails closed instead of bypassing the \
                         configured allowedModels policy. Fix settings.json and retry.",
                        error.message
                    );
                }
                if let Some(patterns) = settings.get_allowed_models() {
                    if !pa_core::models::model_allowed(&selector, &patterns) {
                        return Err(ModelAllowlistRefusal { selector }.into());
                    }
                }
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
}
