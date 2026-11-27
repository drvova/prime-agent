//! A session's live setting switches, the old daemon's worker commands of
//! the same names: `session.cycle_model`, `session.set_scoped_models`,
//! `session.cycle_effort` (`cycle_thinking_level`),
//! `session.set_service_tier`, `session.set_transport`,
//! `session.set_steering_mode`, `session.set_follow_up_mode`,
//! `session.set_auto_compaction` and `session.set_auto_retry`. Each is a
//! call on an open session; its result is the old response's `data`, and
//! `null` for a command whose old response had none. The rules both daemons
//! follow (the cycle's candidates and step, the scoped list, the queue mode
//! and transport a switch persists) are pa-core's
//! `pa_core::settings::switches`.
//!
//! - `session.cycle_model {session, direction?}` steps through the scoped
//!   list (its entries among the available models) when one is set, else
//!   the available models, forward or `"backward"`; fewer than two answer
//!   `null`. The model is gated by the `allowedModels` allowlist, switched
//!   with its level (a scoped entry's pinned level, else the running one,
//!   clamped to the model) and recorded as a `model_change` row, and the
//!   settings' default model follows. Answers `{model, thinkingLevel,
//!   serviceTier, isScoped}`.
//! - `session.set_scoped_models {session, scopedModels}` sets the list for
//!   the session's open life (the snapshot's `scopedModels`).
//! - `session.cycle_effort {session}` steps a thinking model's level through
//!   the levels its model supports, as `session.set_effort` sets one;
//!   answers `{level}`, or `null` for a model that does not think.
//! - `session.set_service_tier {session, serviceTier}` keeps the requested
//!   tier as the session's preference (a `service_tier_change` row when it
//!   changes) and the settings' default when the model supports it; the
//!   active tier is the preference clamped to the model.
//! - The queue modes, transport, auto-compaction and auto-retry switches
//!   write the settings the session's next turn reads; the queue modes and
//!   auto-compaction also switch the running session.
//!
//! A change of the active service tier (a tier set, or a model cycled to
//! one that keeps a different tier) is the old `service_tier_changed`
//! event on the session's stream; a model or level change reaches the
//! roster feed.

use anyhow::Context as _;
use pa_core::settings::switches::{self, CycleDirection};
use pa_core::settings::SettingsManager;
use pa_types::ai::{Model, ServiceTier, Transport};
use serde_json::{json, Value};

use super::model::registry;
use super::RuntimeHandlers;
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// Writes the global settings of open session `id` (its working
    /// directory's settings document) with `write`.
    async fn write_settings(
        &self,
        id: &str,
        write: impl FnOnce(&mut SettingsManager) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let cwd = self
            .open
            .cwd(id)
            .with_context(|| format!("session {id} is not open in the runtime daemon"))?;
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || write(&mut SettingsManager::create(&cwd, &agent_dir)))
            .await
            .context("the settings' thread panicked")?
    }

    /// Open session `id`'s running model (the whole model, as the registry
    /// lists it) and its service-tier preference (its last
    /// `service_tier_change` row).
    async fn running(&self, id: &str) -> anyhow::Result<(Model, Option<ServiceTier>)> {
        let live = self.runtime.built_engine(id).await?;
        let (running, preference) = live
            .runtime
            .spawn(async move {
                let model = live.engine.session.agent().state().await.model;
                let persistence = live.engine.session.shared_persistence();
                let preference = persistence.lock().await.active_context().service_tier;
                (model, preference)
            })
            .await
            .context("the session's runtime ended during the read")?;
        Ok((self.full_model(&running).await?, preference))
    }

    /// The old `service_tier_changed` event on open session `id`'s stream,
    /// when the active tier moved from `before` to `after`.
    fn tier_moved(
        &self,
        id: &str,
        before: Option<ServiceTier>,
        after: Option<ServiceTier>,
    ) -> anyhow::Result<()> {
        if before != after {
            let stream = self.runtime.stream_sender(id)?;
            stream(json!({
                "type": "service_tier_changed",
                "serviceTier": after.unwrap_or(ServiceTier::Auto),
            }));
        }
        Ok(())
    }

    /// `session.cycle_model` (see the module documentation).
    pub(super) async fn cycle_model(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let direction = match args.get("direction").and_then(Value::as_str) {
            Some("backward") => CycleDirection::Backward,
            _ => CycleDirection::Forward,
        };
        let scoped = self.open.scoped_models(&id);
        let (running, preference) = self.running(&id).await?;
        let cwd = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
        let next = {
            let (agent_dir, scoped, cwd) = (self.agent_dir.clone(), scoped.clone(), cwd.clone());
            let current = (running.provider.clone(), running.id.clone());
            tokio::task::spawn_blocking(move || {
                let available: Vec<Model> = registry(&agent_dir)
                    .get_available()
                    .into_iter()
                    .cloned()
                    .collect();
                let candidates = switches::cycle_candidates(&scoped, &available);
                let Some(next) = switches::next_candidate(
                    &candidates,
                    Some((current.0.as_str(), current.1.as_str())),
                    direction,
                ) else {
                    return anyhow::Ok(None);
                };
                // The old daemon's gate, before anything moves.
                pa_core::models::assert_model_allowed(
                    &pa_core::models::load_model_allowlist(&cwd, &agent_dir),
                    &format!("{}/{}", next.model.provider, next.model.id),
                )?;
                anyhow::Ok(Some(next.clone()))
            })
            .await
            .context("the catalog's thread panicked")??
        };
        let Some(next) = next else {
            return Ok(Value::Null);
        };
        let model = next.model;
        let live = self.runtime.built_engine(&id).await?;
        let pinned = next
            .thinking_level
            .as_deref()
            .and_then(pa_types::ai::thinking_levels::thinking_level_from_str);
        let switched = model.clone();
        let level = live
            .runtime
            .spawn(async move {
                let session = &live.engine.session;
                let current = pa_core::session_engine::provider_adapter::model_thinking_level(
                    session.agent().state().await.thinking_level,
                );
                let level =
                    pa_types::ai::clamp_thinking_level(&switched, pinned.unwrap_or(current));
                session
                    .set_model_and_thinking_level(
                        &switched,
                        &switched.provider,
                        &switched.id,
                        pa_core::session_engine::provider_adapter::map_thinking_level(level),
                    )
                    .await
                    .map(|()| level)
            })
            .await
            .context("the session's runtime ended during the change")??;
        // The next session starts on the cycled model.
        let (provider, model_id) = (model.provider.clone(), model.id.clone());
        self.write_settings(&id, move |settings| {
            settings.set_default_model_and_provider(&provider, &model_id)
        })
        .await?;
        let before = pa_types::ai::clamp_service_tier(Some(&running), preference);
        let after = pa_types::ai::clamp_service_tier(Some(&model), preference);
        self.tier_moved(&id, before, after)?;
        self.feeds.due(&id);
        Ok(json!({
            "model": model,
            "thinkingLevel": level.wire_name(),
            "serviceTier": after.unwrap_or(ServiceTier::Auto),
            "isScoped": !scoped.is_empty(),
        }))
    }

    /// `session.set_scoped_models` (see the module documentation).
    pub(super) fn set_scoped_models(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let scoped =
            switches::scoped_models(args.get("scopedModels")).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            self.open.set_scoped_models(&id, scoped.to_vec()),
            "session {id} is not open in the runtime daemon"
        );
        Ok(Value::Null)
    }

    /// `session.cycle_effort` (see the module documentation).
    pub(super) async fn cycle_effort(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let state = live
            .runtime
            .spawn(async move { live.engine.session.agent().state().await })
            .await
            .context("the session's runtime ended during the read")?;
        let model = self.full_model(&state.model).await?;
        if !model.reasoning {
            return Ok(Value::Null);
        }
        let levels = pa_types::ai::get_supported_thinking_levels(&model);
        let current =
            pa_core::session_engine::provider_adapter::model_thinking_level(state.thinking_level);
        let Some(&next) = switches::next_thinking_level(&levels, &current) else {
            return Ok(Value::Null);
        };
        self.apply_effort(&id, next).await?;
        Ok(json!({ "level": next.wire_name() }))
    }

    /// `session.set_service_tier` (see the module documentation).
    pub(super) async fn set_service_tier(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let tier: ServiceTier = args
            .get("serviceTier")
            .filter(|tier| !tier.is_null())
            .and_then(|tier| serde_json::from_value(tier.clone()).ok())
            .context("set_service_tier requires a serviceTier")?;
        let (model, preference) = self.running(&id).await?;
        let before = pa_types::ai::clamp_service_tier(Some(&model), preference);
        let after = pa_types::ai::clamp_service_tier(Some(&model), Some(tier));
        if preference != Some(tier) {
            // The row keeps the requested tier, not the clamped one.
            let live = self.runtime.built_engine(&id).await?;
            live.runtime
                .spawn(async move {
                    let persistence = live.engine.session.shared_persistence();
                    let mut manager = persistence.lock().await;
                    manager.append_service_tier_change(Some(tier)).map(|_| ())
                })
                .await
                .context("the session's runtime ended during the change")??;
            // The default only when the model supports the tier.
            if pa_types::ai::supports_service_tier(&model, tier) {
                self.write_settings(&id, move |settings| settings.set_default_service_tier(tier))
                    .await?;
            }
        }
        self.tier_moved(&id, before, after)?;
        Ok(Value::Null)
    }

    /// `session.set_transport` (see the module documentation).
    pub(super) async fn set_transport(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let transport: Transport = args
            .get("transport")
            .and_then(|transport| serde_json::from_value(transport.clone()).ok())
            .context("set_transport requires a transport")?;
        let setting = switches::transport_setting(transport);
        self.write_settings(&id, move |settings| settings.set_transport(setting))
            .await?;
        Ok(Value::Null)
    }

    /// `session.set_steering_mode` and `session.set_follow_up_mode`, as
    /// `queue` names (see the module documentation).
    pub(super) async fn set_queue_mode(&self, args: &Value, queue: Queue) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let command = match queue {
            Queue::Steering => "set_steering_mode",
            Queue::FollowUp => "set_follow_up_mode",
        };
        let setting = switches::queue_mode(command, args.get("mode").and_then(Value::as_str))
            .map_err(anyhow::Error::msg)?;
        self.write_settings(&id, move |settings| match queue {
            Queue::Steering => settings.set_steering_mode(setting),
            Queue::FollowUp => settings.set_follow_up_mode(setting),
        })
        .await?;
        // The running agent drains by the new mode from its next step.
        let mode = match setting {
            pa_core::settings::QueueModeSetting::All => pa_agent::agent::QueueMode::All,
            pa_core::settings::QueueModeSetting::OneAtATime => {
                pa_agent::agent::QueueMode::OneAtATime
            }
        };
        let live = self.runtime.built_engine(&id).await?;
        let agent = live.engine.session.agent();
        match queue {
            Queue::Steering => agent.set_steering_mode(mode),
            Queue::FollowUp => agent.set_follow_up_mode(mode),
        }
        Ok(Value::Null)
    }

    /// `session.set_auto_compaction` (see the module documentation): the
    /// settings first, so a failed save switches nothing.
    pub(super) async fn set_auto_compaction(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        // Missing reads as off, as the old command read it.
        let enabled = args
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.write_settings(&id, move |settings| {
            settings.set_compaction_enabled(enabled)
        })
        .await?;
        let live = self.runtime.built_engine(&id).await?;
        live.engine.session.set_auto_compaction_enabled(enabled);
        Ok(Value::Null)
    }

    /// `session.set_auto_retry` (see the module documentation): each turn
    /// reads the retry policy from the settings.
    pub(super) async fn set_auto_retry(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let enabled = args
            .get("enabled")
            .and_then(Value::as_bool)
            .context("set_auto_retry requires enabled")?;
        self.write_settings(&id, move |settings| settings.set_retry_enabled(enabled))
            .await?;
        Ok(Value::Null)
    }
}

/// Which of a session's queues a mode switch is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Queue {
    Steering,
    FollowUp,
}
