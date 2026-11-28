//! The agents view's kernel activity on the runtime daemon (8d-4), the old
//! daemon's worker and supervisor commands of the same names, each answering
//! the old response's `data` or its refusal text, the checks and shapes
//! pa-core's (`session_engine::kernel_activity`, `kernel::persistent_shells`):
//!
//! - `session.kernel_bash {session}` (`list_kernel_bash`): `{activities}`, the
//!   open session's kernel shell commands;
//!   `session.kernel_bash_tail {session, activityId, lines?}`
//!   (`tail_kernel_bash`): `{id, tail}`, the newest `lines` (1 to 200, 50 by
//!   default) of one command's output; the call
//!   `session.kill_kernel_bash {session, activityId}` (`kill_kernel_bash`):
//!   `{id, killed}`. A kernel that is not running is not started for them
//!   (`Kernel is not running`).
//! - The call `session.factory_activity {session, action, runId?, specId?,
//!   timeoutMs?}` (`factory_activity`): the kernel factory executor's reply to
//!   one `/factory` action; the unscoped graph also carries the session's
//!   stored specs as `factorySpecs`, and answers `{runs: []}` with them while
//!   the session's kernel is not running.
//! - `persistent_shells` (`list_persistent_shells`): `{activities}`, every
//!   persistent shell machine-wide with its assigned `sessionId`;
//!   `persistent_shells.tail {shellId, lines}` (`tail_persistent_shell`):
//!   `{shellId, tail}`; the call `persistent_shells.kill {shellId}`
//!   (`kill_persistent_shell`): `{shellId}` once its group ended. The reads and
//!   the kill's grace run on a blocking thread.

use anyhow::Context as _;
use pa_core::session_engine::kernel_activity::{
    stored_factory_spec_count, FactoryLaneRequest, KernelBashAction, KernelBashRequest,
};
use serde_json::{json, Value};

use super::{string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.kernel_bash`, `session.kernel_bash_tail` and
    /// `session.kill_kernel_bash` (see the module documentation).
    pub(super) async fn kernel_bash(
        &self,
        args: &Value,
        action: KernelBashAction,
    ) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let request = KernelBashRequest::parse(action, args).map_err(anyhow::Error::msg)?;
        let live = self.runtime.built_engine(&id).await?;
        let engine = std::sync::Arc::clone(&live.engine);
        let (activity_id, lines) = (request.activity_id.clone(), request.lines);
        let fields = live
            .runtime
            .spawn(async move {
                engine
                    .bash_activity(action.kernel_action(), activity_id.as_deref(), lines)
                    .await
            })
            .await
            .context("the session's runtime ended during the read")??;
        Ok(request.reply(fields))
    }

    /// `session.factory_activity` (see the module documentation).
    pub(super) async fn factory_activity(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let request = FactoryLaneRequest::parse(args).map_err(anyhow::Error::msg)?;
        let live = self.runtime.built_engine(&id).await?;
        let engine = std::sync::Arc::clone(&live.engine);
        let asked = request.clone();
        let reply = live
            .runtime
            .spawn(async move {
                engine
                    .factory_activity(
                        &asked.action,
                        asked.run_id.as_deref(),
                        asked.spec_id.as_deref(),
                        asked.timeout_ms,
                    )
                    .await
            })
            .await
            .context("the session's runtime ended during the factory activity")?;
        let mut result = request.settle(reply)?;
        if request.lists_runs() {
            // The session's own stored specs, under its artifact dir.
            let persistence = live.engine.session.shared_persistence();
            let artifacts = live
                .runtime
                .spawn(async move { persistence.lock().await.get_session_artifact_dir() })
                .await
                .context("the session's runtime ended during the read")?;
            let specs = tokio::task::spawn_blocking(move || {
                artifacts.as_deref().map_or(0, stored_factory_spec_count)
            })
            .await
            .map_err(|error| anyhow::anyhow!("factory spec reader failed: {error}"))?;
            result["factorySpecs"] = json!(specs);
        }
        Ok(result)
    }

    /// `persistent_shells` (see the module documentation).
    pub(super) async fn persistent_shells(&self) -> anyhow::Result<Value> {
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || pa_core::kernel::persistent_shells::list(&agent_dir))
            .await
            .context("the persistent shells' read panicked")
    }

    /// `persistent_shells.tail` (see the module documentation).
    pub(super) async fn persistent_shell_tail(&self, args: &Value) -> anyhow::Result<Value> {
        let shell = string_arg(args, "shellId")?;
        let lines = args
            .get("lines")
            .and_then(Value::as_u64)
            .and_then(|lines| usize::try_from(lines).ok())
            .context("the `lines` argument must be a count")?;
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || {
            pa_core::kernel::persistent_shells::tail(&agent_dir, &shell, lines)
        })
        .await
        .context("the persistent shell's read panicked")?
        .map_err(anyhow::Error::msg)
    }

    /// `persistent_shells.kill` (see the module documentation).
    pub(super) async fn kill_persistent_shell(&self, args: &Value) -> anyhow::Result<Value> {
        let shell = string_arg(args, "shellId")?;
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || {
            pa_core::kernel::persistent_shells::kill(&agent_dir, &shell)
        })
        .await
        .context("the persistent shell's kill panicked")?
        .map_err(anyhow::Error::msg)
    }
}
