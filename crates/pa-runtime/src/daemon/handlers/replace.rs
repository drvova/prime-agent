//! The whole-session replacements, the old worker's commands of the same
//! names: an open session goes on from another transcript under the same id
//! ([`crate::runtime::Runtime::replace_session`]: its engine and kernel are
//! built again from that transcript, as a crash's rebuild is, and it keeps
//! its topics' subscribers, its stream and its tasks). Its lane is held for
//! the replacement and a running turn is aborted first; the daemon holds the
//! new transcript's lease from then on and lets the old one go. The rules
//! are pa-core's (`pa_core::session::replacement`):
//!
//! - `session.fork_entry {session, entryId, position?}` (`fork`): the
//!   session's path to the entry is copied into a new transcript beside its
//!   own, which the session goes on from: up to and with the entry for
//!   `position: "at"`, else up to a user message's parent, whose text comes
//!   back (`{cancelled: false, selectedText?}`). An ephemeral session's
//!   context is cut back to that path in place.
//! - `session.import_jsonl {session, inputPath, cwdOverride?}`
//!   (`import_jsonl`): the transcript file is copied into the sessions
//!   directory under its own name, and the session goes on from the copy in
//!   its working directory (`cwdOverride`, else the one it names):
//!   `{cancelled: false}`. A missing file, or a working directory that does
//!   not exist, is refused with the old `errorInfo` (the reply's `info`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session::lease::{acquire_runtime_session_lease, SessionLease};
use pa_core::session::replacement::{
    import_target, replacement_cwd, ForkPoint, ReplacementRefused,
};
use pa_core::session::store::store;
use serde_json::{json, Value};

use super::{optional_string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;
use crate::wire::ErrorWithInfo;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.fork_entry` (see the module documentation).
    pub(super) async fn fork_entry(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let entry_id = optional_string_arg(args, "entryId")?.unwrap_or_default();
        let position = optional_string_arg(args, "position")?;
        let _hold = self.hold_for_move(&id, "fork").await?;
        let live = self.runtime.built_engine(&id).await?;
        let engine = Arc::clone(&live.engine);
        let ephemeral = self.open.ephemeral(&id);
        let (point, source) = live
            .runtime
            .spawn(async move {
                let persistence = engine.session.shared_persistence();
                let mut manager = persistence.lock().await;
                manager.ensure_full_history().await?;
                let target = manager
                    .get_entry_by_id(&entry_id)
                    .map(serde_json::to_value)
                    .transpose()?;
                let point = ForkPoint::of(target.as_ref(), position.as_deref())?;
                if ephemeral {
                    // Nothing on disk to copy: the context is cut back to
                    // the path in place.
                    let branch = point.leaf.as_deref().map_or_else(Vec::new, |leaf| {
                        manager
                            .get_branch(Some(leaf))
                            .into_iter()
                            .cloned()
                            .collect()
                    });
                    drop(manager);
                    engine.session.discard_compact_auto_refine();
                    engine.session.rebuild_branch_context(branch).await?;
                    return anyhow::Ok((point, None));
                }
                // The copy reads the transcript from disk: every row the
                // session holds is written first.
                manager.flush_now()?;
                let source = manager
                    .get_session_file()
                    .map(Path::to_path_buf)
                    .context("the session has no transcript to fork")?;
                anyhow::Ok((point, Some(source)))
            })
            .await
            .context("the session's runtime ended during the fork")??;
        let Some(source) = source else {
            return Ok(point.reply());
        };
        let (leaf, agent_dir) = (point.leaf.clone(), self.agent_dir.clone());
        let (forked, lease) = tokio::task::spawn_blocking(move || {
            let session_dir = source.parent().unwrap_or(Path::new("")).to_path_buf();
            let forked = pa_core::session::manager::SessionManager::create_branched_file(
                &source,
                leaf.as_deref(),
                &session_dir,
            )
            .map_err(anyhow::Error::msg)?;
            let lease = acquire_runtime_session_lease(&forked, &agent_dir)?;
            anyhow::Ok((forked, lease))
        })
        .await
        .context("the fork's thread panicked")??;
        let cwd = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
        self.replace_open(&id, forked, cwd, Some(lease)).await?;
        Ok(point.reply())
    }

    /// `session.import_jsonl` (see the module documentation).
    pub(super) async fn import_jsonl(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let input = PathBuf::from(optional_string_arg(args, "inputPath")?.unwrap_or_default());
        let cwd_override = optional_string_arg(args, "cwdOverride")?;
        // An ephemeral session keeps no transcript to replace.
        let current = self.open.path(&id).filter(|_| !self.open.ephemeral(&id));
        let fallback = self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone());
        let target =
            import_target(&input, current.as_deref().and_then(Path::parent)).map_err(refused)?;
        let current = current.unwrap_or_default();
        if let Some(other) = self.open.id_of(&target).filter(|other| *other != id) {
            anyhow::bail!(
                "session {other} is open on {}: stop it before importing over it",
                target.display()
            );
        }
        let agent_dir = self.agent_dir.clone();
        let (target, cwd, lease) = tokio::task::spawn_blocking(move || {
            let canonical = pa_core::session::lease::canonical_session_path;
            // The transcript the session holds already keeps its lease.
            let lease: Option<SessionLease> = if canonical(&target) == canonical(&current) {
                None
            } else {
                Some(acquire_runtime_session_lease(&target, &agent_dir)?)
            };
            // An import of the file in place copies nothing.
            if canonical(&target) != canonical(&input) {
                store().import(&input, &target)?;
            }
            store().repair_tail(&target);
            let header = pa_core::session::manager::read_session_header(&target)
                .with_context(|| format!("no session header in {}", target.display()))?;
            let cwd =
                replacement_cwd(&target, &header.cwd, cwd_override, &fallback).map_err(refused)?;
            let cwd = cwd.map_or(fallback, PathBuf::from);
            anyhow::Ok((target, cwd, lease))
        })
        .await
        .context("the import's thread panicked")??;
        let _hold = self.hold_for_move(&id, "import").await?;
        self.replace_open(&id, target, cwd, lease).await?;
        Ok(json!({ "cancelled": false }))
    }

    /// Open session `id` goes on from `transcript` in `cwd`, held by `lease`
    /// (`None` keeps its own): the daemon holds it on the new transcript
    /// first, so what reads it during the rebuild reads that one, then its
    /// engine is built again from it and its roster entry is due.
    async fn replace_open(
        &self,
        id: &str,
        transcript: PathBuf,
        cwd: PathBuf,
        lease: Option<SessionLease>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.open
                .replace(id, transcript.clone(), cwd.clone(), lease)
                .await,
            "session {id} is not open in the runtime daemon"
        );
        let replaced = self.runtime.replace_session(id, transcript, cwd).await;
        self.feeds.due(id);
        replaced
    }
}

/// A replacement pa-core refused, as the reply's error: its message, with
/// its `errorInfo` as the reply's `info` when it has one.
fn refused(refused: ReplacementRefused) -> anyhow::Error {
    match refused.info.as_ref().map(serde_json::to_value) {
        Some(Ok(info)) => anyhow::Error::new(ErrorWithInfo {
            message: refused.message,
            info,
        }),
        Some(Err(_)) | None => anyhow::Error::new(refused),
    }
}
