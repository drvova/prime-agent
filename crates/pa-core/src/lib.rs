// large_futures: stack futures on hot paths by design. too_many_lines:
// style gate only. Casts: 64-bit targets; narrowing sits at bounded
// OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Test-only: exact-float `assert_eq!`s assert parsed fixture values; an
// epsilon compare would weaken the assertions, not fix a lint.
#![cfg_attr(test, allow(clippy::float_cmp))]

//! Session engine: tools, skills, prompts, compaction, refinement, kernel/RLM
//! manager, subagents, session manager, settings.
//!
//! Public API: the tool-definition contract; subsystem internals are `pub(crate)`.
//! `SessionEngine` (message in -> events out) is the future facade.

/// The lines this crate logs with `eprintln!` belong to the session whose code logged them.
/// Sessions hosted in one daemon share its standard error, so in this crate the macro hands
/// each line to the calling session's log ([`pa_types::session_scope::log_line`]); outside a
/// hosted session (the CLI's own modes, a test) it is the process's standard error as
/// before. Declared before the modules so it is in scope in all of them.
macro_rules! eprintln {
    ($($arg:tt)*) => {
        pa_types::session_scope::log_line(format_args!($($arg)*))
    };
}

pub(crate) mod tools;

// Tool-definition contract.
pub use tools::tool_definition::{
    AbortSignal, ExecuteFn, ExecuteFuture, ExecutionMode, OnUpdate, PrepareArgumentsFn,
    ToolContentBlock, ToolDefinition, ToolExecutionResult, ToolUpdate,
};

// Path-resolution helper for the CLI's `@file` expansion (cwd-relative
// resolve with the macOS filename variants).
pub use tools::path_utils::resolve_read_path;

// Result-rendering helpers: the image metadata pair the daemon's snapshot
// elision consumes alongside the tool renderers (narrow re-export).
pub use tools::render_utils::{get_image_dimensions_prefix, IMAGE_DIMENSIONS_PREFIX_BYTES};

// Kernel cells: a cell's result types and the kernel lifecycle seam.
pub use tools::kernel_cell::{
    sent_agent_message_json, ExecuteResult, ExecuteStatus, KernelAttachment, KernelErrorInfo,
    KernelExecutor, KernelProvisioner, KernelToolOptions, LateSentAgentMessageHandler,
};
pub mod agent_state;
pub mod agent_traces;
pub mod auth;
pub mod autonomous;
pub mod cron;
pub mod export_html;
pub(crate) mod fs_edit;
pub(crate) mod fs_search;
pub mod goals;
pub mod interaction;
pub mod kernel;
pub mod mcp;
pub mod models;
pub mod packages;
pub mod platform;
pub(crate) mod prefetch;
pub(crate) mod project_card;
pub mod prompts;
pub mod refinement;
pub mod resources;
pub mod reuse;
pub mod session;
pub mod session_engine;
pub mod settings;
pub mod skills;
pub mod slash_command_args;
pub(crate) mod symbol_index;
pub mod update;
pub mod usage_host;
pub(crate) mod web_crawl;
pub(crate) mod web_read;
pub mod web_search;
pub mod worktree_host;
