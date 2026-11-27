//! What each call a cell can make reaches, for the trust gate: every host
//! request pa-core registers and every family of side-effecting natives,
//! each with exactly one class. The gate admits an untrusted turn's call by
//! its class alone; a request not listed here (one an embedder registers,
//! or one pa-core gained since) is [`CallClass::Privileged`]. The guard in
//! `tests.rs` fails while pa-core registers a request this table does not
//! list, or the table lists one it no longer registers.

use pa_core::kernel::NativeFamily;

/// What a call reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallClass {
    /// Reads files: admitted for an untrusted turn when every path its
    /// payload names resolves under the session's working directory.
    WorkspaceRead,
    /// The session's own paged-out context, its todo list, and the end of
    /// its own turn.
    OwnSession,
    /// A report the kernel sends about its own work: a count, a finished or
    /// quiet command's notice, a cell's completion. Admitted, so the work an
    /// owner's turn started still reports while an untrusted turn runs; what
    /// a cell could forge with one reaches only its own session.
    KernelReport,
    /// Acts for the user, or reaches past the session: runs commands, writes
    /// files, reaches the web (a URL can carry what the turn read out),
    /// spawns or messages agents, schedules, configures, observes other
    /// sessions, asks the user. Refused for an untrusted turn.
    Privileged,
}

/// Every host request pa-core registers, with its class.
pub(crate) const HOST_REQUESTS: &[(&str, CallClass)] = &[
    ("agent_message.list_agents", CallClass::Privileged),
    ("agent_message.send", CallClass::Privileged),
    ("agent_message.wait", CallClass::Privileged),
    ("agent_observe.get", CallClass::Privileged),
    ("agent_observe.list", CallClass::Privileged),
    ("agent_observe.recent", CallClass::Privileged),
    ("agent_observe.session_messages", CallClass::Privileged),
    ("bash.completed", CallClass::KernelReport),
    ("bash.consumed", CallClass::KernelReport),
    ("bash.quiet", CallClass::KernelReport),
    ("better.checks", CallClass::Privileged),
    ("better.evidence", CallClass::Privileged),
    ("better.run", CallClass::Privileged),
    ("call.refused", CallClass::KernelReport),
    ("cell.completed", CallClass::KernelReport),
    ("cell.consumed", CallClass::KernelReport),
    ("checkpoint.create", CallClass::Privileged),
    ("checkpoint.list", CallClass::Privileged),
    ("checkpoint.restore", CallClass::Privileged),
    ("command_cache.fact", CallClass::KernelReport),
    ("compact.run", CallClass::Privileged),
    ("compact.status", CallClass::Privileged),
    ("ctx.grep", CallClass::OwnSession),
    ("ctx.info", CallClass::OwnSession),
    ("ctx.page", CallClass::OwnSession),
    ("ctx.tail", CallClass::OwnSession),
    ("edit.fact", CallClass::KernelReport),
    ("edit_check.fact", CallClass::KernelReport),
    ("elevation.fact", CallClass::KernelReport),
    ("event.define_source", CallClass::Privileged),
    ("event.list", CallClass::Privileged),
    ("event.publish", CallClass::Privileged),
    ("event.remove_source", CallClass::Privileged),
    ("event.subscribe", CallClass::Privileged),
    ("event.unsubscribe", CallClass::Privileged),
    ("factory.progress", CallClass::KernelReport),
    ("fs.edit", CallClass::Privileged),
    ("fs.find", CallClass::WorkspaceRead),
    ("fs.glob", CallClass::WorkspaceRead),
    ("fs.grep", CallClass::WorkspaceRead),
    ("fs.read", CallClass::WorkspaceRead),
    ("fs.symbol", CallClass::Privileged),
    ("fs.write", CallClass::Privileged),
    ("goal.complete", CallClass::Privileged),
    ("goal.create", CallClass::Privileged),
    ("goal.get", CallClass::Privileged),
    ("hypotheses.record", CallClass::Privileged),
    ("mcp.begin_login", CallClass::Privileged),
    ("mcp.config", CallClass::Privileged),
    ("mcp.list_connections", CallClass::Privileged),
    ("mcp.list_plugins", CallClass::Privileged),
    ("mcp.refresh", CallClass::Privileged),
    ("mcp.search_plugins", CallClass::Privileged),
    ("model.apply_effort", CallClass::Privileged),
    ("model.info", CallClass::Privileged),
    ("model.set_effort", CallClass::Privileged),
    ("notify.send", CallClass::Privileged),
    ("question.ask", CallClass::Privileged),
    ("question.cancel", CallClass::Privileged),
    ("refine.run", CallClass::Privileged),
    ("refine.status", CallClass::Privileged),
    ("rizin.used", CallClass::KernelReport),
    ("rlm.abort", CallClass::Privileged),
    ("rlm.collect", CallClass::Privileged),
    ("rlm.create_session", CallClass::Privileged),
    ("rlm.decide", CallClass::Privileged),
    ("rlm.delete_subagent", CallClass::Privileged),
    ("rlm.end_turn", CallClass::OwnSession),
    ("rlm.find_models", CallClass::Privileged),
    ("rlm.list_subagents", CallClass::Privileged),
    ("rlm.progress.note", CallClass::Privileged),
    ("rlm.reload", CallClass::Privileged),
    ("rlm.rename", CallClass::Privileged),
    ("rlm.run", CallClass::Privileged),
    ("rlm.session.messages", CallClass::Privileged),
    ("rlm.session.saved_messages", CallClass::Privileged),
    ("rlm.session.search", CallClass::Privileged),
    ("rlm.session.tool_result", CallClass::Privileged),
    ("rlm_heartbeat.create", CallClass::Privileged),
    ("rlm_heartbeat.delete", CallClass::Privileged),
    ("rlm_heartbeat.list", CallClass::Privileged),
    ("rlm_heartbeat.update", CallClass::Privileged),
    ("schedule.add", CallClass::Privileged),
    ("schedule.cancel", CallClass::Privileged),
    ("schedule.list", CallClass::Privileged),
    ("settings.host", CallClass::Privileged),
    ("settings.session", CallClass::Privileged),
    ("shell.fact", CallClass::KernelReport),
    ("tests.summary", CallClass::KernelReport),
    ("todo.get", CallClass::OwnSession),
    ("todo.set", CallClass::OwnSession),
    ("todo.update", CallClass::OwnSession),
    ("usage.quotas", CallClass::Privileged),
    ("usage.session", CallClass::Privileged),
    ("web.automate", CallClass::Privileged),
    ("web.capture", CallClass::Privileged),
    ("web.crawl", CallClass::Privileged),
    ("web.extract", CallClass::Privileged),
    ("web.read", CallClass::Privileged),
    ("web.search", CallClass::Privileged),
    ("worktree.record", CallClass::Privileged),
];

/// The class of each family of side-effecting natives. The file readers
/// are admitted here because each sends its `fs.*` request, which the gate
/// checks with its paths.
pub(crate) fn native_class(family: NativeFamily) -> CallClass {
    match family {
        NativeFamily::FileRead => CallClass::WorkspaceRead,
        NativeFamily::Bash
        | NativeFamily::SymbolIndex
        | NativeFamily::FileWrite
        | NativeFamily::Worktree
        | NativeFamily::Hypotheses
        | NativeFamily::Harness
        | NativeFamily::Factory
        | NativeFamily::Rizin
        | NativeFamily::Mcp => CallClass::Privileged,
    }
}
