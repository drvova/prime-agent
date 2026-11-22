//! The kernel's read-only `settings.*` host requests (the REPL's
//! `rlm.settings` client): the limits this session actually runs with.
//!
//! - `settings.session {}`: the session-owned facts. The model, its context
//!   window, the current thinking level, and the cell yield window (learned
//!   per project unless the user set one) are live; compaction, image model, web-search provider, and prompt-cache retention
//!   are the values the session armed at creation.
//! - `settings.host {}`: the embedding-owned facts (the RLM depth bound and
//!   its source, autonomous mode with its budget). The daemon worker
//!   registers the real handler; a session without one keeps the fallback
//!   here (the depth bound from the session file and settings, no
//!   autonomous runner).
//!
//! Replies are built field by field from typed values: no settings document
//! is passed through, so no credential, key, or token can reach the kernel.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use serde_json::{json, Value};

use super::turn_boundary::{TurnBoundaryRequests, TurnBoundaryRuntime};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The request type the daemon worker overrides with its own depth and autonomous state.
pub const SETTINGS_HOST_REQUEST_TYPE: &str = "settings.host";

/// The creation-time values the session armed (see the module docs).
#[derive(Clone)]
pub(crate) struct ArmedSettings {
    /// The kernel cell yield window policy; `cellTimeoutMs` reports the
    /// window it arms now (`None`: cells run to their end in the call).
    pub(crate) cell_window: crate::kernel::cell_window::CellWindow,
    pub(crate) compaction_enabled: bool,
    pub(crate) compaction_threshold_percent: f64,
    pub(crate) compaction_keep_recent_tokens: u64,
    pub(crate) compaction_agent_callable: bool,
    pub(crate) image_model: Option<String>,
    pub(crate) web_search_provider: String,
    pub(crate) cache_retention: Value,
    pub(crate) rlm_depth: u32,
    pub(crate) daemonless: bool,
}

/// Observes each answered `settings.session` (adoption telemetry).
pub(crate) type SettingsObserver = Arc<dyn Fn() + Send + Sync>;

/// The live model facts: the bound runtime's model, its context window, and
/// the thinking level the next call runs at (a routed override's when set).
pub(crate) async fn live_model_facts(runtime: &TurnBoundaryRuntime) -> Value {
    let thinking = match runtime.agent.model_override() {
        Some(route) => route.thinking_level,
        None => runtime.agent.state().await.thinking_level,
    };
    json!({
        "id": runtime.model_info.id,
        "provider": runtime.model_info.provider,
        "contextWindow": runtime.context_window,
        "thinkingLevel": thinking,
    })
}

/// Register `settings.session`, and the no-daemon `settings.host` fallback
/// unless the embedding already registered the real one.
pub(crate) fn register_settings_handlers(
    handlers: &mut HostRequestHandlers,
    requests: &Arc<TurnBoundaryRequests>,
    armed: ArmedSettings,
    host_fallback: HostFallback,
    observer: Option<SettingsObserver>,
) {
    // Weak: the kernel holds its handlers for its whole life.
    let requests: Weak<TurnBoundaryRequests> = Arc::downgrade(requests);
    let armed = Arc::new(armed);
    handlers.register(
        "settings.session",
        host_handler(move |_payload| {
            let requests = requests.clone();
            let armed = Arc::clone(&armed);
            let observer = observer.clone();
            async move {
                let Some(runtime) = requests.upgrade().and_then(|requests| requests.bound()) else {
                    anyhow::bail!("the session is still starting; try again");
                };
                let model = live_model_facts(&runtime).await;
                if let Some(observer) = &observer {
                    observer();
                }
                Ok(json!({
                    "model": model,
                    "cellTimeoutMs": armed.cell_window.current_ms(),
                    "compaction": {
                        "enabled": armed.compaction_enabled,
                        "thresholdPercent": armed.compaction_threshold_percent,
                        "keepRecentTokens": armed.compaction_keep_recent_tokens,
                        "agentCallable": armed.compaction_agent_callable,
                    },
                    "imageModel": armed.image_model,
                    "webSearchProvider": armed.web_search_provider,
                    "cacheRetention": armed.cache_retention,
                    "rlmDepth": armed.rlm_depth,
                    "daemonless": armed.daemonless,
                }))
            }
        }),
    );
    if handlers.get(SETTINGS_HOST_REQUEST_TYPE).is_none() {
        let fallback = Arc::new(host_fallback);
        handlers.register(
            SETTINGS_HOST_REQUEST_TYPE,
            host_handler(move |_payload| {
                let fallback = Arc::clone(&fallback);
                async move {
                    let (max_depth, source) = tokio::task::spawn_blocking(move || {
                        super::rlm_host::resolve_rlm_max_depth(
                            fallback.session_file.as_deref(),
                            None,
                            &fallback.cwd,
                            &fallback.agent_dir,
                        )
                    })
                    .await?;
                    Ok(json!({
                        "rlmMaxDepth": {"maxDepth": max_depth, "source": source},
                        "autonomous": null,
                    }))
                }
            }),
        );
    }
}

/// What the no-daemon `settings.host` fallback resolves the depth bound from.
pub(crate) struct HostFallback {
    pub(crate) session_file: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
}

#[cfg(test)]
mod tests;
