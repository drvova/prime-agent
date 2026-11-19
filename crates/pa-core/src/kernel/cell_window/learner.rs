//! One project's live learner: the estimator behind a lock, loaded and saved by
//! a background worker so no cell's start or result path ever waits on disk.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use super::estimator::{CellDurationEstimator, EstimatorState, LearnedWindow, COLD_WINDOW};
use super::store::DiskStore;

/// Persists one project's learned cell-duration state for [`CellWindowLearner`].
///
/// The learner calls `load` once and then `save` with each newer state, always
/// from its own background worker task and never from a cell's start or result
/// path, so an implementation may take as long as its IO takes. Implementations
/// do blocking IO off the async threads and never fail outward: a missing,
/// unreadable, or corrupt state loads as `None` (the learner starts cold), and a
/// failed save is logged and dropped (the next save carries a newer state).
pub(crate) trait CellWindowStore: Send + Sync + 'static {
    fn load(&self) -> impl Future<Output = Option<EstimatorState>> + Send;
    fn save(&self, state: EstimatorState) -> impl Future<Output = ()> + Send;
}

/// Cells that end before the stored state has loaded wait here, at most this
/// many, and are replayed onto it.
const MAX_PENDING: usize = 64;

enum Slot {
    /// The worker has not loaded the stored state yet: the cold window applies.
    Loading(Vec<Duration>),
    Ready(CellDurationEstimator),
}

/// What the worker has done, for tests to wait on.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct WorkerProgress {
    loaded: bool,
    saved: Option<EstimatorState>,
}

/// The learned yield window of one project (see [`super::CellWindow`]).
pub(crate) struct CellWindowLearner<S: CellWindowStore = DiskStore> {
    store: Arc<S>,
    slot: Arc<Mutex<Slot>>,
    /// The newest state to persist; the worker saves whatever is newest when it
    /// gets to it, so a slow disk coalesces saves instead of queueing them.
    latest: watch::Sender<Option<EstimatorState>>,
    /// The worker's receiving end, until [`Self::prime`] starts it.
    unstarted: Mutex<Option<watch::Receiver<Option<EstimatorState>>>>,
    progress: Arc<watch::Sender<WorkerProgress>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<S: CellWindowStore> CellWindowLearner<S> {
    pub(crate) fn new(store: S) -> Self {
        let (latest, receiver) = watch::channel(None);
        Self {
            store: Arc::new(store),
            slot: Arc::new(Mutex::new(Slot::Loading(Vec::new()))),
            latest,
            unstarted: Mutex::new(Some(receiver)),
            progress: Arc::new(watch::Sender::new(WorkerProgress::default())),
        }
    }

    /// Start the background worker (once): it loads the stored state, then
    /// saves each newer one. Called when the session's kernel is first ensured,
    /// so the load runs while the kernel boots. Needs a Tokio runtime; without
    /// one the learner stays cold.
    pub(crate) fn prime(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(mut receiver) = lock(&self.unstarted).take() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let slot = Arc::clone(&self.slot);
        let progress = Arc::clone(&self.progress);
        runtime.spawn(async move {
            let stored = store.load().await;
            let replayed = {
                let mut slot = lock(&slot);
                let pending = match &mut *slot {
                    Slot::Loading(pending) => std::mem::take(pending),
                    Slot::Ready(_) => Vec::new(),
                };
                let mut estimator = stored
                    .map(CellDurationEstimator::from_state)
                    .unwrap_or_default();
                for duration in &pending {
                    estimator.observe(*duration);
                }
                *slot = Slot::Ready(estimator);
                estimator.state().filter(|_| !pending.is_empty())
            };
            progress.send_modify(|progress| progress.loaded = true);
            if let Some(state) = replayed {
                store.save(state).await;
                progress.send_modify(|progress| progress.saved = Some(state));
            }
            // Ends when the learner (the sender) is dropped and the newest state is saved.
            while receiver.changed().await.is_ok() {
                let state = *receiver.borrow_and_update();
                if let Some(state) = state {
                    store.save(state).await;
                    progress.send_modify(|progress| progress.saved = Some(state));
                }
            }
        });
    }

    /// The window for a cell starting now. Never waits: before the stored
    /// state loads it is the cold window.
    pub(crate) fn window(&self) -> LearnedWindow {
        match &*lock(&self.slot) {
            Slot::Loading(_) => LearnedWindow::Cold(COLD_WINDOW),
            Slot::Ready(estimator) => estimator.window(),
        }
    }

    /// Learn from one finished cell. Never waits: the save is handed to the
    /// worker.
    pub(crate) fn observe(&self, duration: Duration) {
        let state = match &mut *lock(&self.slot) {
            Slot::Loading(pending) => {
                if pending.len() < MAX_PENDING {
                    pending.push(duration);
                }
                return;
            }
            Slot::Ready(estimator) => {
                estimator.observe(duration);
                estimator.state()
            }
        };
        self.latest.send_replace(state);
    }

    /// Wait until the worker has loaded the stored state.
    #[cfg(test)]
    pub(crate) async fn loaded(&self) {
        let mut progress = self.progress.subscribe();
        let _ = progress.wait_for(|progress| progress.loaded).await;
    }

    /// Wait until the worker has saved the current state.
    #[cfg(test)]
    pub(crate) async fn saved(&self) {
        let current = match &*lock(&self.slot) {
            Slot::Loading(_) => None,
            Slot::Ready(estimator) => estimator.state(),
        };
        let mut progress = self.progress.subscribe();
        let _ = progress
            .wait_for(|progress| progress.saved == current)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FAST_PROJECT: [Duration; 3] = [
        Duration::from_secs(2),
        Duration::from_secs(3),
        Duration::from_secs(2),
    ];

    /// What a project learned survives the session: a new learner over the same
    /// agent dir and project starts from it instead of cold.
    #[tokio::test]
    async fn the_learned_window_survives_a_reload() {
        let agent_dir = tempfile::tempdir().unwrap();
        let project = Path::new("/work/project");
        let first = CellWindowLearner::new(DiskStore::for_project(agent_dir.path(), project));
        first.prime();
        first.loaded().await;
        for duration in FAST_PROJECT {
            first.observe(duration);
        }
        first.saved().await;

        let second = CellWindowLearner::new(DiskStore::for_project(agent_dir.path(), project));
        second.prime();
        second.loaded().await;
        assert_eq!(
            [first.window(), second.window()],
            [LearnedWindow::Learned(Duration::from_secs(10)); 2]
        );
    }

    /// Loads at once, and never finishes a save.
    struct HangingSaves;

    impl CellWindowStore for HangingSaves {
        fn load(&self) -> impl Future<Output = Option<EstimatorState>> + Send {
            std::future::ready(None)
        }

        fn save(&self, _state: EstimatorState) -> impl Future<Output = ()> + Send {
            std::future::pending()
        }
    }

    /// The cell start and result paths never wait on persistence: with a store
    /// whose saves never return, each cell's result is learned and the next
    /// cell's window is armed at once.
    #[tokio::test]
    async fn a_hanging_save_never_delays_the_cell_path() {
        let learner = Arc::new(CellWindowLearner::new(HangingSaves));
        learner.prime();
        learner.loaded().await;
        let cells = Arc::clone(&learner);
        let runtime = tokio::runtime::Handle::current();
        let (armed_tx, armed_rx) = std::sync::mpsc::channel();
        // A plain thread inside the runtime's context: a cell path that waited
        // on the save would hang there, and the receive below times out
        // (without holding up the runtime's shutdown).
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            let armed: Vec<_> = FAST_PROJECT
                .iter()
                .map(|duration| {
                    cells.observe(*duration);
                    cells.window()
                })
                .collect();
            let _ = armed_tx.send(armed);
        });
        let armed = armed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the cell path waited on a hanging save");
        assert_eq!(
            armed,
            [
                LearnedWindow::Cold(COLD_WINDOW),
                LearnedWindow::Cold(COLD_WINDOW),
                LearnedWindow::Learned(Duration::from_secs(10)),
            ]
        );
    }
}
