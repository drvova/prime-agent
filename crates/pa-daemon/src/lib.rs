//! Session supervisor, worker processes, and wire protocol for Prime Agent:
//! the supervisor spawns one worker process per active session, supervises
//! restarts with backoff, and routes clients; sessions persist as
//! append-only JSONL in the same layout as the TS product.
#![expect(
    clippy::large_futures,
    reason = "stack-resident futures on the hot paths by design; boxing the call sites costs speed"
)]
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "64-bit targets; the narrowing casts sit at OS boundaries (pid, fd, time, size) the kernel bounds"
)]
#![cfg_attr(
    test,
    expect(
        clippy::cast_precision_loss,
        reason = "the roster push benchmark reports its ratios as floats"
    )
)]
#![expect(
    clippy::too_many_lines,
    reason = "the function-length threshold is a style gate, not correctness"
)]
#![expect(
    clippy::unnecessary_wraps,
    clippy::struct_excessive_bools,
    reason = "API-shape opinions, not defects: the surfaces are deliberate"
)]
/// Session code logs with `eprintln!`, as it did when each session was a process with its own
/// stderr. Sessions now share the daemon's, so in this crate the macro routes each line to
/// the calling session's own log, tagged with the session (`worker_stderr::line`); a line
/// logged outside any session still goes to the daemon's stderr. Declared before the modules
/// so it is in scope in all of them.
macro_rules! eprintln {
    ($($arg:tt)*) => {
        $crate::worker_stderr::line(format_args!($($arg)*))
    };
}

pub mod agent_engine;
pub mod agent_message_broadcast;
pub mod agent_message_ingest;
pub mod agent_messaging;
pub(crate) mod agent_roster;
pub(crate) mod agent_traffic;
mod async_safe_runtime;
mod auto_compaction;
mod autonomous_continuation;
pub(crate) mod backpressure;
pub(crate) mod bash_notices;
pub(crate) mod boot_reap;
pub mod branch_navigation;
pub(crate) mod child_status_notices;
mod compact_autorefine;
pub mod compaction;
mod compaction_outcome;
pub(crate) mod compaction_supervision;
pub(crate) mod context_breakdown;
pub(crate) mod context_tree_cache;
pub(crate) mod context_tree_children;
pub(crate) mod create_reuse;
pub mod descriptor;
pub mod engine;
pub(crate) mod event_bus;
pub(crate) mod factory_activity;
pub mod framing;
mod goal_continuation;
pub(crate) mod goal_state_persist;
pub(crate) mod herdr;
pub(crate) mod history_import;
pub mod hold_refusal;
mod idle_eviction;
pub(crate) mod image_route;
pub mod input_pause_lease;
mod interrupted_calls;
pub mod journal;
pub mod keeper;
pub mod mcp_connections;
pub mod mcp_login;
pub(crate) mod messaging;
pub mod model_allowlist;
pub(crate) mod model_catalog;
pub(crate) mod model_switch;
mod overflow_compaction;
pub mod ownership;
pub mod paths;
pub(crate) mod peer;
pub(crate) mod peer_client;
pub(crate) mod peer_tickets;
pub(crate) mod persistent_shells;
pub mod platform;
pub mod prompt_admission;
pub mod protocol;
mod provider_admission;
pub(crate) mod queue_commands;
pub mod registration;
pub(crate) mod registry;
mod revival_gate;
pub(crate) mod revived_turn;
pub mod rlm_child_model;
pub mod rlm_child_usage;
pub mod rlm_children;
pub(crate) mod rlm_roster;
pub mod rlm_surface;
pub(crate) mod roster_activity;
pub(crate) mod saved_session_commands;
pub(crate) mod scheduled_jobs;
pub(crate) mod scheduling_catalog;
pub(crate) mod session_bindings;
pub(crate) mod session_catalog;
pub(crate) mod session_commands;
pub(crate) mod session_custom;
pub mod session_export;
pub mod session_host;
pub mod session_input_pause;
pub mod session_navigation;
pub(crate) mod session_scan;
pub mod session_stats;
pub mod session_store;
pub mod session_tree;
pub mod session_usage;
pub(crate) mod setting_switches;
pub mod side_question;
mod signal_drain;
pub mod snapshot_stream;
pub mod socket;
pub(crate) mod state_getters;
mod stop_cleanup;
pub(crate) mod streaming;
pub mod supervisor;
pub mod supervisor_link;
pub(crate) mod supervisor_lost;
pub(crate) mod supervisor_parent_death;
pub(crate) mod supervisor_roster;
pub(crate) mod supervisor_roster_seed;
pub(crate) mod ts_era;
pub mod types;
pub(crate) mod update_prepare;
pub(crate) mod update_restore;
pub(crate) mod update_roster;
pub(crate) mod update_stop;
pub(crate) mod usage_report;
pub(crate) mod user_bash;
pub(crate) mod user_interaction;
pub mod util;
pub mod worker;
pub(crate) mod worker_stderr;
pub(crate) mod workspace_checkpoints;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) mod trace_test_env;
