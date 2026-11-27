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
//!   `deleted`.
//! - **Passive children**: the saved catalog walks the ledger from its rows
//!   and the sessions the runtime runs for the children no root it runs or
//!   lists keeps (pa-core's `rlm_roster`, the old supervisor's walk).
//!
//! The ledger's own writes are durable, single `O_APPEND` records, and run
//! on the blocking pool. A family with no transcripts (a parent kept in
//! memory) writes nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session_engine::rlm_host::RlmFactoryTag;
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

/// The runtime daemon's family records: its sessions dir's spawn ledger.
pub(crate) struct DaemonFamily {
    ledger: Arc<RlmSpawnLedger>,
}

impl DaemonFamily {
    /// The records of the sessions under `sessions_dir`, the ledger kept
    /// under `agent_dir`'s `rlm-ledger/`.
    pub(crate) fn new(agent_dir: &Path, sessions_dir: &Path) -> Self {
        DaemonFamily {
            ledger: Arc::new(RlmSpawnLedger::new(agent_dir, sessions_dir, |message| {
                tracing::warn!("{message}");
            })),
        }
    }

    /// Runs `write` on the ledger on the blocking pool.
    async fn on_ledger<T: Send + 'static>(
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

    /// Records a child's delete: its edge tombstoned and its display entry
    /// `deleted`.
    pub(crate) async fn deleted(&self, child_id: String, child_file: String, child_dir: String) {
        let tombstoned = {
            let child_id = child_id.clone();
            self.on_ledger(move |ledger| {
                ledger.append_delete(&child_id, &child_file, RlmLedgerDeleteReason::User)
            })
            .await
        };
        if let Err(error) = tombstoned {
            tracing::warn!("the RLM ledger delete was not written: {error:#}");
        }
        self.display_status(child_dir, child_id, "deleted").await;
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
