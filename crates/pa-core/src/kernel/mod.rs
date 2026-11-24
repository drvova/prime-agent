//! RLM kernel layer: the session's in-process Scheme kernel ([`scheme`]) and
//! the host-side services its cells use (host requests, output spill, cell
//! windows, error context, session-boot admission).

pub mod boot_admission;
pub(crate) mod call_refusals;
pub mod cancellation;
#[cfg(test)]
mod capabilities;
pub mod cell_window;
pub(crate) mod command_cache;
pub(crate) mod edit_checks;
pub(crate) mod edit_matches;
pub(crate) mod elevation_runs;
pub mod error_context;
pub(crate) mod hypotheses;
pub mod orphan_journal;
pub mod rlm_runtime;
pub mod scheme;
pub mod shared;
pub(crate) mod shell_runs;
pub mod spill;
pub(crate) mod test_runs;

pub use cancellation::AbortSignal;
pub use rlm_runtime::{
    create_default_rlm_subagent_session_name, find_rlm_model_matches,
    find_unique_rlm_short_form_model_match, format_rlm_model_unavailable_error,
    normalize_requested_rlm_subagent_model, normalize_requested_rlm_subagent_session_name,
    normalize_requested_rlm_subagent_thinking_level, RlmModelInfo, RlmModelMatch, THINKING_LEVELS,
};
pub use scheme::{
    keep_spare_scheme_kernels, pin_scheme_home, prewarm_scheme_kernel, run_elevation_helper,
    spare_scheme_kernels, SchemeKernel, SchemeKernelOptions, SchemeKernelProvisioner,
    StuckCellSink,
};
pub use shared::{
    HostRequestHandler, HostRequestHandlers, HostRequestPayload, KernelDiffDisplay,
    KernelSentAgentMessage,
};
