//! The worker's workspace checkpoints: the turn boundaries that record the
//! session's files (pa-core's [`CheckpointStore`]) and the rewind lookups the
//! tree navigation restores from.
//!
//! Every turn records two trees: the workspace before the turn ran, keyed to
//! the turn's first appended entry ([`Boundary::Before`] — "rewind to before
//! this message"), and the workspace after it settled, keyed to the new leaf
//! ([`Boundary::At`] — the conflict baseline, and the redo target). Every
//! failure degrades to a conversation-only rewind; none reaches the turn.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_core::workspace_checkpoint::{Boundary, CheckpointError, CheckpointStore, TreeId};

use crate::session_store::{SessionEntry, SessionFile};
use crate::worker::SessionCore;

/// The checkpoint store of the worker's current session, opened lazily and
/// reopened when a fork or switch changes the session.
pub(crate) struct WorkspaceCheckpoints {
    agent_dir: PathBuf,
    enabled: bool,
    /// The session id the slot was opened for, and the open outcome (a failed
    /// open is kept so it is reported once, not retried per turn).
    slot: Mutex<Option<(String, Option<Arc<CheckpointStore>>)>>,
}

/// A turn's opening checkpoint, closed by [`TurnCheckpoint::settle`].
pub(crate) struct TurnCheckpoint {
    store: Arc<CheckpointStore>,
    leaf_before: Option<String>,
    tree: TreeId,
}

impl TurnCheckpoint {
    /// Record a settled turn: the opening tree on the turn's first entry and
    /// the settled workspace on the new leaf. Blocking (git).
    pub(crate) fn settle(self, core: &Mutex<SessionCore>) {
        let (first_entry, leaf) = {
            let core = core.lock().unwrap();
            let Some(store) = core.store.as_ref() else {
                return;
            };
            (
                first_entry_after(store, self.leaf_before.as_deref()),
                store.leaf_id().map(str::to_string),
            )
        };
        // A turn that appended nothing (aborted before delivery) has no entry to key.
        let (Some(first_entry), Some(leaf)) = (first_entry, leaf) else {
            return;
        };
        let recorded = self
            .store
            .record(&first_entry, Boundary::Before, &self.tree)
            .and_then(|()| self.store.capture())
            .and_then(|settled| self.store.record(&leaf, Boundary::At, &settled));
        if let Err(error) = recorded {
            log_capture_failure(&error);
        }
    }
}

/// What a rewind did to the files, for the `navigate_tree` response.
pub(crate) enum FileRewind {
    Restored {
        files: usize,
        conflicts: Vec<String>,
    },
    Unavailable(String),
}

impl WorkspaceCheckpoints {
    pub(crate) fn new(agent_dir: PathBuf) -> Self {
        WorkspaceCheckpoints {
            agent_dir,
            enabled: true,
            slot: Mutex::new(None),
        }
    }

    /// A checkpoint surface that never opens a store (tests whose sessions
    /// run in shared directories).
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        WorkspaceCheckpoints {
            agent_dir: PathBuf::new(),
            enabled: false,
            slot: Mutex::new(None),
        }
    }

    /// The store for the core's session, or `None` for an in-memory session,
    /// an RLM child (its writes land in the parent turn's settle capture), or
    /// a workspace that cannot be checkpointed.
    fn store(&self, core: &SessionCore) -> Option<Arc<CheckpointStore>> {
        let store = core.store.as_ref()?;
        if !self.enabled || store.path.as_os_str().is_empty() || core.rlm_depth > 0 {
            return None;
        }
        let session_id = store.session_id();
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((open_for, opened)) = slot.as_ref() {
            if open_for == session_id {
                return opened.clone();
            }
        }
        let opened = match CheckpointStore::open(
            &self.agent_dir,
            std::path::Path::new(&core.cwd),
            session_id,
        ) {
            Ok(opened) => Some(Arc::new(opened)),
            Err(error) => {
                eprintln!(
                    "pa-daemon: workspace checkpoints unavailable for {}: {error}",
                    core.cwd
                );
                None
            }
        };
        *slot = Some((session_id.to_string(), opened.clone()));
        opened
    }

    /// Capture the workspace before a turn runs. Blocking (git); call off the
    /// async runtime.
    pub(crate) fn begin_turn(&self, core: &Mutex<SessionCore>) -> Option<TurnCheckpoint> {
        let (store, leaf_before) = {
            let core = core.lock().unwrap();
            let leaf = core.store.as_ref()?.leaf_id().map(str::to_string);
            (self.store(&core)?, leaf)
        };
        match store.capture() {
            Ok(tree) => Some(TurnCheckpoint {
                store,
                leaf_before,
                tree,
            }),
            Err(error) => {
                log_capture_failure(&error);
                None
            }
        }
    }

    /// The ids of every entry a navigation could restore files for.
    pub(crate) fn restorable_entry_ids(&self, core: &SessionCore) -> Vec<String> {
        let (Some(store), Some(session)) = (self.store(core), core.store.as_ref()) else {
            return Vec::new();
        };
        session
            .entries()
            .iter()
            .filter(|entry| rewind_tree_in(&store, entry).is_some())
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Restore the files for a navigation that already moved the leaf from
    /// `old_leaf` to `new_leaf`. The abandoned state is recorded at `old_leaf`
    /// so navigating back restores it. Blocking (git).
    pub(crate) fn rewind_files(
        &self,
        core: &Mutex<SessionCore>,
        target: &SessionEntry,
        old_leaf: Option<&str>,
        new_leaf: Option<&str>,
    ) -> FileRewind {
        let (store, tree) = {
            let core = core.lock().unwrap();
            let Some(store) = self.store(&core) else {
                return FileRewind::Unavailable("checkpoints are off for this workspace".into());
            };
            let Some(tree) = rewind_tree_in(&store, target) else {
                return FileRewind::Unavailable("no checkpoint recorded for this point".into());
            };
            (store, tree)
        };
        let baseline = old_leaf.and_then(|leaf| store.lookup(leaf, Boundary::At));
        let outcome = match store.restore(&tree, baseline.as_ref()) {
            Ok(outcome) => outcome,
            Err(error) => return FileRewind::Unavailable(error.to_string()),
        };
        let recorded = old_leaf
            .map_or(Ok(()), |leaf| {
                store.record(leaf, Boundary::At, &outcome.preimage)
            })
            .and_then(|()| new_leaf.map_or(Ok(()), |leaf| store.record(leaf, Boundary::At, &tree)));
        if let Err(error) = recorded {
            log_capture_failure(&error);
        }
        FileRewind::Restored {
            files: outcome.restored,
            conflicts: outcome.conflicts,
        }
    }
}

/// The tree a navigation to `target` restores: the workspace before a
/// prompt-like entry (the navigation lands on its parent), or at any other.
fn rewind_tree_in(store: &CheckpointStore, target: &SessionEntry) -> Option<TreeId> {
    if crate::session_tree::user_entry_text(target).is_some() || target.type_ == "custom_message" {
        store.lookup(&target.id, Boundary::Before)
    } else {
        store.lookup(&target.id, Boundary::At)
    }
}

/// The first entry the turn appended: the current branch's child of the leaf
/// the turn started on (the branch's root when it started empty).
fn first_entry_after(store: &SessionFile, leaf_before: Option<&str>) -> Option<String> {
    let branch = store.branch();
    match leaf_before {
        Some(leaf) => {
            let position = branch.iter().position(|entry| entry.id == leaf)?;
            branch.get(position + 1).map(|entry| entry.id.clone())
        }
        None => branch.first().map(|entry| entry.id.clone()),
    }
}

/// Report a capture failure once: the store turns itself off after the first,
/// and the later `Disabled` answers stay quiet.
fn log_capture_failure(error: &CheckpointError) {
    if !matches!(error, CheckpointError::Disabled(_)) {
        eprintln!("pa-daemon: workspace checkpoint skipped: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One turn's bracket on a file-backed session: the opening tree keys to
    /// the prompt, rewinding before it restores the files, and navigating back
    /// to the abandoned leaf (redo) restores the turn's edits.
    #[test]
    fn a_rewind_restores_the_turn_and_a_redo_returns_it() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.path().join("session.jsonl"));
        let core = Mutex::new(SessionCore::test_core(
            Some(session),
            workspace.to_string_lossy().into_owned(),
        ));
        let checkpoints = WorkspaceCheckpoints::new(root.path().join("agent"));

        let turn = checkpoints.begin_turn(&core).unwrap();
        let (prompt, answer) = {
            let mut core = core.lock().unwrap();
            let store = core.store.as_mut().unwrap();
            let prompt = store
                .append_message(&json!({"role": "user", "content": "edit it", "timestamp": 1}));
            std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
            std::fs::write(workspace.join("added.rs"), "new\n").unwrap();
            let answer =
                store.append_message(&json!({"role": "assistant", "content": [], "timestamp": 2}));
            (prompt, answer)
        };
        turn.settle(&core);
        assert_eq!(
            checkpoints.restorable_entry_ids(&core.lock().unwrap()),
            vec![prompt.clone(), answer.clone()]
        );

        // Undo: the leaf moves before the prompt, then the files follow.
        let target = core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .entry(&prompt)
            .cloned()
            .unwrap();
        core.lock()
            .unwrap()
            .store
            .as_mut()
            .unwrap()
            .branch_to(None)
            .unwrap();
        let FileRewind::Restored { files, conflicts } =
            checkpoints.rewind_files(&core, &target, Some(&answer), None)
        else {
            panic!("rewind degraded");
        };
        assert_eq!((files, conflicts), (2, Vec::new()));
        assert_eq!(
            std::fs::read_to_string(workspace.join("main.rs")).unwrap(),
            "v1\n"
        );
        assert!(!workspace.join("added.rs").exists());

        // Redo: back to the abandoned leaf restores its recorded state.
        let target = core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .entry(&answer)
            .cloned()
            .unwrap();
        core.lock()
            .unwrap()
            .store
            .as_mut()
            .unwrap()
            .branch_to(Some(&answer))
            .unwrap();
        let FileRewind::Restored { files, .. } =
            checkpoints.rewind_files(&core, &target, None, Some(&answer))
        else {
            panic!("redo degraded");
        };
        assert_eq!(files, 2);
        assert_eq!(
            std::fs::read_to_string(workspace.join("main.rs")).unwrap(),
            "v2\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("added.rs")).unwrap(),
            "new\n"
        );
    }
}
