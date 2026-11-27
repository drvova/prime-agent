//! The worker's [`WorkspaceCheckpointController`]: the kernel's `checkpoint.*` snapshots on the
//! session's shadow-git store. A checkpoint is a captured tree plus a row in the session's own
//! `<session>.repl.jsonl`, so another session's checkpoints (and the per-turn rewind records)
//! are never found by id. A restore writes only inside the store's workspace.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use pa_core::session_engine::checkpoint_host::{
    CheckpointRestore, WorkspaceCheckpoint, WorkspaceCheckpointController,
};
use serde::{Deserialize, Serialize};

use super::{Store, WorkspaceCheckpoints};
use crate::worker::SessionCore;

/// One checkpoint row in the session's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReplRow {
    id: String,
    label: String,
    tree: String,
    created_at_ms: u64,
}

/// The kernel's checkpoint store for the worker's current session.
pub(crate) struct ReplCheckpoints {
    checkpoints: Arc<WorkspaceCheckpoints>,
    core: Arc<Mutex<SessionCore>>,
    agent_dir: PathBuf,
}

impl ReplCheckpoints {
    pub(crate) fn new(
        checkpoints: Arc<WorkspaceCheckpoints>,
        core: Arc<Mutex<SessionCore>>,
        agent_dir: PathBuf,
    ) -> Self {
        Self {
            checkpoints,
            core,
            agent_dir,
        }
    }

    fn store(&self) -> Result<Arc<Store>> {
        let core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checkpoints.store(&core).ok_or_else(|| {
            anyhow!(
                "this session's workspace cannot be checkpointed (an unsaved session, a \
                 subagent, or a directory that is not a project workspace)"
            )
        })
    }

    /// This session's rows, oldest first (a torn last line is skipped).
    fn rows(store: &Store) -> Vec<ReplRow> {
        std::fs::read_to_string(store.repl_log())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn tree(store: &Store, id: &str) -> Result<String> {
        Self::rows(store)
            .into_iter()
            .find(|row| row.id == id)
            .map(|row| row.tree)
            .ok_or_else(|| anyhow!("this session has no checkpoint {id:?}"))
    }
}

impl WorkspaceCheckpointController for ReplCheckpoints {
    fn create(&self, label: &str) -> Result<WorkspaceCheckpoint> {
        let store = self.store()?;
        let tree = store.capture().context("capture the workspace")?;
        let label = match label.trim() {
            "" => format!("checkpoint {}", Self::rows(&store).len() + 1),
            label => label.to_string(),
        };
        let row = ReplRow {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            label,
            tree,
            created_at_ms: crate::util::now_ms(),
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.repl_log())?;
        writeln!(file, "{}", serde_json::to_string(&row)?)?;
        Ok(WorkspaceCheckpoint {
            id: row.id,
            label: row.label,
            created_at_ms: row.created_at_ms,
        })
    }

    fn list(&self) -> Result<Vec<WorkspaceCheckpoint>> {
        let store = self.store()?;
        Ok(Self::rows(&store)
            .into_iter()
            .map(|row| WorkspaceCheckpoint {
                id: row.id,
                label: row.label,
                created_at_ms: row.created_at_ms,
            })
            .collect())
    }

    fn changes(&self, id: &str) -> Result<Vec<String>> {
        let store = self.store()?;
        let tree = Self::tree(&store, id)?;
        let now = store.capture()?;
        Ok(store
            .diff(&now, &tree)?
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    }

    fn restore(&self, id: &str) -> Result<CheckpointRestore> {
        let store = self.store()?;
        let tree = Self::tree(&store, id)?;
        // No baseline: the user confirmed the full list of paths `changes` named.
        let (_, restored, _) = store.restore(&tree, None)?;
        Ok(CheckpointRestore { restored })
    }

    fn restore_enabled(&self) -> bool {
        let cwd = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cwd
            .clone();
        pa_core::settings::SettingsManager::create(cwd, &self.agent_dir)
            .get_repl_can_restore_checkpoints()
    }
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use super::*;
    use crate::session_store::SessionFile;

    fn project(root: &std::path::Path, name: &str) -> (Arc<Mutex<SessionCore>>, PathBuf) {
        let workspace = root.join(name);
        std::fs::create_dir_all(&workspace).unwrap();
        let init = pa_types::session_scope::std_command("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status();
        assert!(init.unwrap().success());
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.join(format!("{name}.jsonl")));
        session.append_message(&json!({"role": "user", "content": "go", "timestamp": 1}));
        let core = SessionCore::test_core(Some(session), workspace.to_string_lossy().into_owned());
        (Arc::new(Mutex::new(core)), workspace)
    }

    /// A checkpoint restores the model's own edits (rewrites, removals, additions) after the
    /// changed paths are previewed, and another session never finds it by id.
    #[test]
    fn a_checkpoint_restores_the_workspace_and_stays_in_its_session() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agent");
        let (core, workspace) = project(root.path(), "project");
        let checkpoints = ReplCheckpoints::new(
            Arc::new(WorkspaceCheckpoints::new(agent_dir.clone())),
            core,
            agent_dir.clone(),
        );
        let taken = checkpoints.create("before refactor").unwrap();
        assert_eq!(checkpoints.list().unwrap(), vec![taken.clone()]);
        std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
        std::fs::create_dir_all(workspace.join("new")).unwrap();
        std::fs::write(workspace.join("new/added.rs"), "new\n").unwrap();
        let mut changes = checkpoints.changes(&taken.id).unwrap();
        changes.sort();
        assert_eq!(
            changes,
            vec!["main.rs".to_string(), "new/added.rs".to_string()]
        );
        assert_eq!(
            checkpoints.restore(&taken.id).unwrap(),
            CheckpointRestore { restored: 2 }
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("main.rs")).unwrap(),
            "v1\n"
        );
        assert!(!workspace.join("new").exists());

        // The same workspace in another session: its own (empty) list, the id unknown.
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.path().join("other.jsonl"));
        let other_core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(session),
            workspace.to_string_lossy().into_owned(),
        )));
        let other = ReplCheckpoints::new(
            Arc::new(WorkspaceCheckpoints::new(agent_dir.clone())),
            other_core,
            agent_dir,
        );
        assert_eq!(other.list().unwrap(), Vec::new());
        assert_eq!(
            other.restore(&taken.id).unwrap_err().to_string(),
            format!("this session has no checkpoint {:?}", taken.id)
        );
    }

    /// The restore gate reads the global settings only.
    #[test]
    fn the_restore_gate_follows_the_global_setting() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let (core, _) = project(root.path(), "project");
        let checkpoints = ReplCheckpoints::new(
            Arc::new(WorkspaceCheckpoints::new(agent_dir.clone())),
            core,
            agent_dir.clone(),
        );
        assert!(!checkpoints.restore_enabled());
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{ "replCanRestoreCheckpoints": true }"#,
        )
        .unwrap();
        assert!(checkpoints.restore_enabled());
    }
}
