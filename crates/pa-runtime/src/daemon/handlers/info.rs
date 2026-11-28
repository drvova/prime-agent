//! What the TUI reads about a session and the daemon beside its turns, the
//! old daemon's worker and supervisor commands of the same names, each
//! answering the old response's `data` (`null` for one that had none), the
//! shared logic pa-core's:
//!
//! - `session.commands {session}` (`get_commands`): `{commands}`, the open
//!   session's prompt templates then its skills
//!   (`ResourceReload::connection_commands`).
//! - `session.mcp_connections {session}` (`get_mcp_connections`): the `/mcp`
//!   page from the session's MCP manager, `{connections, services,
//!   credentials, catalogDiagnostics}` (`McpManager::connections_view`).
//! - `session.model_catalog {session}` (`get_model_catalog`): the `/model`
//!   picker's catalog from the current snapshot, `{models,
//!   configuredProviders, billing, accounts, accountChoices}`, the accounts
//!   read through the session's credential pin (`CatalogRefresh::serve`).
//!   The background refresh it arms is coalesced across the daemon's
//!   sessions; a refresh that changes the snapshot publishes `{}` on the
//!   live-only `runtime/models`, the old `model_catalog_changed` broadcast.
//! - `usage {section, forceRefresh?}` (the supervisor's `get_usage`, no
//!   session): the `quotas` half (`QuotaReport`, with this process's request
//!   pacing: its sessions' requests are admitted here) or the `spend` half
//!   (`SpendHistory` over the daemon's sessions dir) (`pa_core::usage_report`).
//! - `session.reload {session}` (`reload`): the session's MCP manager reads
//!   its auth store and settings again (a failed auth reload fails the call),
//!   then its skills, prompt templates and context files are read again.
//! - `session.mark_anthropic_warning_shown {session}`: the once-per-session
//!   marker row, written once (`SessionManager::mark_anthropic_warning_shown`).

use anyhow::Context as _;
use pa_types::event_log::{NewEvent, Topic, Trust};
use pa_types::provider_usage::UsageSection;
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::session::SessionBuilder;

/// The live-only topic a changed model catalog is announced on.
const MODELS_TOPIC: &str = "runtime/models";

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.commands` (see the module documentation).
    pub(super) async fn commands(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        Ok(json!({ "commands": live.engine.resource_reload.connection_commands() }))
    }

    /// `session.mcp_connections` (see the module documentation): off the
    /// runtime, as the reads take the auth store's blocking lock.
    pub(super) async fn mcp_connections(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let manager = std::sync::Arc::clone(&live.engine.mcp_manager);
        tokio::task::spawn_blocking(move || {
            manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connections_view()
        })
        .await
        .map_err(|error| anyhow::anyhow!("MCP roster read failed: {error}"))
    }

    /// `session.model_catalog` (see the module documentation).
    pub(super) async fn model_catalog(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let persistence = live.engine.session.shared_persistence();
        let pin = live
            .runtime
            .spawn(async move {
                persistence
                    .lock()
                    .await
                    .get_header()
                    .and_then(pa_types::session::SessionHeader::credential_pin)
            })
            .await
            .context("the session's runtime ended during the read")?;
        let (catalog, agent_dir, runtime) = (
            std::sync::Arc::clone(&self.catalog),
            self.agent_dir.clone(),
            std::sync::Arc::clone(&self.runtime),
        );
        tokio::task::spawn_blocking(move || {
            catalog.serve(&agent_dir, pin.as_ref(), move || {
                let runtime = std::sync::Arc::clone(&runtime);
                tokio::spawn(async move {
                    let published = runtime
                        .publish(NewEvent {
                            topic: Topic::new(MODELS_TOPIC).expect("a topic"),
                            source: "runtime".to_string(),
                            session: None,
                            payload: json!({}),
                            trust: Trust::Owner,
                            cause: None,
                            reply: None,
                        })
                        .await;
                    if let Err(error) = published {
                        tracing::warn!("a model catalog change was not published: {error:#}");
                    }
                });
            })
        })
        .await
        .context("the catalog's thread panicked")
    }

    /// `usage` (see the module documentation).
    pub(super) async fn usage_page(&self, args: &Value) -> anyhow::Result<Value> {
        let section: UsageSection = serde_json::from_value(
            args.get("section")
                .cloned()
                .context("missing the `section` argument")?,
        )
        .context("the `section` argument is `quotas` or `spend`")?;
        let force_refresh = match args.get("forceRefresh") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => anyhow::bail!("the `forceRefresh` argument must be a boolean"),
        };
        Ok(match section {
            UsageSection::Quotas => serde_json::to_value(
                pa_core::usage_report::quota_page(
                    self.agent_dir.clone(),
                    force_refresh,
                    pa_ai::admission::local().pacing(),
                )
                .await,
            )?,
            UsageSection::Spend => {
                serde_json::to_value(pa_core::usage_report::spend_page(&self.session_dir).await?)?
            }
        })
    }

    /// `session.reload` (see the module documentation).
    pub(super) async fn reload(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let manager = std::sync::Arc::clone(&live.engine.mcp_manager);
        tokio::task::spawn_blocking(move || {
            manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reload_live_inputs()
        })
        .await
        .map_err(|error| anyhow::anyhow!("session reload failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("session reload failed: {error}"))?;
        let engine = std::sync::Arc::clone(&live.engine);
        live.runtime
            .spawn(async move { engine.resource_reload.reload().await })
            .await
            .context("the session's runtime ended during the reload")?
            .map_err(|error| anyhow::anyhow!("session reload failed: {error:#}"))?;
        Ok(Value::Null)
    }

    /// `session.mark_anthropic_warning_shown` (see the module
    /// documentation).
    pub(super) async fn mark_anthropic_warning_shown(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let live = self.runtime.built_engine(&id).await?;
        let persistence = live.engine.session.shared_persistence();
        live.runtime
            .spawn(async move { persistence.lock().await.mark_anthropic_warning_shown() })
            .await
            .context("the session's runtime ended during the change")??;
        Ok(Value::Null)
    }
}
