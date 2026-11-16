//! The worker's workspace checkpoints over pa-core's [`CheckpointStore`].
//!
//! Every turn records the workspace before it ran, keyed to its first appended
//! entry ([`Boundary::Before`]: "rewind to before this message"), and after it
//! settled, keyed to the new leaf ([`Boundary::At`]: the conflict baseline and
//! the redo target). Every failure degrades to a conversation-only rewind.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::workspace_checkpoint::{Boundary, CheckpointStore, TreeId};
use serde_json::{json, Value};

use crate::session_store::SessionEntry;
use crate::worker::SessionCore;

/// The checkpoint store of the worker's current session, opened lazily and
/// reopened when a fork or switch changes the session.
pub(crate) struct WorkspaceCheckpoints {
    agent_dir: PathBuf,
    /// The session the slot was opened for and its store; `None` after a
    /// failed open or capture, so a failure is reported once, not per turn.
    slot: Mutex<Option<(String, Option<Arc<CheckpointStore>>)>>,
}

/// A turn's opening checkpoint, closed by [`TurnCheckpoint::settle`].
pub(crate) struct TurnCheckpoint {
    store: Arc<CheckpointStore>,
    leaf_before: Option<String>,
    tree: TreeId,
}

impl TurnCheckpoint {
    /// Record the opening tree on the turn's first entry and the settled
    /// workspace on the new leaf. Blocking (git).
    pub(crate) fn settle(self, core: &Mutex<SessionCore>) {
        let (first_entry, leaf) = {
            let core = core.lock().unwrap();
            let Some(store) = core.store.as_ref() else {
                return;
            };
            let branch = store.branch();
            let start = match self.leaf_before.as_deref() {
                Some(leaf) => branch
                    .iter()
                    .position(|entry| entry.id == leaf)
                    .map(|at| at + 1),
                None => Some(0),
            };
            (
                start
                    .and_then(|at| branch.get(at))
                    .map(|entry| entry.id.clone()),
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
            eprintln!("pa-daemon: workspace checkpoint skipped: {error}");
        }
    }
}

impl WorkspaceCheckpoints {
    pub(crate) fn new(agent_dir: PathBuf) -> Self {
        WorkspaceCheckpoints {
            agent_dir,
            slot: Mutex::new(None),
        }
    }

    /// The store for the core's session, or `None` for an in-memory session,
    /// an RLM child (its writes land in the parent turn's settle capture), or
    /// a workspace that cannot be checkpointed.
    fn store(&self, core: &SessionCore) -> Option<Arc<CheckpointStore>> {
        let session = core.store.as_ref()?;
        if session.path.as_os_str().is_empty() || core.rlm_depth > 0 {
            return None;
        }
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, opened)) = slot.as_ref().filter(|(id, _)| id == session.session_id()) {
            return opened.clone();
        }
        let opened =
            CheckpointStore::open(&self.agent_dir, Path::new(&core.cwd), session.session_id())
                .map_err(|error| {
                    eprintln!(
                        "pa-daemon: workspace checkpoints off for {}: {error}",
                        core.cwd
                    );
                })
                .ok()
                .map(Arc::new);
        *slot = Some((session.session_id().to_string(), opened.clone()));
        opened
    }

    /// Capture the workspace before a turn runs. Blocking (git).
    pub(crate) fn begin_turn(&self, core: &Mutex<SessionCore>) -> Option<TurnCheckpoint> {
        let (store, leaf_before, session_id) = {
            let core = core.lock().unwrap();
            let session = core.store.as_ref()?;
            let leaf = session.leaf_id().map(str::to_string);
            (self.store(&core)?, leaf, session.session_id().to_string())
        };
        match store.capture() {
            Ok(tree) => Some(TurnCheckpoint {
                store,
                leaf_before,
                tree,
            }),
            Err(error) => {
                eprintln!("pa-daemon: workspace checkpoints off for this session: {error}");
                *self
                    .slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((session_id, None));
                None
            }
        }
    }

    /// The ids of every entry a navigation could restore files for.
    pub(crate) fn restorable_entry_ids(&self, core: &SessionCore) -> Vec<String> {
        let (Some(store), Some(session)) = (self.store(core), core.store.as_ref()) else {
            return Vec::new();
        };
        let entries = session.entries().iter();
        entries
            .filter(|entry| rewind_tree(&store, entry).is_some())
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Restore the files for a navigation that already moved the leaf from
    /// `old_leaf` to `new_leaf`, recording the abandoned state at `old_leaf` so
    /// navigating back restores it. Answers the response's `files` value, or
    /// `None` when the target has no checkpoint. Blocking (git).
    pub(crate) fn rewind_files(
        &self,
        core: &Mutex<SessionCore>,
        target: &SessionEntry,
        old_leaf: Option<&str>,
        new_leaf: Option<&str>,
    ) -> Option<Value> {
        let store = self.store(&core.lock().unwrap())?;
        let tree = rewind_tree(&store, target)?;
        let baseline = old_leaf.and_then(|leaf| store.lookup(leaf, Boundary::At));
        let outcome = match store.restore(&tree, baseline.as_ref()) {
            Ok(outcome) => outcome,
            Err(error) => return Some(json!({ "error": error.to_string() })),
        };
        let recorded = old_leaf
            .map_or(Ok(()), |leaf| {
                store.record(leaf, Boundary::At, &outcome.preimage)
            })
            .and_then(|()| new_leaf.map_or(Ok(()), |leaf| store.record(leaf, Boundary::At, &tree)));
        if let Err(error) = recorded {
            eprintln!("pa-daemon: workspace checkpoint skipped: {error}");
        }
        Some(json!({ "restored": outcome.restored, "conflicts": outcome.conflicts }))
    }
}

/// The tree a navigation to `target` restores: the workspace before a
/// prompt-like entry (the navigation lands on its parent), or at any other.
fn rewind_tree(store: &CheckpointStore, target: &SessionEntry) -> Option<TreeId> {
    let prompt =
        crate::session_tree::user_entry_text(target).is_some() || target.type_ == "custom_message";
    store.lookup(
        &target.id,
        if prompt {
            Boundary::Before
        } else {
            Boundary::At
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionFile;

    /// One turn's bracket on a file-backed session: rewinding before the
    /// prompt restores the files, and navigating back (redo) returns the edits.
    #[test]
    fn a_rewind_restores_the_turn_and_a_redo_returns_it() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let file = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.path().join("session.jsonl"));
        let cwd = workspace.to_string_lossy().into_owned();
        let core = Mutex::new(SessionCore::test_core(Some(session), cwd));
        let checkpoints = WorkspaceCheckpoints::new(root.path().join("agent"));
        let entry = |id: &str| {
            core.lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .entry(id)
                .cloned()
                .unwrap()
        };
        let branch_to = |id: Option<&str>| {
            core.lock()
                .unwrap()
                .store
                .as_mut()
                .unwrap()
                .branch_to(id)
                .unwrap();
        };

        let turn = checkpoints.begin_turn(&core).unwrap();
        let (prompt, answer) = {
            let mut core = core.lock().unwrap();
            let store = core.store.as_mut().unwrap();
            let prompt =
                store.append_message(&json!({"role": "user", "content": "edit", "timestamp": 1}));
            std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
            std::fs::write(workspace.join("added.rs"), "new\n").unwrap();
            let answer =
                store.append_message(&json!({"role": "assistant", "content": [], "timestamp": 2}));
            (prompt, answer)
        };
        turn.settle(&core);
        let restorable = checkpoints.restorable_entry_ids(&core.lock().unwrap());
        assert_eq!(restorable, vec![prompt.clone(), answer.clone()]);

        branch_to(None);
        let undo = checkpoints.rewind_files(&core, &entry(&prompt), Some(&answer), None);
        assert_eq!(undo, Some(json!({ "restored": 2, "conflicts": [] })));
        assert_eq!(
            (file("main.rs"), file("added.rs")),
            (Some("v1\n".into()), None)
        );

        branch_to(Some(&answer));
        let redo = checkpoints.rewind_files(&core, &entry(&answer), None, Some(&answer));
        assert_eq!(redo, Some(json!({ "restored": 2, "conflicts": [] })));
        assert_eq!(
            (file("main.rs"), file("added.rs")),
            (Some("v2\n".into()), Some("new\n".into()))
        );
    }
}
