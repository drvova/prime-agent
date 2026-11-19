//! The `python_repl` cell yield window: how long a cell runs inside its tool
//! call before it continues in the background.
//!
//! Precedence, decided when each cell starts and fixed for that cell:
//!
//! 1. Daemonless sessions run every cell to its end (no window at all).
//! 2. The model's per-call `timeout` wins.
//! 3. A `pythonCellTimeoutMs` the user set in a settings file wins next (`0`
//!    still disables the window), unchanged on the wire.
//! 4. Otherwise the window is learned from this project's recent cells (see
//!    [`estimator`]), starting from the cold 60 s until a few cells are seen.
//!    The learned state is kept per project under the agent dir and loaded and
//!    saved by a background worker (see [`learner`]).

mod estimator;
mod learner;
mod store;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use estimator::LearnedWindow;
use learner::CellWindowLearner;

/// Where one cell's yield window came from (adoption counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellWindowSource {
    /// The model passed `timeout`.
    ModelTimeout,
    /// The user set `pythonCellTimeoutMs`.
    Setting,
    /// Learned from the project's recent cells.
    Learned,
    /// Nothing learned yet: the cold 60 s.
    ColdDefault,
}

/// Hears the source of each armed cell window.
pub(crate) type CellWindowObserver = Arc<dyn Fn(CellWindowSource) + Send + Sync>;

#[derive(Clone, Default)]
enum Policy {
    /// No window: every cell runs to its end in the call.
    #[default]
    RunToEnd,
    /// The user's `pythonCellTimeoutMs` (`0`: disabled).
    Configured(u64),
    Learned(Arc<CellWindowLearner>),
}

/// One session's cell yield window policy (see the module docs). The default
/// runs every cell to its end, as for a session that never set one.
#[derive(Clone, Default)]
pub struct CellWindow {
    policy: Policy,
    on_armed: Option<CellWindowObserver>,
}

/// What [`CellWindow::for_session`] resolves the policy from.
pub(crate) struct CellWindowConfig<'a> {
    pub(crate) daemonless: bool,
    /// `pythonCellTimeoutMs` when the user set it in a settings file.
    pub(crate) configured_ms: Option<u64>,
    pub(crate) agent_dir: &'a Path,
    /// The session's working directory: the project the window is learned for.
    pub(crate) project_dir: &'a Path,
    pub(crate) on_armed: Option<CellWindowObserver>,
}

impl CellWindow {
    /// Daemonless sessions keep `await bash()` unbounded by design (#3523) and
    /// have no background lane to wake them when a cell ends, so their cells
    /// always run to the end inside the call; the window applies only in
    /// daemon-backed sessions.
    pub(crate) fn for_session(config: CellWindowConfig<'_>) -> Self {
        let policy = match (config.daemonless, config.configured_ms) {
            (true, _) => Policy::RunToEnd,
            (false, Some(ms)) => Policy::Configured(ms),
            (false, None) => Policy::Learned(Arc::new(CellWindowLearner::new(
                store::DiskStore::for_project(config.agent_dir, config.project_dir),
            ))),
        };
        Self {
            policy,
            on_armed: config.on_armed,
        }
    }

    /// Start loading the learned state in the background (a no-op for other
    /// policies). Called when the kernel is ensured, ahead of the first cell.
    pub(crate) fn prime(&self) {
        match &self.policy {
            Policy::Learned(learner) => learner.prime(),
            Policy::RunToEnd | Policy::Configured(_) => {}
        }
    }

    /// The yield window for a cell starting now, in milliseconds (`None`: the
    /// cell runs to its end). `model_timeout_ms` is the model's own `timeout`.
    /// Never waits on disk.
    pub(crate) fn arm(&self, model_timeout_ms: Option<u64>) -> Option<u64> {
        let (window_ms, source) = match (&self.policy, model_timeout_ms) {
            (Policy::RunToEnd | Policy::Configured(0), _) => return None,
            (Policy::Configured(_) | Policy::Learned(_), Some(ms)) => {
                (ms, CellWindowSource::ModelTimeout)
            }
            (Policy::Configured(ms), None) => (*ms, CellWindowSource::Setting),
            (Policy::Learned(learner), None) => match learner.window() {
                LearnedWindow::Cold(window) => (millis(window), CellWindowSource::ColdDefault),
                LearnedWindow::Learned(window) => (millis(window), CellWindowSource::Learned),
            },
        };
        if let Some(on_armed) = &self.on_armed {
            on_armed(source);
        }
        Some(window_ms)
    }

    /// The window a cell without its own `timeout` gets now, as
    /// `settings.session` reports it (`cellTimeoutMs`): `None` when cells run
    /// to their end, the configured value (`0` included) when the user set one.
    pub(crate) fn current_ms(&self) -> Option<u64> {
        match &self.policy {
            Policy::RunToEnd => None,
            Policy::Configured(ms) => Some(*ms),
            Policy::Learned(learner) => match learner.window() {
                LearnedWindow::Cold(window) | LearnedWindow::Learned(window) => {
                    Some(millis(window))
                }
            },
        }
    }

    /// Learn from one user cell that ended with a value or an exception, in its
    /// window or later in the background. Never waits on disk.
    pub(crate) fn observe(&self, duration: Duration) {
        match &self.policy {
            Policy::Learned(learner) => learner.observe(duration),
            Policy::RunToEnd | Policy::Configured(_) => {}
        }
    }
}

fn millis(window: Duration) -> u64 {
    u64::try_from(window.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Every policy and model-timeout combination arms the window precedence
    /// says, and reports where it came from: daemonless never arms, the
    /// model's `timeout` beats both the user's setting and the learned window,
    /// a zero setting disables the window, and a project with fast cells gets
    /// its learned window.
    #[tokio::test]
    async fn an_explicit_timeout_wins_over_the_setting_and_the_learned_window() {
        let agent_dir = tempfile::tempdir().unwrap();
        let sources = Arc::new(Mutex::new(Vec::new()));
        let window = |daemonless, configured_ms| {
            let sources = Arc::clone(&sources);
            CellWindow::for_session(CellWindowConfig {
                daemonless,
                configured_ms,
                agent_dir: agent_dir.path(),
                project_dir: Path::new("/work/project"),
                on_armed: Some(Arc::new(move |source| sources.lock().unwrap().push(source))),
            })
        };
        let learned = window(false, None);
        let Policy::Learned(project) = &learned.policy else {
            panic!("no setting and a daemon: the window is learned");
        };
        project.prime();
        project.loaded().await;
        for seconds in [2, 3, 2] {
            learned.observe(Duration::from_secs(seconds));
        }
        let armed = [
            window(true, None).arm(Some(5_000)),
            window(true, Some(90_000)).arm(None),
            window(false, Some(0)).arm(Some(5_000)),
            window(false, Some(90_000)).arm(Some(5_000)),
            window(false, Some(90_000)).arm(None),
            learned.arm(Some(5_000)),
            learned.arm(None),
            window(false, None).arm(None),
        ];
        assert_eq!(
            (armed, sources.lock().unwrap().clone()),
            (
                [
                    None,
                    None,
                    None,
                    Some(5_000),
                    Some(90_000),
                    Some(5_000),
                    Some(10_000),
                    Some(60_000),
                ],
                vec![
                    CellWindowSource::ModelTimeout,
                    CellWindowSource::Setting,
                    CellWindowSource::ModelTimeout,
                    CellWindowSource::Learned,
                    CellWindowSource::ColdDefault,
                ]
            )
        );
    }
}
