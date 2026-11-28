//! A session lifetime's workspace checkpoints, as the old worker keeps them
//! (pa-core's `session::checkpoints`, the store both daemons share): each
//! prompt's turn captures the workspace before it runs and records it, with
//! the workspace as the turn settled, on the entries the turn appended; a
//! `session.navigate_tree` with `restoreFiles` rewinds the files to its
//! target's record.
//!
//! The store is opened for the transcript the session runs on (its header's
//! id: a fork or an import is a store of its own) in the lifetime's working
//! directory, at the first turn or rewind; a session kept in memory or a
//! subagent keeps none (a subagent's writes land in its parent turn's
//! record), and a workspace that cannot be checkpointed (not a project
//! directory, git missing, a capture past its timeout) turns them off for
//! the lifetime, said once. Every git call runs on a blocking thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_core::session::checkpoints::{CheckpointStore, RewindTarget};
use pa_core::session_engine::engine::SessionEngine;
use serde_json::Value;

use crate::lock;

/// One lifetime's checkpoints.
pub(crate) struct Checkpoints {
    agent_dir: PathBuf,
    cwd: PathBuf,
    /// The transcript the store was opened for and the store; `None` after a
    /// failed open or capture.
    slot: Mutex<Option<(String, Option<Arc<CheckpointStore>>)>>,
}

/// A turn's opening capture, recorded by [`Checkpoints::settle`].
pub(crate) struct TurnOpening {
    store: Arc<CheckpointStore>,
    leaf_before: Option<String>,
    tree: String,
}

impl Checkpoints {
    pub(crate) fn new(agent_dir: PathBuf, cwd: PathBuf) -> Self {
        Checkpoints {
            agent_dir,
            cwd,
            slot: Mutex::new(None),
        }
    }

    /// The store of the transcript `session_id`, opened on first use.
    /// Blocking (git).
    fn store(&self, session_id: &str) -> Option<Arc<CheckpointStore>> {
        let mut slot = lock(&self.slot);
        if let Some((_, opened)) = slot.as_ref().filter(|(id, _)| id == session_id) {
            return opened.clone();
        }
        let opened = CheckpointStore::open(&self.agent_dir, &self.cwd, session_id)
            .map_err(|error| {
                tracing::info!(
                    "workspace checkpoints off for {}: {error:#}",
                    self.cwd.display()
                );
            })
            .ok()
            .map(Arc::new);
        *slot = Some((session_id.to_string(), opened.clone()));
        opened
    }

    /// Turns the checkpoints off for transcript `session_id` after `error`.
    fn off(&self, session_id: &str, error: &anyhow::Error) {
        tracing::info!("workspace checkpoints off for this session: {error:#}");
        *lock(&self.slot) = Some((session_id.to_string(), None));
    }

    /// Captures the workspace before a turn of `engine` runs.
    pub(crate) async fn begin(self: &Arc<Self>, engine: &SessionEngine) -> Option<TurnOpening> {
        let (session_id, leaf_before) = {
            let manager = engine.session.shared_persistence();
            let manager = manager.lock().await;
            manager.get_session_file()?;
            (
                manager.get_session_id().to_string(),
                manager.get_leaf_id().map(str::to_string),
            )
        };
        let checkpoints = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let store = checkpoints.store(&session_id)?;
            match store.capture() {
                Ok(tree) => Some(TurnOpening {
                    store,
                    leaf_before,
                    tree,
                }),
                Err(error) => {
                    checkpoints.off(&session_id, &error);
                    None
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Records the turn `opening` began on its first appended entry and its
    /// new leaf; a turn that appended nothing records nothing.
    pub(crate) async fn settle(opening: TurnOpening, engine: &SessionEngine) {
        let (first, leaf) = {
            let manager = engine.session.shared_persistence();
            let manager = manager.lock().await;
            let branch = manager.get_branch(None);
            let start = match opening.leaf_before.as_deref() {
                Some(leaf) => branch
                    .iter()
                    .position(|entry| entry.id() == Some(leaf))
                    .map(|at| at + 1),
                None => Some(0),
            };
            let first = start
                .and_then(|at| branch.get(at))
                .and_then(|entry| entry.id())
                .map(str::to_string);
            (first, manager.get_leaf_id().map(str::to_string))
        };
        let (Some(first), Some(leaf)) = (first, leaf) else {
            return;
        };
        let recorded = tokio::task::spawn_blocking(move || {
            opening.store.record_turn(&opening.tree, &first, &leaf)
        })
        .await;
        if let Ok(Err(error)) = recorded {
            tracing::info!("workspace checkpoint skipped: {error:#}");
        }
    }

    /// Rewinds the files for a navigation of transcript `session_id` that
    /// moved its leaf from `old_leaf` to `new_leaf` onto `target`: the reply's
    /// `files`, or `None` when the target has no record.
    pub(crate) async fn rewind(
        self: &Arc<Self>,
        session_id: String,
        target: &Value,
        old_leaf: Option<String>,
        new_leaf: Option<String>,
    ) -> Option<Value> {
        let id = target["id"].as_str()?.to_string();
        // The workspace before a prompt-like entry (the navigation lands on
        // its parent: `navigation::navigation_point`), or at any other.
        let prompt = pa_core::session::navigation::user_row_text(target).is_some()
            || target["type"] == "custom_message";
        let checkpoints = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let store = checkpoints.store(&session_id)?;
            let target = if prompt {
                RewindTarget::BeforePrompt(&id)
            } else {
                RewindTarget::At(&id)
            };
            store.rewind(target, old_leaf.as_deref(), new_leaf.as_deref())
        })
        .await
        .ok()
        .flatten()
    }
}
