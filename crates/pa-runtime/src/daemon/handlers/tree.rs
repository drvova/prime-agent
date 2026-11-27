//! A session's tree moves and the reads beside them, the old worker's
//! commands of the same names, each answering the old response's `data`
//! (`null` for one that had none); the rules are pa-core's
//! (`pa_core::session::navigation`, `branch_summarization`, `export_html`):
//!
//! - `session.fork_messages {session}` (`get_user_messages_for_forking`):
//!   `{messages: [{entryId, text}]}`, the transcript's user messages with
//!   text, in file order.
//! - `session.last_assistant_text {session}` (`get_last_assistant_text`):
//!   `{text}`, the newest assistant text on the session's branch as the model
//!   sees it (`null` when there is none).
//! - `session.navigate_tree {session, targetId, summarize?,
//!   customInstructions?, replaceInstructions?, label?, restoreFiles?}`
//!   (`navigate_tree`): the open session's leaf moves onto the entry (a user
//!   or custom message is re-entered: the leaf goes to its parent and the
//!   text comes back as `editorText`). Its lane is held for the move, and a
//!   running turn is aborted first. With `summarize`, the branch it leaves
//!   is summarized on the session's model first and recorded as a
//!   `branch_summary` entry (`summaryEntry`), labelled with `label`; without
//!   one, `label` labels the target. The session's context is rebuilt onto
//!   the new branch, and its goal reloaded from it (a change is the old
//!   `goal_update` on the session's stream). A summary aborted answers
//!   `{cancelled: true, aborted: true}`; the target being the leaf already,
//!   `{cancelled: false}`. The runtime daemon keeps no workspace
//!   checkpoints, so `restoreFiles` restores nothing and the answer carries
//!   no `files`.
//! - `session.abort_branch_summary {session}` (`abort_branch_summary`): aborts
//!   the summary a navigation of the session runs, if one does.
//! - `session.export_html {session, outputPath?}` (`export_html`): `{path}`,
//!   the open session written as the standalone HTML viewer (every entry, its
//!   leaf, its system prompt and tools, the settings' theme), to `outputPath`
//!   as given or to the default name beside the daemon.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use pa_agent::abort::AbortController;
use pa_core::session::navigation::{self, NavigateRequest};
use pa_core::session_engine::branch_summarization::{
    collect_entries_for_branch_summary, run_branch_summary, BranchSummaryOutcome,
    BranchSummaryRequest,
};
use pa_core::session_engine::goal_driver::GoalBranchReload;
use pa_types::session::FileEntry;
use serde_json::{json, Value};

use super::attach::Transcript;
use super::{optional_string_arg, RuntimeHandlers};
use crate::lock;
use crate::runtime::Runtime;
use crate::session::SessionBuilder;

/// The abort handles of the branch summaries navigations run, by session.
pub(super) type BranchSummaries =
    std::sync::Mutex<std::collections::HashMap<String, Arc<AbortController>>>;

/// A hold on a session's lane for one tree move, released when it ends.
struct MoveHold<B: SessionBuilder> {
    runtime: Arc<Runtime<B>>,
    session: String,
    hold: String,
}

impl<B: SessionBuilder> Drop for MoveHold<B> {
    fn drop(&mut self) {
        self.runtime.release_input(&self.session, &self.hold);
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.fork_messages` (see the module documentation).
    pub(super) async fn fork_messages(&self, args: &Value) -> anyhow::Result<Value> {
        let (_, path, open) = self.session(args)?;
        let rows = tokio::task::spawn_blocking(move || super::transcript_rows(&path, open))
            .await
            .context("the transcript's thread panicked")??;
        Ok(navigation::fork_messages(&rows))
    }

    /// `session.last_assistant_text` (see the module documentation).
    pub(super) async fn last_assistant_text(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        if open {
            // The session's context as its engine holds it: rows it has not
            // stored yet included.
            let live = self.runtime.built_engine(&id).await?;
            let persistence = live.engine.session.shared_persistence();
            return live
                .runtime
                .spawn(async move {
                    let messages: Vec<Value> = persistence
                        .lock()
                        .await
                        .active_context()
                        .messages
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<Result<_, _>>()?;
                    anyhow::Ok(navigation::last_assistant_text(messages.iter()))
                })
                .await
                .context("the session's runtime ended during the read")?;
        }
        // A saved session's leaf is its last row's.
        let transcript = tokio::task::spawn_blocking(move || Transcript::read(&path, false, None))
            .await
            .context("the transcript's thread panicked")??;
        Ok(navigation::last_assistant_text(
            transcript.context_messages().iter(),
        ))
    }

    /// `session.abort_branch_summary` (see the module documentation).
    pub(super) fn abort_branch_summary(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        if let Some(controller) = lock(&self.branch_summaries).get(&id) {
            controller.abort();
        }
        Ok(Value::Null)
    }

    /// `session.navigate_tree` (see the module documentation).
    #[expect(
        clippy::too_many_lines,
        reason = "a navigation's steps read in the order they run: the tree read, the hold, the \
                  summary, the move, the rebuild"
    )]
    pub(super) async fn navigate_tree(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let request = NavigateRequest::from_payload(args);
        let live = self.runtime.built_engine(&id).await?;
        let persistence = live.engine.session.shared_persistence();
        let target_id = request.target_id.clone();
        let (target, old_leaf, entries, session_id) = live
            .runtime
            .spawn(async move {
                let mut manager = persistence.lock().await;
                manager.ensure_full_history().await?;
                let target = manager
                    .get_entry_by_id(&target_id)
                    .with_context(|| format!("Entry {target_id} not found"))?;
                let target = serde_json::to_value(target)?;
                let entries: Vec<FileEntry> = manager
                    .get_all_entries()
                    .iter()
                    .filter(|entry| !matches!(entry, FileEntry::Header { .. }))
                    .cloned()
                    .collect();
                anyhow::Ok((
                    target,
                    manager.get_leaf_id().map(str::to_string),
                    entries,
                    manager.get_session_id().to_string(),
                ))
            })
            .await
            .context("the session's runtime ended during the read")??;
        if old_leaf.as_deref() == Some(request.target_id.as_str()) {
            return Ok(json!({ "cancelled": false }));
        }
        // Nothing queued starts on the branch being left, and a running turn
        // ends first.
        let hold = MoveHold {
            runtime: Arc::clone(&self.runtime),
            session: id.clone(),
            hold: format!("navigate:{}", uuid::Uuid::new_v4()),
        };
        self.runtime.hold_input(&hold.session, &hold.hold);
        self.runtime.interrupt(&id).await?;
        let (new_leaf, editor_text) = navigation::navigation_point(&target);

        let mut summary = None;
        if request.summarize {
            let collected = collect_entries_for_branch_summary(
                &entries,
                old_leaf.as_deref(),
                &request.target_id,
            );
            if !collected.entries.is_empty() {
                let provider = live
                    .provider_target
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .with_context(|| format!("session {id} has no model to summarize with"))?;
                let controller = Arc::new(AbortController::new());
                lock(&self.branch_summaries).insert(id.clone(), Arc::clone(&controller));
                let summarize = BranchSummaryRequest {
                    entries: collected.entries,
                    custom_instructions: request.custom_instructions.clone(),
                    replace_instructions: request.replace_instructions,
                    session_id,
                };
                let dirs = (
                    self.open.cwd(&id).unwrap_or_else(|| self.cwd.clone()),
                    self.agent_dir.clone(),
                );
                let signal = controller.signal();
                let outcome = live
                    .runtime
                    .spawn(async move {
                        run_branch_summary(
                            summarize,
                            &provider.model,
                            provider.api_key,
                            dirs,
                            &signal,
                        )
                        .await
                    })
                    .await;
                {
                    let mut summaries = lock(&self.branch_summaries);
                    if summaries
                        .get(&id)
                        .is_some_and(|live| Arc::ptr_eq(live, &controller))
                    {
                        summaries.remove(&id);
                    }
                }
                match outcome.context("the session's runtime ended during the summary")? {
                    BranchSummaryOutcome::Complete { run } => summary = Some(run),
                    BranchSummaryOutcome::Aborted => {
                        return Ok(json!({ "cancelled": true, "aborted": true }));
                    }
                    BranchSummaryOutcome::Failed { error } => anyhow::bail!(error),
                }
            }
        }

        // The leaf moves and the summary is recorded, then the context is
        // rebuilt onto the new branch.
        let summarized = summary.is_some();
        let (label, target_id) = (request.label.clone(), request.target_id.clone());
        let engine = Arc::clone(&live.engine);
        let (summary_entry, goal) = live
            .runtime
            .spawn(async move {
                let persistence = engine.session.shared_persistence();
                let mut manager = persistence.lock().await;
                if let Some(leaf) = new_leaf.as_deref() {
                    anyhow::ensure!(
                        manager.get_entry_by_id(leaf).is_some(),
                        "Entry {leaf} not found"
                    );
                }
                let summary_entry = match summary {
                    Some(run) => {
                        let usage = run.usage.map(serde_json::from_value).transpose()?;
                        let summary_id = manager.branch_with_summary(
                            new_leaf.as_deref(),
                            &run.summary,
                            run.details,
                            None,
                            usage,
                            run.model
                                .as_ref()
                                .map(|(provider, model_id)| (provider.as_str(), model_id.as_str())),
                        )?;
                        if let Some(label) = &label {
                            manager.append_label_change(&summary_id, Some(label))?;
                        }
                        manager
                            .get_entry_by_id(&summary_id)
                            .map(serde_json::to_value)
                            .transpose()?
                    }
                    None => {
                        match new_leaf.as_deref() {
                            Some(leaf) => manager.branch(leaf),
                            None => manager.reset_leaf(),
                        }
                        if let Some(label) = &label {
                            manager.append_label_change(&target_id, Some(label))?;
                        }
                        None
                    }
                };
                drop(manager);
                // The moved branch invalidates an armed review.
                engine.session.discard_compact_auto_refine();
                engine.session.rebuild_context_at_leaf().await;
                // A summary is a compaction-style cut; a plain move reloads
                // the branch as it was.
                let reload = if summarized {
                    GoalBranchReload::SameTimeline
                } else {
                    GoalBranchReload::FaithfulBranch
                };
                let before = engine.goal_state().await;
                let mut driver = engine.goal_driver.lock().await;
                driver.reload_from_branch(&*persistence.lock().await, reload);
                let after = driver.state_with_creation_elapsed();
                drop(driver);
                let changed = pa_core::goals::goal_update_dedupe_projection(&before)
                    != pa_core::goals::goal_update_dedupe_projection(&after);
                anyhow::Ok((summary_entry, changed.then_some(after)))
            })
            .await
            .context("the session's runtime ended during the move")??;
        drop(hold);
        if let Some(goal) = goal {
            (self.runtime.stream_sender(&id)?)(json!({ "type": "goal_update", "goal": goal }));
        }
        let mut data = json!({ "cancelled": false });
        if let Some(editor_text) = editor_text {
            data["editorText"] = json!(editor_text);
        }
        if let Some(summary_entry) = summary_entry {
            data["summaryEntry"] = summary_entry;
        }
        Ok(data)
    }

    /// `session.export_html` (see the module documentation).
    pub(super) async fn export_html(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let output = optional_string_arg(args, "outputPath")?;
        let live = self.runtime.built_engine(&id).await?;
        let engine = Arc::clone(&live.engine);
        let agent_dir = self.agent_dir.clone();
        let path = live
            .runtime
            .spawn(async move {
                let persistence = engine.session.shared_persistence();
                let mut manager = persistence.lock().await;
                let session_file = manager
                    .get_session_file()
                    .map(Path::to_path_buf)
                    .context("Cannot export an in-memory session")?;
                manager.ensure_full_history().await?;
                let mut header = Value::Null;
                let mut entries = Vec::new();
                for entry in manager.get_all_entries() {
                    let value = serde_json::to_value(entry)?;
                    match entry {
                        FileEntry::Header { .. } => header = value,
                        _ => entries.push(value),
                    }
                }
                let leaf_id = manager.get_leaf_id().map(str::to_string);
                let cwd = manager.get_cwd().to_path_buf();
                drop(manager);
                let tools = engine.session.agent().state().await.tools;
                let rendered_tools = pa_core::export_html::pre_render_custom_tools(
                    &entries,
                    &pa_core::export_html::RegistryToolRenderer { tools: &tools },
                );
                let data = pa_core::export_html::SessionExportData {
                    header,
                    entries,
                    leaf_id,
                    system_prompt: Some(engine.system_prompt.clone()),
                    tools: Some(pa_core::export_html::tools_section(&tools)),
                    rendered_tools,
                };
                tokio::task::spawn_blocking(move || {
                    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
                    pa_core::export_html::export_session_to_html(
                        &data,
                        settings.get_theme(),
                        &agent_dir,
                        &session_file,
                        output.as_deref(),
                    )
                })
                .await
                .context("the export's thread panicked")?
            })
            .await
            .context("the session's runtime ended during the export")??;
        Ok(json!({ "path": path }))
    }
}
