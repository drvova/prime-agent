//! The `factory_activity` worker arm: the `/factory` view's daemon lane,
//! its checks and replies pa-core's (`session_engine::kernel_activity`).

//! One session-addressed command reaches this session's kernel factory
//! executor (the bridge registered by the pa-core session engine): the
//! payload's `action` rides the out-of-band kernel frame, and the kernel's
//! reply returns verbatim (the run registry stays kernel-owned); the
//! unscoped graph also carries the session's stored spec count. The
//! `run` action's model preflight (the allowlist pin, request auth)
//! happens in the session engine before the frame — a doomed run fails
//! before any child spawns.

use std::path::Path;

use pa_core::session_engine::kernel_activity::{
    factory_lane_enabled, stored_factory_spec_count, FactoryLaneRequest,
};
use serde_json::json;

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

/// The capabilities a hello advertises: the default set, minus the
/// `factory_activity` lane while the factory stays disabled (the opt-in
/// gate, `factory_lane_enabled`; the settings read is fresh per connection,
/// so a `/factory on` toggle surfaces on the next client start). Both hello
/// surfaces (the supervisor's and the worker's) advertise it.
pub(crate) fn advertised_server_capabilities(agent_dir: &Path) -> Vec<String> {
    let mut capabilities = crate::protocol::default_server_capabilities();
    if !factory_lane_enabled(agent_dir) {
        capabilities.retain(|capability| capability != "factory_activity");
    }
    capabilities
}

impl Worker {
    /// `factory_activity`: one factory action over this session's kernel.
    /// The arm mirrors `handle_kernel_bash_activity`: the engine owns the
    /// kernel scope, and a missing session answers with the same "Kernel
    /// is not running" refusal — except the unscoped graph, which still
    /// answers the session's stored specs (pa-core's `FactoryLaneRequest`).
    pub(crate) async fn handle_factory_activity(
        &self,
        payload: &serde_json::Value,
    ) -> DaemonResponse {
        if let Err(response) = self.require_created("factory_activity") {
            return response;
        }
        let request = match FactoryLaneRequest::parse(payload) {
            Ok(request) => request,
            Err(refusal) => return response_failure(None, "factory_activity", refusal, None),
        };
        let reply = match &self.agent_engine {
            Some(engine) => {
                engine
                    .factory_activity(
                        &request.action,
                        request.run_id.as_deref(),
                        request.spec_id.as_deref(),
                        request.timeout_ms,
                    )
                    .await
            }
            None => Err(anyhow::anyhow!(
                pa_types::daemon::KERNEL_NOT_RUNNING_MESSAGE
            )),
        };
        match request.settle(reply) {
            Ok(mut result) => {
                if request.lists_runs() {
                    match self.stored_factory_specs().await {
                        Ok(count) => result["factorySpecs"] = json!(count),
                        Err(error) => {
                            return response_failure(
                                None,
                                "factory_activity",
                                &format!("{error:#}"),
                                None,
                            )
                        }
                    }
                }
                response_success(None, "factory_activity", Some(result))
            }
            Err(error) => response_failure(None, "factory_activity", &format!("{error:#}"), None),
        }
    }

    /// This session's stored factory specs; an in-memory session has none.
    async fn stored_factory_specs(&self) -> anyhow::Result<usize> {
        let artifacts = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            core.store.as_ref().and_then(|store| {
                crate::scheduled_jobs::session_artifact_dir(&store.path, store.session_id())
            })
        };
        // Harness files may live on slow storage. Keep reads and JSON parsing
        // off the async executor used by the worker's dispatch loop.
        tokio::task::spawn_blocking(move || {
            artifacts.as_deref().map_or(0, stored_factory_spec_count)
        })
        .await
        .map_err(|error| anyhow::anyhow!("factory spec reader failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live session without a kernel (not booted yet, or passivated) still answers the unscoped
    /// graph: no runs, and its own stored specs from its local harness store — never the global
    /// store's.
    #[tokio::test]
    async fn a_live_session_without_a_kernel_answers_its_stored_specs() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let agent_dir = dir.path().join("agent");
        let worker = Worker::new(
            crate::worker::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "token".to_string(),
                worker_instance_id: String::new(),
                active_session_id: "no-kernel".to_string(),
                agent_dir: agent_dir.clone(),
                recovery_journal_path: dir.path().join("recovery.jsonl"),
                telemetry_disabled: Some(true),
                script: Some(serde_json::json!({ "responses": [] })),
            },
            None,
        );
        let created = worker
            .dispatch(
                "create",
                &serde_json::json!({ "cwd": "/tmp", "sessionDir": sessions }),
            )
            .await;
        assert!(created.success, "{created:?}");
        let artifacts = {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().expect("persisted session");
            crate::scheduled_jobs::session_artifact_dir(&store.path, store.session_id())
        };
        crate::test_support::write_factory_specs(
            &pa_core::refinement::get_local_harness_state_dir(artifacts.as_deref())
                .expect("local harness dir"),
            &["a", "b"],
        );
        crate::test_support::write_factory_specs(
            &pa_core::refinement::get_global_harness_state_dir(&agent_dir),
            &["shared"],
        );
        let graph = worker
            .dispatch(
                "factory_activity",
                &serde_json::json!({ "action": "graph" }),
            )
            .await;
        assert!(graph.success, "{graph:?}");
        assert_eq!(
            graph.data,
            Some(serde_json::json!({ "runs": [], "factorySpecs": 2 }))
        );
        #[cfg(unix)]
        {
            let harness = pa_core::refinement::get_local_harness_state_dir(artifacts.as_deref())
                .expect("local harness dir");
            let state_path = pa_core::refinement::get_harness_state_path(&harness);
            let payload = serde_json::json!({ "action": "graph" });
            let slow_graph = crate::test_support::assert_async_file_read(
                &state_path,
                worker.dispatch("factory_activity", &payload),
            )
            .await;
            assert_eq!(
                slow_graph.data,
                Some(serde_json::json!({ "runs": [], "factorySpecs": 2 }))
            );
        }
        let _ = worker.dispatch("kill", &serde_json::json!({})).await;
    }

    /// The opt-in default: no settings file, no factory lane.
    #[test]
    fn the_factory_lane_stays_unadvertised_until_enabled() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");

        let disabled = advertised_server_capabilities(&agent_dir);
        assert!(
            !disabled
                .iter()
                .any(|capability| capability == "factory_activity"),
            "the default factory gate stays off: {disabled:?}"
        );

        // `/factory on` persists `factory.enabled` in the settings file —
        // the exact shared key the lane advertisement reads.
        let settings = serde_json::json!({ "factory": { "enabled": true } });
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&settings).expect("serialize"),
        )
        .expect("write settings");
        let enabled = advertised_server_capabilities(&agent_dir);
        assert!(
            enabled
                .iter()
                .any(|capability| capability == "factory_activity"),
            "the enabled factory advertises its lane: {enabled:?}"
        );

        // `/factory off` re-reads as disabled: the lane leaves the
        // advertisement again.
        let off = serde_json::json!({ "factory": { "enabled": false } });
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&off).expect("serialize"),
        )
        .expect("write settings");
        let disabled_again = advertised_server_capabilities(&agent_dir);
        assert!(
            !disabled_again
                .iter()
                .any(|capability| capability == "factory_activity"),
            "the disabled factory lane leaves the advertisement: {disabled_again:?}"
        );
    }
}
