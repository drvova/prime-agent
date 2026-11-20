//! Per-project learning from what users do with rewrites: a recency-weighted estimate of how
//! often a rewrite is accepted (sent as is: 1, edited: 1/2, discarded: 0) sets how many reads the
//! next run starts with. A project whose rewrites keep getting discarded starts deeper; one whose
//! rewrites go out as is starts lean. Nothing is configurable.
//!
//! The estimate is an exponentially weighted mean (the #129 cell-window pattern): the newest
//! outcomes dominate, so a project whose needs change is followed within a few rewrites. It is
//! stored as one small JSON file per project under `<agent dir>/better-learning/`, named by a hash
//! of the project directory (the path itself is never stored or reported). The file is read once,
//! by the first run (which already runs in the background), and written fire-and-forget on a
//! blocking thread after each outcome: no paint, keypress, or submit waits on it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Weight of the newest outcome, in percent: after five outcomes the oldest weighs under a fifth
/// (`0.7^5`). Integer arithmetic, so decisions replay exactly.
const RECENCY_WEIGHT_PCT: i64 = 30;

/// Full acceptance, in thousandths.
const ACCEPTED: u32 = 1000;

/// Outcomes seen before the learned budget replaces the cold one.
const MIN_SAMPLES: u32 = 3;

/// Reads the first pass starts with before anything is learned.
pub(crate) const COLD_BUDGET: u32 = 8;

/// The learned first-pass budget spans `[LEAN_BUDGET, LEAN_BUDGET + DEPTH_SPAN]`: always accepted
/// rewrites start lean, always discarded ones start deep.
const LEAN_BUDGET: u32 = 4;
const DEPTH_SPAN: u32 = 8;

/// Bumped when the stored estimate changes meaning; an older file then loads as cold.
const SCHEMA: u64 = 1;
const DIR: &str = "better-learning";

/// One outcome the learner folds in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    SentAsIs,
    Edited,
    Discarded,
}

impl Signal {
    /// Thousandths of acceptance.
    fn score(self) -> u32 {
        match self {
            Self::SentAsIs => ACCEPTED,
            Self::Edited => ACCEPTED / 2,
            Self::Discarded => 0,
        }
    }
}

/// The estimate's whole persisted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Estimate {
    /// Outcomes observed, saturating.
    pub(crate) samples: u32,
    /// Exponentially weighted mean acceptance, in thousandths (`0..=1000`).
    pub(crate) acceptance: u32,
}

impl Estimate {
    const COLD: Self = Self {
        samples: 0,
        acceptance: ACCEPTED,
    };

    fn is_valid(&self) -> bool {
        self.acceptance <= ACCEPTED
    }

    fn observe(self, signal: Signal) -> Self {
        let acceptance = if self.samples == 0 {
            signal.score()
        } else {
            let (old, new) = (i64::from(self.acceptance), i64::from(signal.score()));
            let mean = old + (new - old) * RECENCY_WEIGHT_PCT / 100;
            u32::try_from(mean.clamp(0, i64::from(ACCEPTED))).unwrap_or(ACCEPTED)
        };
        Self {
            samples: self.samples.saturating_add(1),
            acceptance,
        }
    }

    /// The reads the next run's first pass gets.
    pub(crate) fn first_pass_budget(&self) -> u32 {
        if self.samples < MIN_SAMPLES {
            return COLD_BUDGET;
        }
        let rejected = ACCEPTED - self.acceptance.min(ACCEPTED);
        LEAN_BUDGET + (rejected * DEPTH_SPAN + ACCEPTED / 2) / ACCEPTED
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u64,
    estimate: Estimate,
}

/// One project's learner, shared by a session's runs and outcome reports.
pub(crate) struct ProjectLearning {
    /// `None`: an ephemeral session learns in memory only.
    path: Option<PathBuf>,
    /// `None` until the first run read the stored estimate.
    estimate: Mutex<Option<Estimate>>,
}

impl ProjectLearning {
    pub(crate) fn new(agent_dir: Option<&Path>, project_dir: &Path) -> Self {
        let path = agent_dir.map(|agent_dir| {
            let digest = Sha256::digest(project_dir.to_string_lossy().as_bytes());
            let name = digest[..8]
                .iter()
                .fold(String::with_capacity(16), |mut name, byte| {
                    let _ = write!(name, "{byte:02x}");
                    name
                });
            agent_dir.join(DIR).join(format!("{name}.json"))
        });
        Self {
            path,
            estimate: Mutex::new(None),
        }
    }

    /// The estimate, reading the stored one first if no run has yet. Only runs call this, and
    /// runs never sit on a paint or submit path.
    pub(crate) async fn estimate(&self) -> Estimate {
        if let Some(estimate) = *self.lock() {
            return estimate;
        }
        let stored = match self.path.clone() {
            None => None,
            Some(path) => tokio::task::spawn_blocking(move || {
                // Missing, unreadable, corrupt, older-schema, or out of range: start cold.
                let raw = std::fs::read(&path).ok()?;
                let stored: Stored = serde_json::from_slice(&raw).ok()?;
                (stored.schema == SCHEMA && stored.estimate.is_valid()).then_some(stored.estimate)
            })
            .await
            .ok()
            .flatten(),
        };
        *self.lock().get_or_insert(stored.unwrap_or(Estimate::COLD))
    }

    /// Fold one outcome in and persist the new estimate fire-and-forget. An outcome always
    /// follows one of this session's runs, which read the stored estimate; one that somehow
    /// precedes every run starts from the cold estimate.
    pub(crate) fn observe(&self, signal: Signal) {
        let next = {
            let mut estimate = self.lock();
            let next = estimate.unwrap_or(Estimate::COLD).observe(signal);
            *estimate = Some(next);
            next
        };
        let (Some(path), Ok(runtime)) = (self.path.clone(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        runtime.spawn_blocking(move || {
            if let Err(error) = save(&path, next) {
                tracing::debug!(%error, "better learning not saved");
            }
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Estimate>> {
        self.estimate.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn save(path: &Path, estimate: Estimate) -> std::io::Result<()> {
    let body = serde_json::to_string(&Stored {
        schema: SCHEMA,
        estimate,
    })?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Temp + rename with a per-write name: sessions sharing a project never share a temp file.
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp, body)?;
    crate::platform::rename_onto(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project's first-pass budgets after each outcome: cold until three outcomes, lean while
    /// rewrites go out as is, deeper as they get discarded, and back as they are accepted again.
    #[test]
    fn the_first_pass_budget_follows_a_whole_sequence_of_outcomes() {
        use Signal::{Discarded, Edited, SentAsIs};
        let mut estimate = Estimate::COLD;
        let budgets: Vec<u32> = [
            SentAsIs, SentAsIs, SentAsIs, Discarded, Discarded, Discarded, Edited, SentAsIs,
            SentAsIs, SentAsIs,
        ]
        .into_iter()
        .map(|signal| {
            estimate = estimate.observe(signal);
            estimate.first_pass_budget()
        })
        .collect();
        assert_eq!(budgets, [8, 8, 4, 6, 8, 9, 9, 7, 6, 6]);
    }

    /// Outcomes persist per project and seed the next session; a corrupt file starts cold.
    #[tokio::test]
    async fn the_estimate_persists_per_project_and_a_corrupt_file_starts_cold() {
        let agent_dir = tempfile::tempdir().unwrap();
        let project = Path::new("/work/parser");
        let learning = ProjectLearning::new(Some(agent_dir.path()), project);
        assert_eq!(learning.estimate().await, Estimate::COLD);
        learning.observe(Signal::Discarded);
        let path = learning.path.clone().unwrap();
        let expected = Estimate {
            samples: 1,
            acceptance: 0,
        };
        // The save is fire-and-forget: wait for the file it writes (observable readiness).
        let stored = loop {
            if let Ok(raw) = std::fs::read(&path) {
                break serde_json::from_slice::<Stored>(&raw).unwrap().estimate;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(stored, expected);
        let next_session = ProjectLearning::new(Some(agent_dir.path()), project);
        assert_eq!(next_session.estimate().await, expected);
        std::fs::write(&path, b"{\"schema\":1,\"estimate\":{\"sam").unwrap();
        let corrupt = ProjectLearning::new(Some(agent_dir.path()), project);
        assert_eq!(corrupt.estimate().await, Estimate::COLD);
    }
}
