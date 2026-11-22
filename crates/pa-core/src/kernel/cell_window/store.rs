//! The learned window's on-disk state: one small JSON file per project under
//! `<agent dir>/cell-windows/`, named by a hash of the project directory (the
//! path itself is never stored or reported).

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::estimator::EstimatorState;
use super::learner::CellWindowStore;

/// Bumped when the stored estimator changes meaning; an older file then loads
/// as cold.
const SCHEMA: u64 = 1;
const DIR: &str = "cell-windows";

#[derive(Serialize, Deserialize)]
struct StoredWindow {
    schema: u64,
    estimator: EstimatorState,
}

/// One project's learned state on disk.
pub(crate) struct DiskStore {
    path: PathBuf,
}

impl DiskStore {
    /// The store for cells run in `project_dir` (the session's working
    /// directory), under `agent_dir`.
    pub(crate) fn for_project(agent_dir: &Path, project_dir: &Path) -> Self {
        let digest = Sha256::digest(project_dir.to_string_lossy().as_bytes());
        let name = digest[..8]
            .iter()
            .fold(String::with_capacity(16), |mut name, byte| {
                let _ = write!(name, "{byte:02x}");
                name
            });
        Self {
            path: agent_dir.join(DIR).join(format!("{name}.json")),
        }
    }
}

impl CellWindowStore for DiskStore {
    fn load(&self) -> impl Future<Output = Option<EstimatorState>> + Send {
        let path = self.path.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                // Missing, unreadable, corrupt, older-schema, or non-finite: start cold.
                let raw = std::fs::read(&path).ok()?;
                let stored: StoredWindow = serde_json::from_slice(&raw).ok()?;
                (stored.schema == SCHEMA && stored.estimator.is_valid()).then_some(stored.estimator)
            })
            .await
            .ok()
            .flatten()
        }
    }

    fn save(&self, state: EstimatorState) -> impl Future<Output = ()> + Send {
        let path = self.path.clone();
        async move {
            let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                pa_types::session_scope::ensure_live()?;
                let body = serde_json::to_string(&StoredWindow {
                    schema: SCHEMA,
                    estimator: state,
                })?;
                let dir = path.parent().unwrap_or_else(|| Path::new("."));
                std::fs::create_dir_all(dir)?;
                // Temp + rename, so a reader never sees half a file. The temp name is unique per
                // write (not per process like `settings::storage::atomic_write_with`): sessions in
                // one process can share a project, and two writers must never share a temp file.
                let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
                std::fs::write(&temp, body)?;
                crate::platform::rename_onto(&temp, &path).inspect_err(|_| {
                    let _ = std::fs::remove_file(&temp);
                })
            })
            .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%error, "learned cell window not saved");
                }
                Err(error) => tracing::debug!(%error, "learned cell window save task failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corrupt (or foreign) state file loads as cold instead of failing the session.
    #[tokio::test]
    async fn a_corrupt_state_file_starts_cold() {
        let agent_dir = tempfile::tempdir().unwrap();
        let store = DiskStore::for_project(agent_dir.path(), Path::new("/work/project"));
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, b"{\"schema\":1,\"estimator\":{\"samp").unwrap();
        assert_eq!(store.load().await, None);
    }
}
