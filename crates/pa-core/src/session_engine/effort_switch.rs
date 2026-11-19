//! The kernel's opt-in `model.set_effort(level)` (host request `model.set_effort`): change the
//! CURRENT session's thinking level from the REPL.
//!
//! It changes what every later call costs, so it is gated behind the global
//! `replCanChangeEffort` setting (off by default; a project's settings cannot turn it on). With
//! the setting off the request fails with how to enable it. Reading the level is always allowed
//! (`model.info()`, `settings.get()`).
//!
//! The switch applies from the next model call. An embedding that owns the session's model
//! selection (the daemon worker: clamping to the model's levels, the durable row, the settings
//! default, the roster) registers [`MODEL_APPLY_EFFORT_REQUEST_TYPE`] and the switch goes
//! through it; otherwise the switch sets the agent's level and records the session row here.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use anyhow::{anyhow, bail};
use pa_agent::types::ThinkingLevel;
use serde_json::{json, Value};

use super::turn_boundary::TurnBoundaryRequests;
use crate::kernel::shared::{host_handler, HostHandlerFn, HostRequestHandlers, HostRequestPayload};

/// The embedding's switch: payload `{level}` (a validated wire level), reply `{thinkingLevel}`
/// (the effective level after the embedding's clamping).
pub const MODEL_APPLY_EFFORT_REQUEST_TYPE: &str = "model.apply_effort";

/// The setting that turns the switch on.
pub const REPL_CAN_CHANGE_EFFORT_SETTING: &str = "replCanChangeEffort";

/// Observes each switch: applied (`true`) or refused (adoption telemetry; never the level).
pub(crate) type EffortObserver = Arc<dyn Fn(bool) + Send + Sync>;

/// Where the gate reads the setting from.
pub(crate) struct EffortGate {
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
}

impl EffortGate {
    /// The requested level, once the setting allows a switch and the level is valid.
    async fn check(&self, data: &Value) -> anyhow::Result<ThinkingLevel> {
        let (cwd, agent_dir) = (self.cwd.clone(), self.agent_dir.clone());
        let allowed = tokio::task::spawn_blocking(move || {
            crate::settings::SettingsManager::create(&cwd, &agent_dir).get_repl_can_change_effort()
        })
        .await?;
        if !allowed {
            bail!(
                "model.set_effort is off: it changes what every later call costs. To allow it, set \
                 \"{REPL_CAN_CHANGE_EFFORT_SETTING}\": true in the global settings.json in the agent \
                 directory ({}), or ask the user to change the thinking level instead",
                self.agent_dir.join("settings.json").display()
            );
        }
        let level = data
            .get("level")
            .ok_or_else(|| anyhow!("model.set_effort requires a level"))?;
        serde_json::from_value(level.clone()).map_err(|_| {
            anyhow!("level must be one of off, minimal, low, medium, high, xhigh, max, got {level}")
        })
    }
}

/// Register `model.set_effort`. `apply` is the embedding's [`MODEL_APPLY_EFFORT_REQUEST_TYPE`]
/// handler, when it registered one.
pub(crate) fn register_effort_handler(
    handlers: &mut HostRequestHandlers,
    requests: &Arc<TurnBoundaryRequests>,
    gate: EffortGate,
    observer: Option<EffortObserver>,
) {
    let apply: Option<HostHandlerFn> = handlers.get(MODEL_APPLY_EFFORT_REQUEST_TYPE).cloned();
    // Weak: the kernel holds its handlers for its whole life.
    let requests: Weak<TurnBoundaryRequests> = Arc::downgrade(requests);
    let gate = Arc::new(gate);
    handlers.register(
        "model.set_effort",
        host_handler(move |payload| {
            let (requests, gate, apply, observer) = (
                requests.clone(),
                Arc::clone(&gate),
                apply.clone(),
                observer.clone(),
            );
            async move {
                let outcome = switch(&requests, &gate, apply.as_ref(), &payload.data).await;
                if let Some(observer) = &observer {
                    observer(outcome.is_ok());
                }
                outcome
            }
        }),
    );
}

#[tracing::instrument(skip_all)]
async fn switch(
    requests: &Weak<TurnBoundaryRequests>,
    gate: &EffortGate,
    apply: Option<&HostHandlerFn>,
    data: &Value,
) -> anyhow::Result<Value> {
    let level = gate.check(data).await?;
    if let Some(apply) = apply {
        return apply(HostRequestPayload {
            data: json!({ "level": level }),
        })
        .await;
    }
    let Some(runtime) = requests.upgrade().and_then(|requests| requests.bound()) else {
        bail!("the session is still starting; try again");
    };
    runtime.agent.set_thinking_level(level).await;
    let effective = runtime.agent.state().await.thinking_level;
    let wire = serde_json::to_value(effective)?;
    if let Some(name) = wire.as_str() {
        runtime
            .session
            .lock()
            .await
            .append_thinking_level_change(name)?;
    }
    Ok(json!({ "thinkingLevel": wire }))
}

#[cfg(test)]
mod tests;
