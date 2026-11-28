//! The update restart on the runtime daemon (`docs/legacy-upgrade-contract.md`,
//! "Daemon and TUI handoff"): the old supervisor's `prepare_update_restart`
//! and `commit_update_restart` as the calls `daemon.prepare_update_restart`
//! and `daemon.commit_update_restart`, on pa-core's update transaction
//! (`pa_core::update::restart::UpdateRestart`), each answering the old
//! response's `data` or its refusal (message and `errorInfo`) verbatim.
//!
//! - **Prepare** (`{updateId, toVersion?}`): the transaction's drain waits
//!   for the calls in flight, then the roster snapshots every open session
//!   that keeps a transcript: its transcript and working directory, its lane
//!   (a running turn is `streaming`, the queued tasks its `steering` and
//!   `followUps`) and whether its family has live subagents; the subagent and
//!   heartbeat rows are pa-core's, as the old daemon's. There are no worker
//!   rows: the runtime's sessions run in its own process.
//! - **The gate.** While a transaction is active every mutating call
//!   ([`gated_as`], the old command it is) and every publish of input is
//!   refused with the old `Daemon is preparing an update restart` and
//!   `update_restarting` info, the abort family excepted while it drains; an
//!   admitted call counts against the drain until it is answered.
//! - **Commit** (`{updateId}`): `Prepared -> Stopping`, answered `{updateId,
//!   state: "stopping", stopped}` (the open sessions), then the daemon's stop
//!   (`super::lifecycle`), whose `system/closing` carries the old
//!   `daemon_closing`'s update payload (`reason: "update"`, `payload:
//!   {updateId, resume, estSeconds, sessions}`). The stop is the runtime's own
//!   drain, which never refuses: each running turn is cut off with its
//!   continuation queued, and the held sessions are kept for the next daemon.

use pa_core::update::restart::roster::{live_child_parents, process_identity, RosterInputs};
use pa_core::update::restart::{now_ms, UpdateRefusal, UpdateRestart};
use pa_types::daemon::{
    UpdateId, UpdateRoster, UpdateRosterInFlight, UpdateRosterQueue, UpdateRosterSession,
    UpdateRosterSessionKind,
};
use pa_types::event_log::{Priority, Topic};
use serde_json::{json, Map, Value};

use super::{optional_string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;
use crate::wire::{Call, ErrorWithInfo};

/// The old wire command each call is, for the update gate (its read-only and
/// drain tables are the old command types'); `None` for the update's own
/// calls, which the gate never refuses.
pub(super) fn gated_as(call: Call) -> Option<&'static str> {
    Some(match call {
        Call::DaemonPrepareUpdateRestart | Call::DaemonCommitUpdateRestart => return None,
        Call::SessionCreate => "create",
        Call::SessionStop => "kill",
        Call::SessionRename => "rename",
        Call::SessionDelete => "delete_saved_session",
        Call::SessionSetModel => "set_model",
        Call::SessionSetEffort => "set_thinking_level",
        Call::SessionFork => "fork_saved_session",
        Call::SessionLabel => "set_session_entry_label",
        Call::SessionExport => "export_jsonl",
        Call::SessionAbort => "abort",
        Call::SessionAbortAndSendQueued => "abort_and_send_queued",
        Call::SessionResumeQueue => "resume_queue",
        Call::SessionWithdraw => "withdraw_queued_message",
        Call::SessionRestore => "restore_next_turn",
        Call::SessionCompact => "compact",
        Call::SessionWaitIdle => "wait_for_idle",
        Call::SessionSend => "send_agent_message",
        Call::SessionSetMcpServers => "set_mcp_servers",
        Call::SessionBash => "bash",
        Call::SessionAbortBash => "abort_bash",
        Call::ScheduleAdd => "cron_add",
        Call::ScheduleCancel => "cron_cancel",
        Call::SessionPauseInput => "acquire_session_input_pause",
        Call::SessionReleaseInput => "release_session_input_pause",
        Call::SessionCancelTask => "cancel_queued_task",
        Call::SessionCancelChild => "cancel_rlm_child",
        Call::SessionCycleModel => "cycle_model",
        Call::SessionSetScopedModels => "set_scoped_models",
        Call::SessionCycleEffort => "cycle_thinking_level",
        Call::SessionSetServiceTier => "set_service_tier",
        Call::SessionSetTransport => "set_transport",
        Call::SessionSetSteeringMode => "set_steering_mode",
        Call::SessionSetFollowUpMode => "set_follow_up_mode",
        Call::SessionSetAutoCompaction => "set_auto_compaction",
        Call::SessionSetAutoRetry => "set_auto_retry",
        Call::SessionReload => "reload",
        Call::SessionMarkAnthropicWarningShown => "mark_anthropic_warning_shown",
        Call::SessionNavigateTree => "navigate_tree",
        Call::SessionAbortBranchSummary => "abort_branch_summary",
        Call::SessionExportHtml => "export_html",
        Call::SessionForkEntry => "fork",
        Call::SessionImportJsonl => "import_jsonl",
        Call::SessionMutateQueuedMessage => "mutate_queued_message",
        Call::SessionAbortCompaction => "abort_compaction",
        Call::SessionAbortRetry => "abort_retry",
        Call::SessionAttach => "attach",
        Call::SessionDetach => "detach",
        Call::SessionAnswerQuestion => "answer_question",
        Call::SessionStartSideQuestion => "start_side_question",
        Call::SessionAbortSideQuestion => "abort_side_question",
        Call::SessionRecordBetterOutcome => "record_better_outcome",
        Call::SessionKillKernelBash => "kill_kernel_bash",
        Call::SessionFactoryActivity => "factory_activity",
        Call::PersistentShellKill => "kill_persistent_shell",
        Call::AgentMessagesPause => "agent_messages_pause",
        Call::AgentMessagesResume => "agent_messages_resume",
        Call::AgentMessagesClear => "agent_messages_clear",
        Call::SessionSetRlmMaxDepth => "set_rlm_max_depth",
        Call::SessionDeleteChild => "delete_rlm_subagent",
        Call::SessionSetHeartbeat => "heartbeat_set",
        Call::SessionUpdateHeartbeat => "heartbeat_update",
        Call::SessionManageHeartbeat => "heartbeat_manage",
        Call::DaemonShutdown => "shutdown",
        Call::DaemonRestart => "restart",
    })
}

/// A refusal as the wire's error, its `errorInfo` the reply's `info`.
fn refused(refusal: UpdateRefusal) -> anyhow::Error {
    match refusal.info {
        Some(info) => anyhow::Error::new(ErrorWithInfo {
            message: refusal.message,
            info: serde_json::to_value(info).unwrap_or(Value::Null),
        }),
        None => anyhow::anyhow!(refusal.message),
    }
}

/// The gate in front of `command` (an old wire command type): the guard an
/// admitted mutating command holds until it is answered.
///
/// # Errors
///
/// A transaction is active and refuses the command.
pub(super) fn admit(
    update: &UpdateRestart,
    command: &str,
) -> anyhow::Result<Option<pa_core::update::restart::MutationGuard>> {
    update.admit(command).map_err(refused)
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// The gate in front of a client's publish on `topic`: input is the old
    /// `prompt`, and input for a session the daemon does not hold open is
    /// refused as the old daemon refused a prompt to an unknown session (a
    /// client re-attaches and sends it again, or keeps its draft).
    pub(super) fn admit_input(&self, topic: &Topic) -> anyhow::Result<()> {
        let mut levels = topic.levels();
        if let (Some("agent"), Some(session), Some("input"), None) =
            (levels.next(), levels.next(), levels.next(), levels.next())
        {
            drop(admit(&self.update, "prompt")?);
            anyhow::ensure!(
                self.runtime.is_open(session),
                "Unknown active session: {session}"
            );
        }
        Ok(())
    }

    /// `daemon.prepare_update_restart {updateId, toVersion?}`.
    pub(super) async fn prepare_update_restart(&self, args: &Value) -> anyhow::Result<Value> {
        let update_id = optional_string_arg(args, "updateId")?;
        let to_version = optional_string_arg(args, "toVersion")?
            .unwrap_or_else(|| self.boot.app_version.clone());
        self.update
            .prepare(update_id, |update_id, _deadline| {
                self.update_roster(update_id, to_version)
            })
            .await
            .map_err(refused)
    }

    /// The roster of the sessions the daemon holds open (see the module
    /// documentation).
    async fn update_roster(
        &self,
        update_id: UpdateId,
        to_version: String,
    ) -> anyhow::Result<UpdateRoster> {
        let mut open: Vec<(String, std::path::PathBuf)> = self
            .open
            .paths()
            .into_iter()
            .filter(|(id, _)| !self.open.ephemeral(id))
            .collect();
        open.sort();
        let sessions: Vec<(String, String, String, crate::tasks::LaneState)> = open
            .into_iter()
            .map(|(id, path)| {
                let cwd = self.open.cwd(&id).unwrap_or_default();
                let lane = self.runtime.lane(&id);
                (
                    id,
                    path.to_string_lossy().into_owned(),
                    cwd.to_string_lossy().into_owned(),
                    lane,
                )
            })
            .collect();
        let agent_dir = self.agent_dir.clone();
        let socket = self.update.socket_path().to_string_lossy().into_owned();
        let from_version = self.boot.app_version.clone();
        let build = move |ledger: &pa_core::session_engine::rlm_ledger::RlmSpawnLedger| {
            let child_parents = live_child_parents(ledger)?;
            let mut resident_files = std::collections::HashSet::new();
            let rows = sessions
                .into_iter()
                .map(|(id, path, cwd, lane)| {
                    let canonical = pa_core::session::lease::canonical_session_path(
                        std::path::Path::new(&path),
                    );
                    let row = session_row(id, path, cwd, &lane, child_parents.contains(&canonical));
                    resident_files.insert(canonical);
                    row
                })
                .collect();
            pa_core::update::restart::roster::assemble(
                RosterInputs {
                    update_id: &update_id,
                    socket_path: &socket,
                    agent_dir: &agent_dir,
                    supervisor: process_identity(format!("runtime:{}", std::process::id())),
                    from_version: &from_version,
                    to_version: &to_version,
                    created_at_ms: now_ms(),
                    ledger,
                },
                rows,
                Vec::new(),
                &resident_files,
            )
        };
        match self.runtime.subagents().daemon() {
            Some(family) => family.on_ledger(build).await,
            None => anyhow::bail!("the runtime daemon keeps no spawn ledger"),
        }
    }

    /// `daemon.commit_update_restart {updateId}`: answered, then the daemon
    /// stops for the update (see the module documentation).
    pub(super) fn commit_update_restart(&self, args: &Value) -> anyhow::Result<Value> {
        let update_id = self
            .update
            .commit(optional_string_arg(args, "updateId")?)
            .map_err(refused)?;
        let mut ids: Vec<String> = self.open.paths().into_keys().collect();
        ids.sort();
        let stopped = ids.len();
        // The old daemon's `name` is the one a session was created with; the
        // runtime's sessions are created without one.
        let sessions: Vec<Value> = ids
            .into_iter()
            .map(|id| json!({ "sessionId": id, "name": "" }))
            .collect();
        tracing::info!("update {update_id}: all {stopped} session(s) stop; exiting for the update");
        self.request_stop(self.update.closing_payload(&update_id, &sessions));
        Ok(UpdateRestart::commit_reply(&update_id, stopped))
    }
}

/// One open session's roster row.
fn session_row(
    id: String,
    path: String,
    cwd: String,
    lane: &crate::tasks::LaneState,
    rlm_children: bool,
) -> UpdateRosterSession {
    let (mut steering, mut follow_ups) = (Vec::new(), Vec::new());
    for task in &lane.queued {
        match task.priority {
            Priority::Steer => steering.push(task.prompt.clone()),
            Priority::Prompt | Priority::FollowUp | Priority::Event | Priority::Background => {
                follow_ups.push(task.prompt.clone());
            }
        }
    }
    let streaming = lane.running.is_some();
    let lanes_pending = !steering.is_empty() || !follow_ups.is_empty();
    UpdateRosterSession {
        session_id: id.clone(),
        active_session_id: id,
        // The old create command's form, so either daemon can open it again.
        runtime_config: json!({ "create": { "sessionPath": path, "cwd": cwd } }),
        session_file: path,
        name: None,
        kind: UpdateRosterSessionKind::TopLevel,
        parent_session_id: None,
        rlm_depth: 0,
        cwd,
        queue: UpdateRosterQueue {
            next_turn: Vec::new(),
            actions: json!({ "steering": steering, "followUps": follow_ups }),
        },
        in_flight: UpdateRosterInFlight {
            streaming,
            compacting: false,
            bash_running: false,
            rlm_children,
            retrying: false,
            prompt_in_flight: false,
        },
        should_resume: streaming || lanes_pending,
        rest: Map::default(),
    }
}
