//! What a session reads from its Scheme kernel besides running cells: whether
//! the kernel is up, the work it still has in the background, the shell
//! commands the user can look at and stop, and the start ahead of first use.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use serde_json::Value;

use super::SchemeKernelProvisioner;
use crate::kernel::shared::BackgroundWorkSettledCallback;
use crate::tools::ipython::IpythonKernelProvisioner;

/// A session's live background work: every shell command whose end the
/// session may still have to hear of, and every cell that outlived its call.
/// The session's liveness probes read the count (goal continuation, the
/// daemon's idle eviction, the print mode's quiescence), and the last piece
/// ending fires the embedding's callback.
#[derive(Clone, Default)]
pub(crate) struct BackgroundWork(Arc<Work>);

#[derive(Default)]
struct Work {
    live: AtomicUsize,
    on_settled: Option<BackgroundWorkSettledCallback>,
}

/// One piece of live background work, counted until it is dropped.
pub(crate) struct LiveWork(Arc<Work>);

impl BackgroundWork {
    /// A count that tells `on_settled` each time it falls back to zero.
    pub(super) fn new(on_settled: Option<BackgroundWorkSettledCallback>) -> Self {
        BackgroundWork(Arc::new(Work {
            live: AtomicUsize::new(0),
            on_settled,
        }))
    }

    /// Count one more piece of work, until the returned hold is dropped. It
    /// is counted when this returns, so work entered before its cell ends is
    /// never missed by a probe that runs right after the cell.
    pub(crate) fn enter(&self) -> LiveWork {
        self.0.live.fetch_add(1, Ordering::SeqCst);
        LiveWork(Arc::clone(&self.0))
    }

    fn count(&self) -> usize {
        self.0.live.load(Ordering::SeqCst)
    }
}

impl Drop for LiveWork {
    fn drop(&mut self) {
        // The piece's completion notice has been handed over (or was never
        // due) by now, so a continuation that was waiting for it may run.
        if let (1, Some(on_settled)) = (
            self.0.live.fetch_sub(1, Ordering::SeqCst),
            &self.0.on_settled,
        ) {
            on_settled();
        }
    }
}

#[cfg(test)]
impl SchemeKernelProvisioner {
    /// The host handlers the kernel answers `(host-request ...)` from.
    pub(crate) fn host_handlers(&self) -> &crate::kernel::shared::HostRequestHandlers {
        &self.state.handlers
    }

    /// The session settings the kernel's cells and commands run under.
    pub(crate) fn environment(&self) -> &super::SessionEnvironment {
        &self.state.environment
    }
}

impl SchemeKernelProvisioner {
    /// Whether the session's engine has started and has not been killed. A
    /// start still in flight is not a running kernel.
    #[must_use]
    pub fn has_running_kernel(&self) -> bool {
        self.state
            .session
            .try_lock()
            .is_ok_and(|session| session.is_some())
    }

    /// The top-level names the session's cells have defined, in the order
    /// they were first defined. `None` when the kernel is not running, or
    /// its engine is busy with a cell (a cell in the background holds it
    /// until it ends).
    #[must_use]
    pub fn defined_names(&self) -> Option<Vec<String>> {
        let session = self.state.session.try_lock().ok()?.clone()?;
        let engine = session.engine.try_lock().ok()?;
        Some(
            engine
                .readable_globals(session.own_globals)
                .into_iter()
                .map(|name| name.resolve().to_string())
                .collect(),
        )
    }

    /// The session's live background jobs: shell commands whose completion
    /// notice may still be owed, and cells that outlived their call.
    #[must_use]
    pub fn background_job_count(&self) -> usize {
        self.state.environment.background.count()
    }

    /// Whether any background job is live (see [`Self::background_job_count`]).
    #[must_use]
    pub fn has_background_work(&self) -> bool {
        self.background_job_count() > 0
    }

    /// Start the kernel in the background, so the session's first cell does
    /// not wait for it. A start that fails is reported by the cell that
    /// needs the kernel next. Must be called from within a tokio runtime.
    pub fn prewarm(&self) {
        let starting = self.ensure(None, None);
        tokio::spawn(async move {
            if let Err(error) = starting.await {
                tracing::warn!("the Scheme kernel's early start failed: {error:#}");
            }
        });
    }

    /// Stop the kernel of a session that goes on (an idle one giving up what
    /// it holds): its engine, shell commands, MCP connections and factory
    /// control loops end as with `kill`, and the next cell starts a new
    /// kernel. Nothing brings back what the cells defined, so when they had
    /// defined anything the session is told through
    /// [`super::SchemeKernelOptions::on_state_lost`]. A kernel that is not
    /// running is left alone.
    pub async fn release(&self) {
        if !self.has_running_kernel() {
            return;
        }
        // A busy engine cannot be listed: whatever it holds is lost too.
        let defined = self.defined_names().is_none_or(|names| !names.is_empty());
        self.kill().await;
        if let (true, Some(on_state_lost)) = (defined, &self.state.on_state_lost) {
            on_state_lost();
        }
    }

    /// Kill the kernel from a place that cannot wait for it (a drop). Inside
    /// a tokio runtime the whole `kill` runs there. Outside one the engine's
    /// cell, shell commands and factory loops are stopped at once, and the
    /// MCP connections end when the kernel is dropped.
    pub fn kill_detached(&self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(self.kill());
            return;
        }
        // A start in flight holds the lock: its kernel ends with the
        // provisioner's last reference instead.
        let session = self
            .state
            .session
            .try_lock()
            .ok()
            .and_then(|mut session| session.take());
        if let Some(session) = session {
            session.cells.kill();
            session.jobs.kill_all();
            session.factory.shutdown();
        }
    }

    /// Look at or stop one of the kernel's shell commands without waiting
    /// for the engine: `list` answers `{activities}`, `tail` the newest
    /// `lines` lines of one command's output as `{activityId, tail}`, and
    /// `kill` stops one as `{activityId, killed}`. An idle kernel is not
    /// started for it.
    ///
    /// # Errors
    ///
    /// The kernel is not running, the action is unknown, `tail` or `kill`
    /// names no command or one the kernel does not have, or `lines` is
    /// outside 1..=200.
    pub async fn bash_activity(
        &self,
        action: &str,
        activity_id: Option<&str>,
        lines: usize,
    ) -> anyhow::Result<Value> {
        let session = self
            .state
            .session
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("Kernel is not running"))?;
        if !matches!(action, "list" | "tail" | "kill") {
            bail!("unknown bash activity action");
        }
        let activity_id = activity_id.map(str::trim).filter(|id| !id.is_empty());
        if action != "list" && activity_id.is_none() {
            bail!("bash activity tail/kill requires string activityId");
        }
        if action == "tail" && !(1..=200).contains(&lines) {
            bail!("lines must be an integer between 1 and 200");
        }
        session.jobs.activity(action, activity_id, lines)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
    use crate::kernel::scheme::{KernelStart, SchemeKernelOptions};
    use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
    use crate::tools::ipython::{ExecuteResult, ExecuteStatus, KernelExecuteOptions};

    /// How long a test waits for something that must happen before it fails.
    const PATIENCE: Duration = Duration::from_secs(30);

    async fn run(provisioner: &SchemeKernelProvisioner, code: &str) -> ExecuteResult {
        provisioner
            .ensure(None, None)
            .await
            .unwrap()
            .execute(
                code,
                KernelExecuteOptions {
                    signal: None,
                    timeout_ms: None,
                    on_stream: None,
                    on_late_sent_agent_message: None,
                },
            )
            .await
            .unwrap()
    }

    /// A released kernel takes its cells' definitions with it and says so;
    /// the next cell starts a new one. Releasing a kernel whose cells
    /// defined nothing tells no one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_released_kernel_starts_clean_and_reports_the_definitions_it_lost() {
        let lost = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(Mutex::new(Vec::new()));
        let (counted, noted) = (Arc::clone(&lost), Arc::clone(&starts));
        let provisioner = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            cwd: "/".into(),
            on_state_lost: Some(Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })),
            on_started: Some(Arc::new(move |start: KernelStart| {
                noted.lock().unwrap().push(start.succeeded);
            })),
            ..SchemeKernelOptions::default()
        });
        let before_any_cell = (
            provisioner.has_running_kernel(),
            provisioner.defined_names(),
        );
        run(&provisioner, "(define kept 7) (define (twice n) (* 2 n))").await;
        let defined = provisioner.defined_names();
        provisioner.release().await;
        let released = provisioner.has_running_kernel();
        let after = run(&provisioner, "kept").await.status;
        let redefined = provisioner.defined_names();
        provisioner.release().await;
        assert_eq!(
            (
                before_any_cell,
                defined,
                released,
                after,
                redefined,
                lost.load(Ordering::SeqCst),
                starts.lock().unwrap().clone(),
            ),
            (
                (false, None),
                Some(vec!["kept".to_string(), "twice".to_string()]),
                false,
                ExecuteStatus::Error,
                Some(Vec::new()),
                1,
                vec![true, true],
            )
        );
    }

    /// A cell that outlived its call is live background work, and holds the
    /// engine, until its completion notice has been handed over; then the
    /// settle callback fires.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_background_cell_is_live_work_until_its_notice_is_handed_over() {
        let dir = tempfile::tempdir().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let (settled, mut settlements) = tokio::sync::mpsc::unbounded_channel();
        let notices = Arc::new(AtomicUsize::new(0));
        let mut handlers = HostRequestHandlers::new();
        handlers.register("gate.wait", {
            let release = Arc::clone(&release);
            host_handler(move |_payload: HostRequestPayload| {
                let release = Arc::clone(&release);
                async move {
                    release.notified().await;
                    Ok(json!({}))
                }
            })
        });
        handlers.register("cell.completed", {
            let notices = Arc::clone(&notices);
            host_handler(move |_payload: HostRequestPayload| {
                notices.fetch_add(1, Ordering::SeqCst);
                async move { Ok(json!({})) }
            })
        });
        let seen = Arc::clone(&notices);
        let provisioner = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: dir.path().to_path_buf(),
            cell_window: CellWindow::for_session(CellWindowConfig {
                daemonless: false,
                configured_ms: Some(100),
                agent_dir: dir.path(),
                project_dir: dir.path(),
                on_armed: None,
            }),
            on_background_work_settled: Some(Arc::new(move || {
                // The notice was handed over before the work counted as settled.
                let _ = settled.send(seen.load(Ordering::SeqCst));
            })),
            ..SchemeKernelOptions::default()
        });
        let idle = provisioner.background_job_count();
        let long = run(&provisioner, r#"(host-request "gate.wait" (hash))"#).await;
        let while_running = (
            long.backgrounded_cell.is_some(),
            provisioner.background_job_count(),
            provisioner.has_background_work(),
            provisioner.defined_names(),
        );
        release.notify_one();
        let notices_at_settle = tokio::time::timeout(PATIENCE, settlements.recv())
            .await
            .unwrap();
        assert_eq!(
            (
                idle,
                while_running,
                notices_at_settle,
                provisioner.background_job_count()
            ),
            (0, (true, 1, true, None), Some(1), 0)
        );
    }
}
