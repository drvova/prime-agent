//! The kernel capabilities' standalone adoption events: one rule file per capability (or per
//! pair of capabilities that share one event), each holding the events that capability's
//! manifest (`crates/pa-core/src/kernel/capabilities/<name>.toml`) claims in its `[[telemetry]]`
//! blocks. pa-core counts every property here through one generic capability counter and emits
//! these lists as they stand, so a new capability adds its manifest block and its rule file and
//! touches no shared counter struct.

mod bash;
mod better;
mod cells;
mod checkpoint;
mod core;
mod ctx;
mod decision;
mod fs;
mod hypothesis;
mod mcp_session;
mod notify_model;
mod resource_reload_settings;
mod rizin;
mod schedule;
mod todo_ask;
mod usage;
mod web;
mod worktree;

use super::EventRule;

/// The capability events emitted before `agent session ended`.
pub const CAPABILITY_EVENTS_BEFORE_SESSION_END: &[&EventRule] =
    &[&todo_ask::AGENT_TODOS_AND_QUESTIONS];

/// The capability events emitted after `agent session ended`, in emission order.
pub const CAPABILITY_EVENTS_AFTER_SESSION_END: &[&EventRule] = &[
    &better::AGENT_BETTER_PROMPT,
    &web::AGENT_WEB_READS,
    &fs::AGENT_FS_SEARCH,
    &fs::AGENT_FS_EDIT,
    &fs::AGENT_FS_REREADS,
    &schedule::AGENT_REPL_SCHEDULE,
    &checkpoint::AGENT_REPL_CHECKPOINT,
    &usage::AGENT_USAGE_READS,
    &worktree::AGENT_WORKTREES,
    &ctx::AGENT_CONTEXT_PAGING,
    &core::AGENT_RLM_CHILD_ABORT,
    &core::AGENT_RLM_DELEGATION_DEPTH,
    &resource_reload_settings::AGENT_REPL_RELOAD,
    &mcp_session::AGENT_KERNEL_LOGIN_AND_RECALL,
    &notify_model::AGENT_REPL_NOTIFY,
    &decision::AGENT_REPL_DECIDE,
    &cells::AGENT_REPL_BACKGROUND_CELLS,
    &ctx::AGENT_PREVIEWS,
    &fs::AGENT_SYMBOL_INDEX,
    &fs::AGENT_PREFETCH,
    &bash::AGENT_READONLY_CACHE,
    &hypothesis::AGENT_REPL_HYPOTHESES,
    &fs::AGENT_EDIT_CHECKS,
    &bash::AGENT_TEST_SUMMARIES,
    &rizin::AGENT_REPL_RIZIN,
    &bash::AGENT_BUILTIN_SHELL,
    &cells::AGENT_KERNEL_CALL_REFUSALS,
    &core::AGENT_RLM_COLLECT_WAITS,
    &cells::AGENT_KERNEL_WAITS,
];
