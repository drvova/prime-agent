//! The session's live-input reload (`/reload`'s daemon half): re-read the
//! MCP manager's settings and auth state — the pieces another process (a
//! `/mcp` key flow) can change under a running session. Request auth
//! needs no rebind: the live session's stream re-resolves it from the
//! credential store on every provider call (the TS streamFn), so a
//! rotated credential serves the next request with or without a reload.
use super::AgentSessionEngine;

impl AgentSessionEngine {
    /// `/reload`'s session half: re-read the MCP manager's live inputs.
    /// A failed auth-store read fails the reload (the manager keeps its
    /// previous credentials) instead of answering a success the session
    /// did not apply.
    pub(crate) fn reload_live_inputs(&self) -> Result<(), String> {
        let mut manager = self
            .mcp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let auth_reload = manager.reload_auth_storage();
        manager.refresh();
        auth_reload
    }
}

impl AgentSessionEngine {
    /// `/reload`'s resource half: the live session re-reads its skills,
    /// prompt templates, and context files (the same re-read the kernel's
    /// `rlm.reload()` runs). A session not built yet has nothing to re-read:
    /// its first build discovers them fresh.
    ///
    /// # Errors
    ///
    /// The discovery failing (an unreadable settings file).
    pub(crate) async fn reload_resources(&self) -> anyhow::Result<()> {
        let engine = self.session.lock().await.clone();
        if let Some(engine) = engine {
            engine.resource_reload.reload().await?;
        }
        Ok(())
    }

    /// Register the worker's `settings.host` kernel host request (the REPL's
    /// `settings.get()`): the RLM depth bound with its source and the
    /// autonomous runner's status and budget. The turn loop holds the
    /// autonomous lock across its own awaits, so a busy lock answers the
    /// status as unavailable instead of parking the cell.
    pub(super) fn register_settings_host_handler(
        &self,
        handlers: &mut pa_core::kernel::shared::HostRequestHandlers,
    ) {
        let engine = self
            .self_weak
            .lock()
            .expect("engine self weak lock")
            .clone();
        handlers.register(
            pa_core::session_engine::settings_view::SETTINGS_HOST_REQUEST_TYPE,
            pa_core::kernel::shared::host_handler(move |_payload| {
                let engine = engine.clone();
                async move {
                    let Some(engine) = engine.and_then(|weak| weak.upgrade()) else {
                        anyhow::bail!("the session ended before the request could be served");
                    };
                    let depth = crate::engine::SessionEngine::rlm_max_depth_status(&*engine);
                    let autonomous = match engine.autonomous.try_lock() {
                        Ok(state) => {
                            serde_json::to_value(pa_core::autonomous::autonomous_status(&state))?
                        }
                        Err(_) => serde_json::Value::Null,
                    };
                    Ok(serde_json::json!({"rlmMaxDepth": depth, "autonomous": autonomous}))
                }
            }),
        );
    }
}
