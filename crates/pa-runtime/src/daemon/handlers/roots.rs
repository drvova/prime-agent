//! `rlm.create_session` on the runtime daemon ([`RootSessions`]): a depth-0
//! session's kernel creates a sibling root, as the old supervisor's
//! `create_session` host did.
//!
//! - **Model.** The selector asked for, else the creator's, resolved against
//!   the credential-backed catalog and refused outside the creator's settings'
//!   `allowedModels`; a thinking level the model does not support is refused
//!   (pa-core's `rlm_child_model`, the supervisor's checks).
//! - **Session.** Created as a client's `session.create` creates one (its
//!   lease, kept options, open record and roster entry) in the daemon's
//!   session dir, in the requested working directory, on the creator's minimal
//!   environment (`rlm_root_session_launch_env`), its header naming the
//!   creator's transcript at depth 0: the agents view and `sessions.list` show
//!   where it came from, and it is no child of the creator's, so nothing the
//!   creator's family goes through (a crash, a replacement, a stop) touches it.
//! - **Task.** Named when a name was asked for, then prompted with the task as
//!   its owner; a root whose prompt is refused is stopped again. The handle is
//!   answered once the task is queued, never after its turn.

use pa_core::session_engine::rlm_child_model::{
    assert_thinking_supported, resolve_child_model, rlm_root_session_launch_env,
};
use pa_core::session_engine::rlm_host::RlmCreateSessionHandle;
use pa_types::event_log::AgentInput;
use serde_json::json;

use super::{lifecycle, update, RuntimeHandlers};
use crate::session::SessionBuilder;
use crate::subagents::{RootSessionRequest, RootSessions};
use crate::wire::ConnectionId;

impl<B: SessionBuilder> RootSessions for RuntimeHandlers<B> {
    fn create_root(
        &self,
        request: RootSessionRequest,
    ) -> pa_agent::BoxFut<'_, anyhow::Result<RlmCreateSessionHandle>> {
        Box::pin(async move {
            anyhow::ensure!(!self.stop.stopping(), "{}", lifecycle::SHUTTING_DOWN);
            // A create like a client's, behind the update gate until answered.
            let _mutation = update::admit(&self.update, "create")?;
            let model = {
                let agent_dir = self.agent_dir.clone();
                let (cwd, reference, creator_model, thinking) = (
                    request.creator_cwd.clone(),
                    request.model.clone(),
                    request.creator_model.clone(),
                    request.thinking.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    let allowlist = pa_core::models::load_model_allowlist(&cwd, &agent_dir);
                    let model = resolve_child_model(
                        &agent_dir,
                        reference.as_deref(),
                        creator_model.as_deref(),
                        "top-level session",
                        &allowlist,
                    )?;
                    assert_thinking_supported(&agent_dir, thinking.as_deref(), &model)?;
                    anyhow::Ok(model)
                })
                .await
                .map_err(|_| anyhow::anyhow!("the model resolution's thread panicked"))??
            };
            let launch_env = rlm_root_session_launch_env(&model, |key| {
                request
                    .creator_env
                    .get(key)
                    .cloned()
                    .or_else(|| pa_types::session_scope::var(key).ok())
            });
            // Each null is a choice not made (`SessionOptions::from_json`).
            let (provider, model_id) = model
                .split_once('/')
                .map_or((None, model.as_str()), |(provider, id)| {
                    (Some(provider), id)
                });
            let thinking = match request.thinking {
                Some(level) => Some(json!(level)),
                None => request
                    .creator_thinking
                    .map(serde_json::to_value)
                    .transpose()?,
            };
            let options = json!({
                "provider": provider,
                "model": model_id,
                "thinking": thinking,
                "parent": request
                    .creator_file
                    .as_ref()
                    .map(|creator| json!({ "path": creator, "rlmDepth": 0 })),
            });
            let created = self
                .create(
                    json!({
                        "cwd": request.cwd,
                        "launchEnv": launch_env,
                        "options": options,
                    }),
                    // No client asked, as the update boot's restore opens its rows: a
                    // root is never ephemeral, so no connection owns it.
                    ConnectionId(0),
                )
                .await?;
            let id = created["session"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("the created root has no id: {created}"))?
                .to_string();
            let prompted = async {
                if let Some(name) = &request.name {
                    self.rename(&json!({ "session": id, "name": name })).await?;
                }
                self.publish_input(
                    &id,
                    AgentInput {
                        text: request.prompt,
                        images: Vec::new(),
                        priority: None,
                    },
                )
                .await
            }
            .await;
            if let Err(error) = prompted {
                if let Err(stop) = self.stop(&json!({ "session": id })).await {
                    tracing::warn!("the unprompted root {id} did not stop cleanly: {stop:#}");
                }
                return Err(error.context(format!("prompt the created root session {id}")));
            }
            Ok(RlmCreateSessionHandle {
                active_session_id: id.clone(),
                session_id: id.clone(),
                name: request.name.unwrap_or_else(|| id.clone()),
                session_file: created["path"].as_str().unwrap_or_default().to_string(),
                model,
            })
        })
    }
}
