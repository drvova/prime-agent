//! The queries and calls a client may send, and what answers them.
//!
//! On the wire a query or call names its operation as a string and carries
//! its arguments as JSON (`pa_types::wire2::ClientFrame::{Query, Call}`).
//! The server parses the name into [`Query`] or [`Call`] here and refuses an
//! unknown one with an error reply, so a client newer or older than the
//! runtime loses that one request, not its connection (a typed enum in the
//! frame itself would fail the whole frame, which has no id to reply to).
//! The arguments and the result stay JSON: their shapes arrive with the
//! storage-backed implementations that answer them.

use std::future::Future;
use std::str::FromStr;

use serde_json::Value;

/// Something a client reads that the log does not stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Query {
    /// `sessions.list`: the saved and running sessions.
    SessionsList,
    /// `session.snapshot`: one session's state, to attach to it.
    SessionSnapshot,
    /// `transcript.page`: a page of a session's transcript.
    TranscriptPage,
    /// `settings`: the effective settings.
    Settings,
    /// `models`: the model catalog.
    Models,
    /// `sources`: the event sources and their timers.
    Sources,
    /// `subscriptions`: the subscriptions on the bus.
    Subscriptions,
    /// `tasks`: queued and running tasks.
    Tasks,
    /// `session.context`: the messages the session's next request is built
    /// from, with its model and thinking level.
    SessionContext,
    /// `session.usage`: the session's message counts, token spend and
    /// context use.
    SessionUsage,
    /// `session.tree`: the session's entries as a tree (id, parent, kind,
    /// label) and its leaf.
    SessionTree,
    /// `session.context_tree`: the session's place in its family with its
    /// spend and context use.
    SessionContextTree,
    /// `session.breakdown`: the context window's details (the next
    /// request's estimate by category, prompt-cache totals, compactions,
    /// spend).
    SessionBreakdown,
    /// `schedules`: the scheduled jobs of one session or of every session
    /// the runtime holds.
    Schedules,
    /// `session.children`: the roster of an open session's direct
    /// subagents.
    SessionChildren,
    /// `session.model`: an open session's model, thinking level and the
    /// levels its model supports.
    SessionModel,
    /// `session.system_prompt`: an open session's system prompt.
    SessionSystemPrompt,
    /// `runtime.roster`: every running session's roster entry; its changes
    /// follow on `runtime/roster`.
    Roster,
    /// `runtime.traffic`: the agent traffic kept so far; new entries follow
    /// on `runtime/traffic`.
    Traffic,
    /// `session.commands`: an open session's slash-command catalog (its
    /// prompt templates and skills).
    SessionCommands,
    /// `session.mcp_connections`: an open session's `/mcp` page (its MCP
    /// connections, service catalog and stored keys).
    SessionMcpConnections,
    /// `session.model_catalog`: the `/model` picker's catalog for an open
    /// session; a change the background refresh finds follows on
    /// `runtime/models`.
    SessionModelCatalog,
    /// `usage`: one half of the `/usage` page (the provider quotas or the
    /// local spend history).
    Usage,
    /// `session.fork_messages`: the user messages an open session can be
    /// forked before (the `/fork` selector's list).
    SessionForkMessages,
    /// `session.last_assistant_text`: the newest reply text on an open
    /// session's branch.
    SessionLastAssistantText,
    /// `session.kernel_bash`: an open session's kernel shell commands.
    SessionKernelBash,
    /// `session.kernel_bash_tail`: the newest output lines of one of an open
    /// session's kernel shell commands.
    SessionKernelBashTail,
    /// `persistent_shells`: every persistent kernel shell machine-wide, with
    /// the session it is assigned to.
    PersistentShells,
    /// `persistent_shells.tail`: the newest output lines of one persistent
    /// shell.
    PersistentShellTail,
    /// `agent_messages`: a session's agent-message safety status (the first
    /// running session's without one).
    AgentMessages,
    /// `session.rlm_max_depth`: a session's delegation depth bound and its
    /// source.
    SessionRlmMaxDepth,
    /// `session.heartbeat`: a session's live or paused heartbeat.
    SessionHeartbeat,
    /// `daemon.update_restore_status`: the old daemon's
    /// `update_restore_status`, the update boot's restore progress.
    DaemonUpdateRestoreStatus,
    /// `daemon.identity`: the old `daemon_hello`'s identity fields (the
    /// process, its restart owner, the release), which an update coordinator
    /// checks the daemon it drives by.
    DaemonIdentity,
    /// `system.history_import`: the boot's history import running now (its
    /// started frame without `type`), `null` while none runs; its frames
    /// follow on `system/history_import`.
    HistoryImport,
}

/// A name that is not a query or call this runtime answers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this runtime has no {kind} {name:?}")]
pub struct UnknownOperation {
    /// `query` or `call`.
    pub kind: &'static str,
    pub name: String,
}

impl FromStr for Query {
    type Err = UnknownOperation;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "sessions.list" => Query::SessionsList,
            "session.snapshot" => Query::SessionSnapshot,
            "transcript.page" => Query::TranscriptPage,
            "settings" => Query::Settings,
            "models" => Query::Models,
            "sources" => Query::Sources,
            "session.children" => Query::SessionChildren,
            "session.model" => Query::SessionModel,
            "session.system_prompt" => Query::SessionSystemPrompt,
            "subscriptions" => Query::Subscriptions,
            "tasks" => Query::Tasks,
            "session.context" => Query::SessionContext,
            "session.usage" => Query::SessionUsage,
            "session.tree" => Query::SessionTree,
            "session.context_tree" => Query::SessionContextTree,
            "session.breakdown" => Query::SessionBreakdown,
            "schedules" => Query::Schedules,
            "runtime.roster" => Query::Roster,
            "runtime.traffic" => Query::Traffic,
            "session.commands" => Query::SessionCommands,
            "session.mcp_connections" => Query::SessionMcpConnections,
            "session.model_catalog" => Query::SessionModelCatalog,
            "usage" => Query::Usage,
            "session.fork_messages" => Query::SessionForkMessages,
            "session.last_assistant_text" => Query::SessionLastAssistantText,
            "session.kernel_bash" => Query::SessionKernelBash,
            "session.kernel_bash_tail" => Query::SessionKernelBashTail,
            "persistent_shells" => Query::PersistentShells,
            "persistent_shells.tail" => Query::PersistentShellTail,
            "agent_messages" => Query::AgentMessages,
            "session.rlm_max_depth" => Query::SessionRlmMaxDepth,
            "session.heartbeat" => Query::SessionHeartbeat,
            "daemon.update_restore_status" => Query::DaemonUpdateRestoreStatus,
            "daemon.identity" => Query::DaemonIdentity,
            "system.history_import" => Query::HistoryImport,
            unknown => {
                return Err(UnknownOperation {
                    kind: "query",
                    name: unknown.to_string(),
                })
            }
        })
    }
}

/// A change a client asks for that is not an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// `session.create`
    SessionCreate,
    /// `session.stop`
    SessionStop,
    /// `session.rename`
    SessionRename,
    /// `session.delete`
    SessionDelete,
    /// `session.set_model`
    SessionSetModel,
    /// `session.set_effort`
    SessionSetEffort,
    /// `session.fork`
    SessionFork,
    /// `session.label`
    SessionLabel,
    /// `session.export`
    SessionExport,
    /// `session.abort`: aborts the running turn and parks the session's
    /// queue (nothing queued starts until `session.resume_queue`).
    SessionAbort,
    /// `session.abort_and_send_queued`: aborts the running turn and keeps
    /// the queue going: the steers queued run next, delivered together.
    SessionAbortAndSendQueued,
    /// `session.resume_queue`: starts a parked queue again.
    SessionResumeQueue,
    /// `session.withdraw`: takes the session's queued input back out of its
    /// queue and answers it.
    SessionWithdraw,
    /// `session.restore`: puts input back at the front of the session's
    /// queue as steers, delivered together as its next turn.
    SessionRestore,
    /// `session.compact`: compacts the session's context in its lane and
    /// answers how it went.
    SessionCompact,
    /// `session.wait_idle`: answers once the session is at rest: nothing
    /// queued or running in its lane, and every subagent below it settled.
    SessionWaitIdle,
    /// `session.send`: delivers a message to a session as an agent message
    /// from the client (or from another session), opening a saved session
    /// for it.
    SessionSend,
    /// `session.set_mcp_servers`: replaces the MCP servers one client (an
    /// owner id) supplies an open session.
    SessionSetMcpServers,
    /// `session.bash`: runs a user's `!` command on a session, its output
    /// streamed on the session's stream.
    SessionBash,
    /// `session.abort_bash`: kills the session's running `!` command.
    SessionAbortBash,
    /// `schedule.add`: schedules a prompt on a session.
    ScheduleAdd,
    /// `schedule.cancel`: cancels a scheduled job.
    ScheduleCancel,
    /// `session.pause_input`: holds an open session's input (nothing queued
    /// starts) for the calling connection, until released or the connection
    /// ends.
    SessionPauseInput,
    /// `session.release_input`: releases one input hold.
    SessionReleaseInput,
    /// `session.cancel_task`: cancels one task of a session: taken out of
    /// its queue, or its running turn aborted.
    SessionCancelTask,
    /// `session.cancel_child`: cancels the run of one of a session's
    /// subagents.
    SessionCancelChild,
    /// `session.cycle_model`: switches an open session to the next model of
    /// its scoped list or of the catalog.
    SessionCycleModel,
    /// `session.set_scoped_models`: sets the model list an open session's
    /// `session.cycle_model` steps through.
    SessionSetScopedModels,
    /// `session.cycle_effort`: steps an open session's thinking level
    /// through the levels its model supports.
    SessionCycleEffort,
    /// `session.set_service_tier`: sets an open session's service tier.
    SessionSetServiceTier,
    /// `session.set_transport`: sets the transport an open session's
    /// requests use.
    SessionSetTransport,
    /// `session.set_steering_mode`: sets how an open session reads its
    /// waiting steers.
    SessionSetSteeringMode,
    /// `session.set_follow_up_mode`: sets how an open session reads its
    /// waiting follow-ups.
    SessionSetFollowUpMode,
    /// `session.set_auto_compaction`: turns an open session's automatic
    /// compaction on or off.
    SessionSetAutoCompaction,
    /// `session.set_auto_retry`: turns the automatic retry of failed model
    /// requests on or off.
    SessionSetAutoRetry,
    /// `session.reload`: an open session reads its MCP inputs, skills,
    /// prompt templates and context files again.
    SessionReload,
    /// `session.mark_anthropic_warning_shown`: records that a client drew an
    /// open session's Anthropic subscription warning.
    SessionMarkAnthropicWarningShown,
    /// `session.navigate_tree`: moves an open session's leaf onto an entry of
    /// its tree, summarizing the branch it leaves when asked.
    SessionNavigateTree,
    /// `session.abort_branch_summary`: aborts the summary a navigation runs.
    SessionAbortBranchSummary,
    /// `session.export_html`: writes an open session as a standalone HTML
    /// file.
    SessionExportHtml,
    /// `session.fork_entry`: an open session goes on from a new transcript
    /// holding its path to an entry, under the same id.
    SessionForkEntry,
    /// `session.import_jsonl`: an open session goes on from a copy of a
    /// transcript file, under the same id.
    SessionImportJsonl,
    /// `session.mutate_queued_message`: deletes, moves or rewrites one row
    /// of an open session's queue view.
    SessionMutateQueuedMessage,
    /// `session.abort_compaction`: aborts the compaction an open session
    /// runs, manual or automatic.
    SessionAbortCompaction,
    /// `session.abort_retry`: stops the automatic retry an open session's
    /// turn waits on.
    SessionAbortRetry,
    /// `session.attach`: attaches the calling connection's client to an open
    /// session and answers the attach snapshot; a client that declares
    /// `user_questions` can answer its kernel questions until it detaches
    /// or its connection ends.
    SessionAttach,
    /// `session.detach`: the calling connection's client leaves an open
    /// session it attached to.
    SessionDetach,
    /// `session.answer_question`: replies to an open session's waiting
    /// kernel question.
    SessionAnswerQuestion,
    /// `session.start_side_question`: starts a `/btw` question or a
    /// `/better` rewrite beside an open session's turns, streamed on
    /// `agent/<id>/side_question`.
    SessionStartSideQuestion,
    /// `session.abort_side_question`: aborts the calling connection's side
    /// run of an open session.
    SessionAbortSideQuestion,
    /// `session.record_better_outcome`: counts what the user did with a
    /// `/better` rewrite.
    SessionRecordBetterOutcome,
    /// `session.kill_kernel_bash`: stops one of an open session's kernel
    /// shell commands.
    SessionKillKernelBash,
    /// `session.factory_activity`: one `/factory` action on an open
    /// session's kernel factory executor.
    SessionFactoryActivity,
    /// `persistent_shells.kill`: ends one persistent kernel shell's process
    /// group.
    PersistentShellKill,
    /// `agent_messages.pause`: refuses the messages delivered to a session
    /// (every running one without one) and takes its waiting ones out.
    AgentMessagesPause,
    /// `agent_messages.resume`: lets a session's (every running one's)
    /// agent messages in again.
    AgentMessagesResume,
    /// `agent_messages.clear`: takes the agent messages waiting on a session
    /// out.
    AgentMessagesClear,
    /// `session.set_rlm_max_depth`: fixes a session's delegation depth bound
    /// (and the global default).
    SessionSetRlmMaxDepth,
    /// `session.delete_child`: deletes one of a session's settled subagents.
    SessionDeleteChild,
    /// `session.set_heartbeat`: replaces a session's heartbeat.
    SessionSetHeartbeat,
    /// `session.update_heartbeat`: pauses, resumes or clears a session's
    /// heartbeat.
    SessionUpdateHeartbeat,
    /// `session.manage_heartbeat`: pauses, resumes or stops one heartbeat of
    /// a session, open or saved.
    SessionManageHeartbeat,
    /// `daemon.shutdown`: stops the daemon on purpose, once the reply is
    /// sent: every connection is told (`system/closing`), each session's
    /// running turn is cut off for the next daemon to continue, and the
    /// process exits cleanly, so its restart owner does not start it again.
    DaemonShutdown,
    /// `daemon.restart`: the old daemon's `restart`, which stops it exactly
    /// as `daemon.shutdown` does; the client that asked starts the next one.
    DaemonRestart,
    /// `daemon.prepare_update_restart`: the old daemon's
    /// `prepare_update_restart`, the update coordinator's prepare (the
    /// sessions' roster written for the next daemon).
    DaemonPrepareUpdateRestart,
    /// `daemon.commit_update_restart`: the old daemon's
    /// `commit_update_restart`, which stops the daemon for the update once
    /// answered.
    DaemonCommitUpdateRestart,
}

impl FromStr for Call {
    type Err = UnknownOperation;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "session.create" => Call::SessionCreate,
            "session.stop" => Call::SessionStop,
            "session.rename" => Call::SessionRename,
            "session.delete" => Call::SessionDelete,
            "session.set_model" => Call::SessionSetModel,
            "session.set_effort" => Call::SessionSetEffort,
            "session.fork" => Call::SessionFork,
            "session.label" => Call::SessionLabel,
            "session.export" => Call::SessionExport,
            "session.abort" => Call::SessionAbort,
            "session.abort_and_send_queued" => Call::SessionAbortAndSendQueued,
            "session.resume_queue" => Call::SessionResumeQueue,
            "session.withdraw" => Call::SessionWithdraw,
            "session.restore" => Call::SessionRestore,
            "session.compact" => Call::SessionCompact,
            "session.wait_idle" => Call::SessionWaitIdle,
            "session.send" => Call::SessionSend,
            "session.set_mcp_servers" => Call::SessionSetMcpServers,
            "session.bash" => Call::SessionBash,
            "session.abort_bash" => Call::SessionAbortBash,
            "schedule.add" => Call::ScheduleAdd,
            "schedule.cancel" => Call::ScheduleCancel,
            "session.pause_input" => Call::SessionPauseInput,
            "session.release_input" => Call::SessionReleaseInput,
            "session.cancel_task" => Call::SessionCancelTask,
            "session.cancel_child" => Call::SessionCancelChild,
            "session.cycle_model" => Call::SessionCycleModel,
            "session.set_scoped_models" => Call::SessionSetScopedModels,
            "session.cycle_effort" => Call::SessionCycleEffort,
            "session.set_service_tier" => Call::SessionSetServiceTier,
            "session.set_transport" => Call::SessionSetTransport,
            "session.set_steering_mode" => Call::SessionSetSteeringMode,
            "session.set_follow_up_mode" => Call::SessionSetFollowUpMode,
            "session.set_auto_compaction" => Call::SessionSetAutoCompaction,
            "session.set_auto_retry" => Call::SessionSetAutoRetry,
            "session.reload" => Call::SessionReload,
            "session.mark_anthropic_warning_shown" => Call::SessionMarkAnthropicWarningShown,
            "session.navigate_tree" => Call::SessionNavigateTree,
            "session.abort_branch_summary" => Call::SessionAbortBranchSummary,
            "session.export_html" => Call::SessionExportHtml,
            "session.fork_entry" => Call::SessionForkEntry,
            "session.import_jsonl" => Call::SessionImportJsonl,
            "session.mutate_queued_message" => Call::SessionMutateQueuedMessage,
            "session.abort_compaction" => Call::SessionAbortCompaction,
            "session.abort_retry" => Call::SessionAbortRetry,
            "session.attach" => Call::SessionAttach,
            "session.detach" => Call::SessionDetach,
            "session.answer_question" => Call::SessionAnswerQuestion,
            "session.start_side_question" => Call::SessionStartSideQuestion,
            "session.abort_side_question" => Call::SessionAbortSideQuestion,
            "session.record_better_outcome" => Call::SessionRecordBetterOutcome,
            "session.kill_kernel_bash" => Call::SessionKillKernelBash,
            "session.factory_activity" => Call::SessionFactoryActivity,
            "persistent_shells.kill" => Call::PersistentShellKill,
            "agent_messages.pause" => Call::AgentMessagesPause,
            "agent_messages.resume" => Call::AgentMessagesResume,
            "agent_messages.clear" => Call::AgentMessagesClear,
            "session.set_rlm_max_depth" => Call::SessionSetRlmMaxDepth,
            "session.delete_child" => Call::SessionDeleteChild,
            "session.set_heartbeat" => Call::SessionSetHeartbeat,
            "session.update_heartbeat" => Call::SessionUpdateHeartbeat,
            "session.manage_heartbeat" => Call::SessionManageHeartbeat,
            "daemon.shutdown" => Call::DaemonShutdown,
            "daemon.restart" => Call::DaemonRestart,
            "daemon.prepare_update_restart" => Call::DaemonPrepareUpdateRestart,
            "daemon.commit_update_restart" => Call::DaemonCommitUpdateRestart,
            unknown => {
                return Err(UnknownOperation {
                    kind: "call",
                    name: unknown.to_string(),
                })
            }
        })
    }
}

/// A handler's error with what a client can act on beyond its message
/// (`Outcome::Error`'s `info`, the old daemon's `errorInfo`): the reply
/// carries `info` when this is anywhere in the error's chain.
#[derive(Debug)]
pub(crate) struct ErrorWithInfo {
    pub(crate) message: String,
    pub(crate) info: Value,
}

impl std::fmt::Display for ErrorWithInfo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ErrorWithInfo {}

/// One client connection of the wire server, as its calls and its end name
/// it: what a call makes a connection hold (an input hold) is let go when
/// the connection ends ([`Handlers::disconnected`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub(crate) u64);

/// Answers the queries and calls of the wire's clients.
///
/// The server runs each query or call on its own task, up to a few at once
/// per connection, so a slow answer delays neither the connection's events
/// nor its publishes; replies may therefore arrive out of order, matched by
/// their ids. `Ok(result)` becomes the reply's `result`, and an error its
/// `message` (with its causes), so an error must say what went wrong in
/// words a user can act on. A query must not change anything. Neither may
/// wait on a client: answer from storage or memory and return.
pub trait Handlers: Send + Sync + 'static {
    /// Answers `query` with `args`.
    fn query(
        &self,
        query: Query,
        args: Value,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;

    /// Makes the change `call` with `args`, sent on `connection`, asks for and
    /// says what it did.
    fn call(
        &self,
        call: Call,
        args: Value,
        connection: ConnectionId,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;

    /// `connection` ended (its calls still running were dropped): lets go of
    /// what its calls made it hold. Called once per connection, after its
    /// last call.
    fn disconnected(&self, connection: ConnectionId) -> impl Future<Output = ()> + Send;

    /// Whether a client's publish on `topic` (one of the client topics) may
    /// go ahead now; asked before it is published, and an error refuses it
    /// with its message. It must not block.
    ///
    /// # Errors
    ///
    /// The publish is refused now (the runtime daemon's update gate).
    fn admit_publish(&self, topic: &pa_types::event_log::Topic) -> anyhow::Result<()>;

    /// Resolves once what a client's publish on `topic` set off has reached
    /// the bus as far as it reached before the publish returned (a session's
    /// stream events queued ahead of it): the publish's reply goes out after
    /// them, so a client never reads the reply before the events that
    /// preceded it, as the old wire's one ordered channel guaranteed.
    fn published(&self, topic: &pa_types::event_log::Topic) -> impl Future<Output = ()> + Send;
}

/// Shared handlers answer as the handlers they share: the runtime daemon's
/// serve its connections and its own feeds.
impl<H: Handlers> Handlers for std::sync::Arc<H> {
    fn query(
        &self,
        query: Query,
        args: Value,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send {
        H::query(self, query, args)
    }

    fn call(
        &self,
        call: Call,
        args: Value,
        connection: ConnectionId,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send {
        H::call(self, call, args, connection)
    }

    fn disconnected(&self, connection: ConnectionId) -> impl Future<Output = ()> + Send {
        H::disconnected(self, connection)
    }

    fn admit_publish(&self, topic: &pa_types::event_log::Topic) -> anyhow::Result<()> {
        H::admit_publish(self, topic)
    }

    fn published(&self, topic: &pa_types::event_log::Topic) -> impl Future<Output = ()> + Send {
        H::published(self, topic)
    }
}
