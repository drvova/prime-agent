//! What the runtime daemon keeps of its sessions' families
//! ([`DaemonFamily`]): the old daemon's RLM spawn ledger
//! (`pa_core::session_engine::rlm_ledger`, one per sessions dir) and each
//! child's display file (`rlm-subagent.json` in its session dir), written at
//! the moments the old daemon writes them, so a family the runtime daemon
//! spawned reads the same to every roster and catalog that reads them, and
//! a parent opened again finds its children there ([`super::Subagents::reopen_children`]).
//!
//! - **Spawn**: the edge (`spawn`, its child id, the parent's and the
//!   child's transcripts, depth, name and factory tag) and a `running`
//!   display entry, before the child is given its task; a spawn whose edge
//!   is not written is refused, as the old daemon's admission is.
//! - **Settle**: a child whose task ended done reads `completed`.
//! - **Rename**: a `rename` record for the renamed child.
//! - **Delete**: the edge tombstoned (`user`) and the display entry
//!   `deleted`, before the child's teardown: a tombstone that is not
//!   written fails the delete. A delete aimed at a child no session runs
//!   (stopped, or deleted before: a retry) is the ledger's alone.
//! - **Passive children**: the saved catalog walks the ledger from its rows
//!   and the sessions the runtime runs for the children no root it runs or
//!   lists keeps (pa-core's `rlm_roster`, the old supervisor's walk), and
//!   the live roster seeds an open root's ledger family
//!   ([`DaemonFamily::family_below`], `daemon/handlers/feeds.rs`).
//!
//! The ledger's own writes are durable, single `O_APPEND` records, and run
//! on the blocking pool. A family with no transcripts (a parent kept in
//! memory) writes nothing.
//!
//! A depth-0 session's kernel also asks the daemon for a sibling root
//! (`rlm.create_session`, [`RootSessions`]): a session of its own, created as
//! a client's `session.create` creates one, its header naming the creator's
//! transcript, and never part of the creator's family (no edge, so the
//! creator's crash, replacement or stop leaves it running), as the old
//! supervisor's `createRlmRootSession` did.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Context as _;
use pa_core::session_engine::rlm_host::{RlmCreateSessionHandle, RlmFactoryTag};
use pa_core::session_engine::rlm_ledger::{
    read_rlm_subagent_display, write_rlm_subagent_display, RlmLedgerDeleteReason, RlmLedgerEdge,
    RlmSpawnInput, RlmSpawnLedger, RlmSubagentDisplayEntry,
};
use pa_core::session_engine::rlm_roster::{
    walk_passive_rlm_children, PassiveRlmChild, RosterWalkRoot,
};

/// One spawn the ledger records.
pub(crate) struct SpawnRecord {
    pub(crate) child_id: String,
    pub(crate) parent_file: String,
    pub(crate) child_file: String,
    pub(crate) child_dir: String,
    pub(crate) depth: u32,
    pub(crate) name: String,
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) factory: Option<RlmFactoryTag>,
    pub(crate) created_at: u64,
}

/// One `rlm.create_session` a depth-0 session's kernel asked for, with what
/// the new root inherits from the session that asked.
pub(crate) struct RootSessionRequest {
    /// The task the new root is prompted with.
    pub(crate) prompt: String,
    /// Its name; `None` leaves it unnamed (its handle names it by its id).
    pub(crate) name: Option<String>,
    /// The model selector asked for; `None` inherits the creator's.
    pub(crate) model: Option<String>,
    /// The thinking level asked for; `None` inherits the creator's.
    pub(crate) thinking: Option<String>,
    /// The new root's working directory, resolved against the creator's.
    pub(crate) cwd: PathBuf,
    /// The creator's working directory (its settings' `allowedModels`).
    pub(crate) creator_cwd: PathBuf,
    /// The creator's transcript, which the new header names; `None` for a
    /// creator kept in memory.
    pub(crate) creator_file: Option<String>,
    /// The creator's model selector and thinking level.
    pub(crate) creator_model: Option<String>,
    pub(crate) creator_thinking: Option<pa_types::ai::ModelThinkingLevel>,
    /// The creator's environment over the process's own.
    pub(crate) creator_env: BTreeMap<String, String>,
}

/// Creates the depth-0 sessions a session's kernel asks for
/// (`rlm.create_session`): the runtime daemon's handlers.
///
/// An implementation creates the root as a client's `session.create` does
/// (its lease, kept options, open record and roster entry), on the model
/// the request resolves to under the settings' `allowedModels` (else the
/// creator's), prompts it with the task as its owner, and answers its
/// handle once the task is queued; a root whose prompt is refused is
/// stopped again. It never waits on the new root's turn.
pub(crate) trait RootSessions: Send + Sync {
    /// Creates and prompts the root `request` asks for.
    fn create_root(
        &self,
        request: RootSessionRequest,
    ) -> pa_agent::BoxFut<'_, anyhow::Result<RlmCreateSessionHandle>>;
}

/// Hears that a session's wait edge (its `waitingOn`) changed, by the
/// session's id; it must return at once (it runs on the waiting cell's path).
pub(crate) type WaitEdgeChanged = Arc<dyn Fn(&str) + Send + Sync>;

/// The runtime daemon's family records: its sessions dir's spawn ledger,
/// and who hears its sessions' wait edges change (the roster feed, which
/// reports each session's `waitingOn` as the old supervisor's roster did).
pub(crate) struct DaemonFamily {
    ledger: Arc<RlmSpawnLedger>,
    wait_edge_changed: WaitEdgeChanged,
    /// Who creates the roots the sessions' kernels ask for, once the
    /// daemon serves.
    roots: OnceLock<Weak<dyn RootSessions>>,
}

impl DaemonFamily {
    /// The records of the sessions under `sessions_dir`, the ledger kept
    /// under `agent_dir`'s `rlm-ledger/`; `wait_edge_changed` hears every
    /// session's `waitingOn` change.
    pub(crate) fn new(
        agent_dir: &Path,
        sessions_dir: &Path,
        wait_edge_changed: WaitEdgeChanged,
    ) -> Self {
        DaemonFamily {
            ledger: Arc::new(RlmSpawnLedger::new(agent_dir, sessions_dir, |message| {
                tracing::warn!("{message}");
            })),
            wait_edge_changed,
            roots: OnceLock::new(),
        }
    }

    /// Gives the family who creates the sessions' roots; attached once, a
    /// second attach changes nothing.
    pub(crate) fn attach_roots(&self, roots: Weak<dyn RootSessions>) {
        let _ = self.roots.set(roots);
    }

    /// Who creates the sessions' roots, while the daemon serves.
    pub(crate) fn roots(&self) -> Option<Arc<dyn RootSessions>> {
        self.roots.get().and_then(Weak::upgrade)
    }

    /// Session `id`'s wait edge changed.
    pub(crate) fn wait_edge_changed(&self, id: &str) {
        (self.wait_edge_changed)(id);
    }

    /// Runs `write` on the ledger on the blocking pool.
    pub(crate) async fn on_ledger<T: Send + 'static>(
        &self,
        write: impl FnOnce(&RlmSpawnLedger) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let ledger = Arc::clone(&self.ledger);
        tokio::task::spawn_blocking(move || write(&ledger))
            .await
            .context("the ledger's thread panicked")?
    }

    /// The deleted subagents' spend by their parent's canonical transcript
    /// path (the agents view's cost rollup bills it to the parent).
    ///
    /// # Errors
    ///
    /// The ledger could not be replayed.
    pub(crate) async fn deleted_descendant_usage(
        &self,
    ) -> anyhow::Result<
        std::collections::HashMap<String, pa_core::session::usage_summary::SessionUsageSummary>,
    > {
        self.on_ledger(RlmSpawnLedger::deleted_descendant_usage_by_parent)
            .await
    }

    /// Records a spawn: its edge, then its `running` display entry.
    ///
    /// # Errors
    ///
    /// The edge could not be written (the ledger refuses it, or the append
    /// failed).
    pub(crate) async fn spawned(&self, spawn: SpawnRecord) -> anyhow::Result<()> {
        self.on_ledger(move |ledger| {
            ledger.append_spawn(&RlmSpawnInput {
                child_id: spawn.child_id.clone(),
                parent: spawn.parent_file,
                child: spawn.child_file.clone(),
                depth: spawn.depth,
                name: spawn.name.clone(),
                factory: spawn.factory,
            })?;
            let display = RlmSubagentDisplayEntry {
                type_tag: "rlm_subagent".to_string(),
                child_id: spawn.child_id,
                session_name: spawn.name,
                session_dir: spawn.child_dir,
                session_file: spawn.child_file,
                rlm_parent_node_id: None,
                prompt: Some(spawn.prompt),
                spawn_code: None,
                model: Some(serde_json::json!(spawn.model)),
                status: "running".to_string(),
                created_at: spawn.created_at,
            };
            if let Err(error) = write_rlm_subagent_display(&display) {
                tracing::warn!("the RLM subagent display entry was not written: {error:#}");
            }
            Ok(())
        })
        .await
    }

    /// Marks the display entry of child `child_id` in `child_dir` with
    /// `status` (`completed` or `deleted`), unless it already reads deleted.
    async fn display_status(&self, child_dir: String, child_id: String, status: &'static str) {
        let written = self
            .on_ledger(move |_| {
                let Some(mut display) = read_rlm_subagent_display(Path::new(&child_dir)) else {
                    return Ok(());
                };
                if display.child_id != child_id || display.status == "deleted" {
                    return Ok(());
                }
                display.status = status.to_string();
                write_rlm_subagent_display(&display).map(|_| ())
            })
            .await;
        if let Err(error) = written {
            tracing::warn!("the RLM subagent display entry was not marked {status}: {error:#}");
        }
    }

    /// A child's task ended done: its display entry reads `completed`.
    pub(crate) async fn completed(&self, child_dir: String, child_id: String) {
        self.display_status(child_dir, child_id, "completed").await;
    }

    /// Records a child's rename.
    pub(crate) async fn renamed(&self, child_id: String, child_file: String, name: String) {
        let written = self
            .on_ledger(move |ledger| ledger.append_rename(&child_id, &child_file, &name))
            .await;
        if let Err(error) = written {
            tracing::warn!("the RLM ledger rename was not written: {error:#}");
        }
    }

    /// Records a child's delete, its deletion boundary: its edge tombstoned
    /// for `reason`, then its display entry `deleted` (best effort: the
    /// ledger's tombstone is the authority).
    ///
    /// # Errors
    ///
    /// The tombstone was not written.
    pub(crate) async fn deleted(
        &self,
        child_id: String,
        child_file: String,
        child_dir: String,
        reason: RlmLedgerDeleteReason,
    ) -> anyhow::Result<()> {
        {
            let child_id = child_id.clone();
            self.on_ledger(move |ledger| {
                ledger
                    .append_delete(&child_id, &child_file, reason)
                    .with_context(|| format!("tombstone RLM subagent {child_id}"))
            })
            .await?;
        }
        self.display_status(child_dir, child_id, "deleted").await;
        Ok(())
    }

    /// The deletion boundary of a delete aimed at the stopped child
    /// `selector` (a session no family member runs): pa-core's
    /// `tombstone_stopped_child`, whose retry writes no second boundary.
    ///
    /// # Errors
    ///
    /// The ledger could not be read or written, or does not record the
    /// child.
    pub(crate) async fn deleted_stopped(
        &self,
        selector: String,
        child_id: Option<String>,
        reason: RlmLedgerDeleteReason,
    ) -> anyhow::Result<()> {
        self.on_ledger(move |ledger| {
            ledger
                .tombstone_stopped_child(&selector, child_id.as_deref(), reason)
                .map(|_| ())
        })
        .await
    }

    /// The children reachable in the ledger from `roots` that are not among
    /// them (pa-core's `walk_passive_rlm_children`), in walk order.
    ///
    /// # Errors
    ///
    /// The ledger could not be read.
    pub(crate) async fn passive_children(
        &self,
        roots: Vec<RosterWalkRoot>,
    ) -> anyhow::Result<Vec<PassiveRlmChild>> {
        self.on_ledger(move |ledger| walk_passive_rlm_children(ledger, &roots))
            .await
    }

    /// The live family the ledger records below the root transcript
    /// `root_file` (every depth, canonical endpoints, walk order): the
    /// roster's seed for an open root (the old supervisor's registration
    /// seed).
    ///
    /// # Errors
    ///
    /// The ledger could not be read.
    pub(crate) async fn family_below(
        &self,
        root_file: PathBuf,
    ) -> anyhow::Result<Vec<RlmLedgerEdge>> {
        self.on_ledger(move |ledger| {
            Ok(ledger
                .family_edges([root_file])?
                .into_iter()
                .filter(|edge| ledger.edge_is_live(&edge.child_id, &edge.child))
                .collect())
        })
        .await
    }

    /// The live children the ledger records under the parent transcript
    /// `parent_file`, in spawn order.
    ///
    /// # Errors
    ///
    /// The ledger could not be read (an oversized ledger).
    pub(crate) async fn children_of(
        &self,
        parent_file: PathBuf,
    ) -> anyhow::Result<Vec<RlmLedgerEdge>> {
        self.on_ledger(move |ledger| ledger.live_edges_of_parent(&parent_file))
            .await
    }
}
