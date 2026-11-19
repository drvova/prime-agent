//! The kernel's opt-in `model.set_effort(...)`, worker half: pa-core gates the request (the
//! `replCanChangeEffort` setting, the level) and hands the validated level to the
//! `model.apply_effort` handler registered here, which applies it through the worker's own
//! thinking switch (clamping, the durable row, the roster push) — the same path as the
//! `set_thinking_level` command, minus the settings default.
use super::AgentSessionEngine;

/// The worker's thinking switch for one validated level: the effective level, or why the session
/// has none.
pub(crate) type KernelEffortSwitch = std::sync::Arc<
    dyn Fn(
            pa_types::ai::ModelThinkingLevel,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

impl AgentSessionEngine {
    /// Wire the worker's thinking switch (set once the worker's roster queue exists).
    pub(crate) fn set_kernel_effort_switch(&self, switch: KernelEffortSwitch) {
        *self
            .kernel_effort_switch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(switch);
    }

    /// Register `model.apply_effort` when the worker wired its switch.
    pub(super) fn register_kernel_effort_handler(
        &self,
        handlers: &mut pa_core::kernel::shared::HostRequestHandlers,
    ) {
        let Some(switch) = self
            .kernel_effort_switch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        handlers.register(
            pa_core::session_engine::effort_switch::MODEL_APPLY_EFFORT_REQUEST_TYPE,
            pa_core::kernel::shared::host_handler(move |payload| {
                let switch = std::sync::Arc::clone(&switch);
                async move {
                    let level = payload
                        .data
                        .get("level")
                        .and_then(serde_json::Value::as_str)
                        .and_then(pa_ai::models::thinking_level_from_str)
                        .ok_or_else(|| {
                            anyhow::anyhow!("model.apply_effort requires a valid level")
                        })?;
                    let effective = switch(level).await.map_err(anyhow::Error::msg)?;
                    Ok(serde_json::json!({ "thinkingLevel": effective }))
                }
            }),
        );
    }
}
