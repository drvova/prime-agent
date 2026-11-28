//! The worker's workspace checkpoints: its session's shadow-git store
//! (`pa_core::session::checkpoints`, the rules both daemons share), opened
//! for the core's session, recording each turn and restoring the files of a
//! rewind.
//!
//! The kernel's `checkpoint.*` snapshots (the `repl` submodule) share the store.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::session::checkpoints::{CheckpointStore as Store, RewindTarget};
use serde_json::Value;

use crate::session_store::SessionEntry;
use crate::worker::SessionCore;

mod repl;
pub(crate) use repl::ReplCheckpoints;

/// The checkpoint store of the worker's current session, opened lazily and
/// reopened when a fork or switch changes the session.
pub(crate) struct WorkspaceCheckpoints {
    agent_dir: PathBuf,
    /// The session the slot was opened for and its store; `None` after a failed
    /// open or capture, so a failure is reported once, not per turn.
    slot: Mutex<Option<(String, Option<Arc<Store>>)>>,
}

/// A turn's opening checkpoint, closed by [`TurnCheckpoint::settle`].
pub(crate) struct TurnCheckpoint {
    store: Arc<Store>,
    leaf_before: Option<String>,
    tree: String,
}

impl TurnCheckpoint {
    /// Record the opening tree on the turn's first entry and the settled
    /// workspace on the new leaf. Blocking (git).
    pub(crate) fn settle(self, core: &Mutex<SessionCore>) {
        let (first, leaf) = {
            let core = core.lock().unwrap();
            let Some(session) = core.store.as_ref() else {
                return;
            };
            let branch = session.branch();
            let start = match self.leaf_before.as_deref() {
                Some(leaf) => branch
                    .iter()
                    .position(|entry| entry.id == leaf)
                    .map(|at| at + 1),
                None => Some(0),
            };
            let first = start
                .and_then(|at| branch.get(at))
                .map(|entry| entry.id.clone());
            (first, session.leaf_id().map(str::to_string))
        };
        // A turn that appended nothing (aborted before delivery) has no entry to key.
        let (Some(first), Some(leaf)) = (first, leaf) else {
            return;
        };
        if let Err(error) = self.store.record_turn(&self.tree, &first, &leaf) {
            eprintln!("pa-daemon: workspace checkpoint skipped: {error:#}");
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
    fn store(&self, core: &SessionCore) -> Option<Arc<Store>> {
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
        let opened = Store::open(&self.agent_dir, Path::new(&core.cwd), session.session_id())
            .map_err(|error| {
                eprintln!(
                    "pa-daemon: workspace checkpoints off for {}: {error:#}",
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
                eprintln!("pa-daemon: workspace checkpoints off for this session: {error:#}");
                *self
                    .slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((session_id, None));
                None
            }
        }
    }

    /// Restore the files for a navigation that already moved the leaf off
    /// `old_leaf`, recording the abandoned state at `old_leaf` so
    /// navigating back restores it. Answers the response's `files` value, or
    /// `None` when the target has no checkpoint. Blocking (git).
    pub(crate) fn rewind_files(
        &self,
        core: &Mutex<SessionCore>,
        target: &SessionEntry,
        old_leaf: Option<&str>,
    ) -> Option<Value> {
        let (store, new_leaf) = {
            let core = core.lock().unwrap();
            let new_leaf = core.store.as_ref()?.leaf_id().map(str::to_string);
            (self.store(&core)?, new_leaf)
        };
        let prompt = crate::session_tree::user_entry_text(target).is_some()
            || target.type_ == "custom_message";
        // The workspace before a prompt-like entry (the navigation lands on
        // its parent), or at any other.
        let target = if prompt {
            RewindTarget::BeforePrompt(&target.id)
        } else {
            RewindTarget::At(&target.id)
        };
        store.rewind(target, old_leaf, new_leaf.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionFile;
    use serde_json::json;

    /// A file-backed session over a fresh project (itself a git repository)
    /// and one turn that edits, deletes, and adds files; answers the
    /// checkpoints, the core, the workspace, and the turn's prompt/answer ids.
    fn one_turn(
        root: &Path,
    ) -> (
        WorkspaceCheckpoints,
        Mutex<SessionCore>,
        PathBuf,
        String,
        String,
    ) {
        let workspace = root.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let init = pa_types::session_scope::std_command("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status();
        assert!(init.unwrap().success());
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        std::fs::write(workspace.join("user.txt"), "v1\n").unwrap();
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.join("session.jsonl"));
        let cwd = workspace.to_string_lossy().into_owned();
        let core = Mutex::new(SessionCore::test_core(Some(session), cwd));
        let checkpoints = WorkspaceCheckpoints::new(root.join("agent"));
        let turn = checkpoints.begin_turn(&core).unwrap();
        let (prompt, answer) = {
            let mut core = core.lock().unwrap();
            let store = core.store.as_mut().unwrap();
            let prompt =
                store.append_message(&json!({"role": "user", "content": "edit", "timestamp": 1}));
            std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
            std::fs::write(workspace.join("user.txt"), "v2\n").unwrap();
            std::fs::create_dir_all(workspace.join("new/dir")).unwrap();
            std::fs::write(workspace.join("new/dir/added.rs"), "new\n").unwrap();
            let answer =
                store.append_message(&json!({"role": "assistant", "content": [], "timestamp": 2}));
            (prompt, answer)
        };
        turn.settle(&core);
        (checkpoints, core, workspace, prompt, answer)
    }

    fn navigate(
        checkpoints: &WorkspaceCheckpoints,
        core: &Mutex<SessionCore>,
        (target, from, to): (&str, Option<&str>, Option<&str>),
    ) -> Option<Value> {
        let entry = core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .entry(target)
            .cloned()
            .unwrap();
        core.lock()
            .unwrap()
            .store
            .as_mut()
            .unwrap()
            .branch_to(to)
            .unwrap();
        checkpoints.rewind_files(core, &entry, from)
    }

    /// Rewinding before the prompt restores the files (edits undone, the added
    /// file and its directories gone, the project's own index untouched), and
    /// navigating back (redo), from a reopened store, returns the edits.
    #[test]
    fn a_rewind_restores_the_turn_and_a_redo_returns_it() {
        let root = tempfile::tempdir().unwrap();
        let (checkpoints, core, workspace, prompt, answer) = one_turn(root.path());
        let file = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();

        let undo = navigate(&checkpoints, &core, (&prompt, Some(&answer), None));
        assert_eq!(undo, Some(json!({ "restored": 3, "conflicts": [] })));
        assert_eq!(
            (file("main.rs"), file("user.txt")),
            (Some("v1\n".into()), Some("v1\n".into()))
        );
        assert!(!workspace.join("new").exists());
        assert!(!workspace.join(".git/index").exists());

        let reopened = WorkspaceCheckpoints::new(root.path().join("agent"));
        let redo = navigate(&reopened, &core, (&answer, None, Some(&answer)));
        assert_eq!(redo, Some(json!({ "restored": 3, "conflicts": [] })));
        assert_eq!(file("new/dir/added.rs"), Some("new\n".into()));
    }

    /// A file edited outside the session after the turn settled is left alone.
    #[test]
    fn a_rewind_keeps_files_changed_outside_the_session() {
        let root = tempfile::tempdir().unwrap();
        let (checkpoints, core, workspace, prompt, answer) = one_turn(root.path());
        std::fs::write(workspace.join("user.txt"), "user edit\n").unwrap();

        let undo = navigate(&checkpoints, &core, (&prompt, Some(&answer), None));
        assert_eq!(
            undo,
            Some(json!({ "restored": 2, "conflicts": ["user.txt"] }))
        );
        let file = |name: &str| std::fs::read_to_string(workspace.join(name)).unwrap();
        assert_eq!(
            (file("main.rs"), file("user.txt")),
            ("v1\n".to_string(), "user edit\n".to_string())
        );
    }
}
