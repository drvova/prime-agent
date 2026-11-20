//! The versioned event catalog: every product event's name and typed property
//! rules, the successor of the deleted `docs/telemetry-events.md`.
//!
//! The catalog is the low-frequency set: per-run facts ride the one
//! `agent run completed` per run (TS parity), per-session facts ride
//! `agent session ended`, the interactive client's adoption counters ride
//! its one `tui exit`; the remaining events are lifecycle moments
//! (startup, onboarding, installation, update, daemon incidents). Schema
//! version 2 is the #2117 vocabulary (the v2 enrichment on the legacy
//! events and the onboarding/startup/installation stages).
//!
//! [`sanitize`] is the platform adjust layer: before a batch reaches any
//! sink, every catalogued event's properties are normalized against its
//! rule - unknown keys are dropped, out-of-vocabulary enums fall back to
//! the documented fallback, numbers are clamped to their caps, strings
//! are capped. Together with the primitive-only [`Properties`] boundary
//! this pins the privacy contract: no prompt, tool, or provider text can
//! ride a property, and no firing site can invent a property name.

use serde_json::Value;

use crate::properties::Properties;

/// The current schema version stamped on every event; additive property
/// changes do not bump it.
pub const SCHEMA_VERSION: u64 = 2;

// ---------------------------------------------------------------------------
// Rule kinds

/// One property's validation rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropKind {
    /// Fixed vocabulary; an out-of-vocabulary value falls back to the
    /// fallback, a null stays null when `nullable`.
    Enum {
        values: &'static [&'static str],
        fallback: &'static str,
        nullable: bool,
    },
    /// Numeric primitive, clamped to `max` (integers stay integers).
    Number {
        max: u64,
        integer: bool,
        nullable: bool,
    },
    /// Float cost in USD, clamped to `max`.
    Cost { max: f64, nullable: bool },
    /// Boolean primitive (null only when `nullable`).
    Boolean { nullable: bool },
    /// A random uuid string (shape-checked).
    Uuid,
    /// A version string (capped at 64 bytes).
    Version,
    /// A free string capped at `max` bytes.
    BoundedString { max: usize },
    /// The documented nested primitive-map exception (`phase_timings`).
    PrimitiveMap,
}

impl PropKind {
    /// Normalize one value against this kind. `None` drops the property.
    fn normalize(&self, value: Value) -> Option<Value> {
        match self {
            PropKind::Enum {
                values,
                fallback,
                nullable,
            } => match value {
                Value::String(text) => (values.contains(&text.as_str()))
                    .then_some(Value::String(text))
                    .or_else(|| Some(Value::String((*fallback).to_string()))),
                Value::Null if *nullable => Some(Value::Null),
                _ => Some(Value::String((*fallback).to_string())),
            },
            PropKind::Number {
                max,
                integer,
                nullable,
            } => match value {
                Value::Null if *nullable => Some(Value::Null),
                Value::Number(number) => {
                    if let Some(n) = number.as_u64() {
                        Some(Value::from(n.min(*max)))
                    } else if let Some(n) = number.as_i64() {
                        if *integer || n < 0 {
                            // Negative or fractional where an integer is
                            // required: not a valid sample.
                            None
                        } else {
                            Some(Value::from((n as u64).min(*max)))
                        }
                    } else if !*integer {
                        number.as_f64().map(|n| {
                            Value::from(if n.is_finite() && n >= 0.0 {
                                n.min(*max as f64)
                            } else {
                                0.0
                            })
                        })
                    } else {
                        None
                    }
                }
                _ if *nullable => None,
                _ => None,
            },
            PropKind::Cost { max, nullable } => match value {
                Value::Null if *nullable => Some(Value::Null),
                Value::Number(number) => number.as_f64().map(|n| {
                    Value::from(if n.is_finite() && n >= 0.0 {
                        n.min(*max)
                    } else {
                        0.0
                    })
                }),
                _ => None,
            },
            PropKind::Boolean { nullable } => match value {
                Value::Bool(_) => Some(value),
                Value::Null if *nullable => Some(value),
                _ => None,
            },
            PropKind::Uuid => match value {
                Value::String(text) => is_uuid(&text).then_some(Value::String(text)),
                _ => None,
            },
            PropKind::Version => match value {
                Value::String(text) => Some(Value::String(cap_string(&text, 64))),
                _ => None,
            },
            PropKind::BoundedString { max } => match value {
                Value::String(text) => Some(Value::String(cap_string(&text, *max))),
                _ => None,
            },
            PropKind::PrimitiveMap => match value {
                Value::Object(map) => (map.values().all(|value| {
                    matches!(
                        value,
                        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                    )
                }))
                .then_some(Value::Object(map)),
                _ => None,
            },
        }
    }
}

/// One event property's rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertyRule {
    /// The value rule.
    pub kind: PropKind,
    /// True when the event is malformed without this property; sanitize
    /// never invents required values.
    pub required: bool,
}

/// One catalogued event: its stable name and every property it may carry.
#[derive(Debug)]
pub struct EventRule {
    /// Stable event name, e.g. `agent started`.
    pub name: &'static str,
    /// Every property the event may carry (base properties are separate).
    pub properties: &'static [(&'static str, PropertyRule)],
    /// The schema version the event entered the catalog at.
    pub since: u64,
}

// Shared vocabularies

pub const ERROR_CATEGORIES: &[&str] = &[
    "authentication",
    "rate_limit",
    "timeout",
    "context_limit",
    "network",
    "provider_unavailable",
    "other",
];

pub const ERROR_SUBTYPES: &[&str] = &[
    "credential_missing",
    "credential_invalid",
    "credential_expired",
    "authentication_rejected",
    "permission_denied",
    "model_access_denied",
    "insufficient_balance",
    "quota_exceeded",
    "rate_limited",
    "network_error",
    "timeout",
    "provider_unavailable",
    "refusal",
    "malformed_response",
    "stream_drop",
    "context_limit",
    "configuration_error",
    "filesystem_error",
    "session_unavailable",
    "cancelled",
    "unknown",
];

/// The #2117 tool categories.
pub const TOOL_CATEGORIES: &[&str] = &[
    "read", "write", "edit", "bash", "grep", "find", "ls", "ipython", "mcp", "custom", "unknown",
];

/// How a provider auth descriptor resolved the session's request
/// credentials (`agent session ended` `auth_strategy`).
pub const AUTH_STRATEGIES: &[&str] = &["api_key", "token_exchange", "oauth", "unknown"];

pub const TERMINAL_OUTCOMES: &[&str] = &[
    "success",
    "error",
    "cancelled",
    "shutdown_interrupted",
    "unknown",
];

/// The #2117 run triggers.
pub const RUN_TRIGGERS: &[&str] = &["prompt", "continuation", "unknown"];

pub const RUN_OUTCOMES: &[&str] = &["success", "error", "aborted"];

pub const STOP_REASONS: &[&str] = &["stop", "length", "toolUse", "error", "aborted", "unknown"];

pub const PROVIDER_CATEGORIES: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "prime",
    "openrouter",
    "bedrock",
    "vertex",
    "mistral",
    "groq",
    "xai",
    "custom",
    "unknown",
];

pub const MODEL_CATEGORIES: &[&str] = &[
    "claude", "gpt", "o1", "o3", "o4", "gemini", "glm", "kimi", "qwen", "deepseek", "llama",
    "mistral", "custom", "unknown",
];

pub const AUTH_CATEGORIES: &[&str] = &[
    "oauth",
    "api_key",
    "mcp_static_token",
    "runtime_api_key",
    "environment",
    "prime_cli",
    "models_json",
    "fallback",
    "stale",
    "stored",
    "none",
    "unknown",
];

pub const FEATURE_NAMES: &[&str] = &[
    "model", "login", "logout", "effort", "goal", "new", "resume", "fork", "clone", "tree",
    "feedback", "rewind", "summary",
];

pub const FEATURE_OUTCOMES: &[&str] = &[
    "initiated",
    "completed",
    "failed",
    "canceled",
    "unavailable",
];

/// The #2117 onboarding stages.
pub const ONBOARDING_STAGES: &[&str] = &[
    "entry",
    "provider_selection",
    "credential_discovery",
    "credential_validation",
    "model_access",
    "ready",
    "exit",
];

pub const ONBOARDING_OUTCOMES: &[&str] = &[
    "initiated",
    "completed",
    "failed",
    "canceled",
    "skipped",
    "configured",
    "unavailable",
    "provider_switched",
];

pub const ONBOARDING_ENTRY_REASONS: &[&str] = &[
    "first_setup",
    "existing_configuration",
    "previously_shown",
    "reentered",
];

pub const ACQUISITION_METHODS: &[&str] = &[
    "existing_configuration",
    "prime_browser",
    "prime_key_entry",
    "oauth",
    "api_key_entry",
    "external_credentials",
    "unknown",
];

pub const VALIDATION_SCOPES: &[&str] = &[
    "configuration",
    "identity_scope",
    "selected_context",
    "inference",
    "unchecked",
];

pub const TIMING_SCOPES: &[&str] = &["system_work", "elapsed_including_user_wait"];

pub const INSTALLATION_STAGES: &[&str] = &[
    "started",
    "requirements",
    "release_lookup",
    "download",
    "verification",
    "package_install",
    "daemon_restart",
    "session_restore",
    "relaunch",
    "ready",
    "completed",
];

pub const INSTALLATION_OUTCOMES: &[&str] = &[
    "started",
    "success",
    "failed",
    "cancelled",
    "skipped",
    "unavailable",
];

pub const INSTALLATION_REASONS: &[&str] = &[
    "up_to_date",
    "unsupported_install",
    "declined",
    "requirements_unavailable",
    "release_lookup_failed",
    "download_failed",
    "verification_failed",
    "install_failed",
    "daemon_restart_failed",
    "session_restore_failed",
    "relaunch_failed",
    "version_mismatch",
    "interrupted",
    "unknown",
];

pub const INSTALLATION_ACTIONS: &[&str] = &["install", "update"];

pub const INSTALLATION_SOURCES: &[&str] = &["shell_installer", "cli", "interactive"];

pub const READY_KINDS: &[&str] = &["interactive", "headless"];

pub const INPUT_STAGES: &[&str] = &[
    "received",
    "queued",
    "preparation",
    "dispatch",
    "admitted",
    "terminal",
    "submitted",
    "rejected",
    "first_visible_status",
    "cancellation_to_idle",
];

/// The #2117 startup stages.
pub const STARTUP_STAGES: &[&str] = &[
    "ui_ready",
    "session_attach",
    "configuration_load",
    "credential_validation",
    "session_ui_rebind",
];

pub const STARTUP_OUTCOMES: &[&str] = &["completed", "failed"];

pub const STARTUP_KINDS: &[&str] = &["cold", "warm_attach", "resumed", "unknown"];

pub const BUILD_CHANNELS: &[&str] = &["release", "prerelease", "development", "unknown"];

pub const WORKLOAD_ORIGINS: &[&str] = &["interactive", "automated", "internal", "test", "unknown"];

// Rule constructors

const fn enum_rule(values: &'static [&'static str], fallback: &'static str) -> PropKind {
    PropKind::Enum {
        values,
        fallback,
        nullable: false,
    }
}

const fn nullable_enum_rule(values: &'static [&'static str], fallback: &'static str) -> PropKind {
    PropKind::Enum {
        values,
        fallback,
        nullable: true,
    }
}

const fn required(kind: PropKind) -> PropertyRule {
    PropertyRule {
        kind,
        required: true,
    }
}

const fn optional(kind: PropKind) -> PropertyRule {
    PropertyRule {
        kind,
        required: false,
    }
}

const fn count() -> PropKind {
    PropKind::Number {
        max: 1_000_000,
        integer: true,
        nullable: false,
    }
}

/// The base properties merged under every event (the platform module stamps them): sanitize
/// validates them against these rules instead of dropping them as uncatalogued.
pub const BASE_PROPERTIES: &[(&str, PropertyRule)] = &[
    ("version", optional(PropKind::Version)),
    (
        "schema_version",
        required(PropKind::Number {
            max: 1_000_000,
            integer: true,
            nullable: false,
        }),
    ),
    (
        "schema_revision",
        optional(PropKind::Number {
            max: 10_000,
            integer: true,
            nullable: false,
        }),
    ),
    (
        "build_channel",
        optional(enum_rule(BUILD_CHANNELS, "unknown")),
    ),
    (
        "workload_origin",
        optional(enum_rule(WORKLOAD_ORIGINS, "unknown")),
    ),
    ("os_family", optional(free_string(32))),
    ("architecture", optional(free_string(32))),
    ("install_method", optional(free_string(32))),
    ("execution_mode", optional(free_string(32))),
    (
        "libc",
        optional(enum_rule(&["glibc", "musl", "none", "unknown"], "unknown")),
    ),
    ("libc_version", optional(free_string(32))),
    (
        "cpu_baseline",
        optional(enum_rule(
            &[
                "avx2",
                "no_avx2",
                "avx2_assumed",
                "not_applicable",
                "unknown",
            ],
            "unknown",
        )),
    ),
    ("os_release", optional(free_string(64))),
    ("os_product_version", optional(free_string(32))),
];

const fn tokens() -> PropKind {
    PropKind::Number {
        max: 1_000_000_000_000,
        integer: true,
        nullable: false,
    }
}

const fn duration() -> PropKind {
    PropKind::Number {
        max: 31_536_000_000,
        integer: true,
        nullable: true,
    }
}

const fn uuid() -> PropKind {
    PropKind::Uuid
}

const fn version() -> PropKind {
    PropKind::Version
}

const fn boolean() -> PropKind {
    PropKind::Boolean { nullable: false }
}

const fn cost() -> PropKind {
    PropKind::Cost {
        max: 1_000_000.0,
        nullable: true,
    }
}

const fn free_string(max: usize) -> PropKind {
    PropKind::BoundedString { max }
}

// The catalog: every event the product emits has exactly one row here; the
// seams are the complete emission set (privacy contract).

/// Session creation, depth-0 only.
const AGENT_STARTED: EventRule = EventRule {
    name: "agent started",
    since: 1,
    properties: &[
        ("session_id", required(uuid())),
        ("skill_count", optional(count())),
        ("python_skill_count", optional(count())),
    ],
};

/// `agent run completed` (v1, enriched in v2): run finalize.
const AGENT_RUN_COMPLETED: EventRule = EventRule {
    name: "agent run completed",
    since: 1,
    properties: &[
        ("session_id", required(uuid())),
        ("outcome", required(enum_rule(RUN_OUTCOMES, "error"))),
        ("duration_ms", required(duration())),
        ("visible_ttft_ms", optional(duration())),
        ("first_model_event_ms", optional(duration())),
        ("model_latency_ms", optional(duration())),
        ("max_model_latency_ms", optional(duration())),
        ("model_call_count", optional(count())),
        ("turn_count", optional(count())),
        ("tool_call_count", optional(count())),
        ("tool_error_count", optional(count())),
        ("input_tokens", optional(tokens())),
        ("output_tokens", optional(tokens())),
        ("cache_read_tokens", optional(tokens())),
        ("cache_write_tokens", optional(tokens())),
        ("total_tokens", optional(tokens())),
        ("compaction_count", optional(count())),
        ("retry_count", optional(count())),
        // The stream loop guard: model calls cut for repeating themselves,
        // retries after such a cut, and repeated-tool-call nudges.
        ("loop_guard_trip_count", optional(count())),
        ("loop_guard_retry_count", optional(count())),
        ("loop_guard_tool_repeat_nudge_count", optional(count())),
        (
            "provider_category",
            optional(enum_rule(PROVIDER_CATEGORIES, "custom")),
        ),
        (
            "model_category",
            optional(enum_rule(MODEL_CATEGORIES, "custom")),
        ),
        (
            "error_category",
            optional(nullable_enum_rule(ERROR_CATEGORIES, "other")),
        ),
        ("run_id", optional(uuid())),
        ("run_index", optional(count())),
        ("trigger", optional(enum_rule(RUN_TRIGGERS, "unknown"))),
        ("stop_reason", optional(enum_rule(STOP_REASONS, "unknown"))),
        (
            "terminal_outcome",
            optional(enum_rule(TERMINAL_OUTCOMES, "unknown")),
        ),
        ("successful_model_call_count", optional(count())),
        ("usage_complete", optional(boolean())),
        ("estimated_cost_usd", optional(cost())),
        (
            "error_subtype",
            optional(enum_rule(ERROR_SUBTYPES, "unknown")),
        ),
        ("first_reasoning_ms", optional(duration())),
        ("run_to_first_text_ms", optional(duration())),
        ("tool_duration_ms", optional(duration())),
        ("retry_wait_ms", optional(duration())),
        ("compaction_duration_ms", optional(duration())),
        ("max_stream_gap_ms", optional(duration())),
        // The folded per-run aggregates (one event per run):
        ("model_latency_p50_ms", optional(duration())),
        ("model_error_count", optional(count())),
        ("error_authentication_count", optional(count())),
        ("error_rate_limit_count", optional(count())),
        ("error_timeout_count", optional(count())),
        ("error_context_limit_count", optional(count())),
        ("error_network_count", optional(count())),
        ("error_provider_unavailable_count", optional(count())),
        ("error_other_count", optional(count())),
        ("tool_read_call_count", optional(count())),
        ("tool_read_error_count", optional(count())),
        ("tool_read_duration_ms", optional(duration())),
        ("tool_read_max_duration_ms", optional(duration())),
        ("tool_write_call_count", optional(count())),
        ("tool_write_error_count", optional(count())),
        ("tool_write_duration_ms", optional(duration())),
        ("tool_write_max_duration_ms", optional(duration())),
        ("tool_edit_call_count", optional(count())),
        ("tool_edit_error_count", optional(count())),
        ("tool_edit_duration_ms", optional(duration())),
        ("tool_edit_max_duration_ms", optional(duration())),
        ("tool_bash_call_count", optional(count())),
        ("tool_bash_error_count", optional(count())),
        ("tool_bash_duration_ms", optional(duration())),
        ("tool_bash_max_duration_ms", optional(duration())),
        ("tool_grep_call_count", optional(count())),
        ("tool_grep_error_count", optional(count())),
        ("tool_grep_duration_ms", optional(duration())),
        ("tool_grep_max_duration_ms", optional(duration())),
        ("tool_find_call_count", optional(count())),
        ("tool_find_error_count", optional(count())),
        ("tool_find_duration_ms", optional(duration())),
        ("tool_find_max_duration_ms", optional(duration())),
        ("tool_ls_call_count", optional(count())),
        ("tool_ls_error_count", optional(count())),
        ("tool_ls_duration_ms", optional(duration())),
        ("tool_ls_max_duration_ms", optional(duration())),
        ("tool_ipython_call_count", optional(count())),
        ("tool_ipython_error_count", optional(count())),
        ("tool_ipython_duration_ms", optional(duration())),
        ("tool_ipython_max_duration_ms", optional(duration())),
        ("mcp_tool_call_count", optional(count())),
        ("mcp_tool_error_count", optional(count())),
        ("mcp_tool_duration_ms", optional(duration())),
        ("mcp_tool_max_duration_ms", optional(duration())),
        ("custom_tool_call_count", optional(count())),
        ("custom_tool_error_count", optional(count())),
        ("custom_tool_duration_ms", optional(duration())),
        ("custom_tool_max_duration_ms", optional(duration())),
        // Bounded `bash()` awaits that returned the finished result inside
        // their wait window vs yielded a still-running snapshot (set only
        // when the run had any).
        ("bash_wait_completed_count", optional(count())),
        ("bash_wait_yielded_count", optional(count())),
    ],
};

/// Session dispose.
const AGENT_SESSION_ENDED: EventRule = EventRule {
    name: "agent session ended",
    since: 1,
    properties: &[
        ("session_id", required(uuid())),
        ("duration_ms", required(duration())),
        ("prompt_count", optional(count())),
        ("run_count", optional(count())),
        ("successful_run_count", optional(count())),
        ("failed_run_count", optional(count())),
        ("aborted_run_count", optional(count())),
        ("tool_call_count", optional(count())),
        ("compaction_count", optional(count())),
        ("model_call_count", optional(count())),
        ("input_tokens", optional(tokens())),
        ("output_tokens", optional(tokens())),
        ("cache_read_tokens", optional(tokens())),
        ("cache_write_tokens", optional(tokens())),
        ("total_tokens", optional(tokens())),
        (
            "terminal_outcome",
            optional(enum_rule(TERMINAL_OUTCOMES, "unknown")),
        ),
        // The folded per-session counters:
        ("retry_count", optional(count())),
        ("model_error_count", optional(count())),
        ("skill_use_count", optional(count())),
        ("mcp_connector_use_count", optional(count())),
        ("kernel_bootstrap_count", optional(count())),
        ("kernel_bootstrap_cold_count", optional(count())),
        ("kernel_bootstrap_failed_count", optional(count())),
        ("kernel_bootstrap_max_ms", optional(duration())),
        ("rlm_child_usage_count", optional(count())),
        ("rlm_child_input_tokens", optional(tokens())),
        ("rlm_child_output_tokens", optional(tokens())),
        ("rlm_child_cache_read_tokens", optional(tokens())),
        ("rlm_child_cache_write_tokens", optional(tokens())),
        ("rlm_child_cost", optional(cost())),
        // Native web search (the query and provider never upload).
        ("web_search_count", optional(count())),
        ("web_search_failed_count", optional(count())),
        ("web_search_fallback_count", optional(count())),
        ("web_search_keyless_count", optional(count())),
        ("web_search_result_count", optional(count())),
        ("web_search_max_ms", optional(duration())),
        // Request-view thinning of aged tool results (counts only).
        ("context_thinned_request_count", optional(count())),
        ("context_thinned_result_count", optional(count())),
        ("context_thinned_chars_saved", optional(tokens())),
        ("context_thinned_tokens_saved", optional(tokens())),
        // Prompt-cache warming and savings: request counts and estimated
        // dollars only (never a prompt, model, or provider).
        ("cache_warm_sent_count", optional(count())),
        ("cache_warm_skipped_count", optional(count())),
        ("cache_warm_cost_usd", optional(cost())),
        ("cache_savings_usd", optional(cost())),
        // Provider auth descriptors (strategy and counters only: never a
        // provider, URL, account, or token).
        (
            "auth_strategy",
            optional(enum_rule(AUTH_STRATEGIES, "unknown")),
        ),
        ("auth_token_exchange_count", optional(count())),
        ("auth_refresh_failed_count", optional(count())),
        ("auth_rejected_retry_count", optional(count())),
        ("feature_model_initiated_count", optional(count())),
        ("feature_model_completed_count", optional(count())),
        ("feature_model_failed_count", optional(count())),
        ("feature_model_canceled_count", optional(count())),
        ("feature_model_unavailable_count", optional(count())),
        ("feature_login_initiated_count", optional(count())),
        ("feature_login_completed_count", optional(count())),
        ("feature_login_failed_count", optional(count())),
        ("feature_login_canceled_count", optional(count())),
        ("feature_login_unavailable_count", optional(count())),
        ("feature_logout_initiated_count", optional(count())),
        ("feature_logout_completed_count", optional(count())),
        ("feature_logout_failed_count", optional(count())),
        ("feature_logout_canceled_count", optional(count())),
        ("feature_logout_unavailable_count", optional(count())),
        ("feature_effort_initiated_count", optional(count())),
        ("feature_effort_completed_count", optional(count())),
        ("feature_effort_failed_count", optional(count())),
        ("feature_effort_canceled_count", optional(count())),
        ("feature_effort_unavailable_count", optional(count())),
        ("feature_goal_initiated_count", optional(count())),
        ("feature_goal_completed_count", optional(count())),
        ("feature_goal_failed_count", optional(count())),
        ("feature_goal_canceled_count", optional(count())),
        ("feature_goal_unavailable_count", optional(count())),
        ("feature_new_initiated_count", optional(count())),
        ("feature_new_completed_count", optional(count())),
        ("feature_new_failed_count", optional(count())),
        ("feature_new_canceled_count", optional(count())),
        ("feature_new_unavailable_count", optional(count())),
        ("feature_resume_initiated_count", optional(count())),
        ("feature_resume_completed_count", optional(count())),
        ("feature_resume_failed_count", optional(count())),
        ("feature_resume_canceled_count", optional(count())),
        ("feature_resume_unavailable_count", optional(count())),
        ("feature_fork_initiated_count", optional(count())),
        ("feature_fork_completed_count", optional(count())),
        ("feature_fork_failed_count", optional(count())),
        ("feature_fork_canceled_count", optional(count())),
        ("feature_fork_unavailable_count", optional(count())),
        ("feature_clone_initiated_count", optional(count())),
        ("feature_clone_completed_count", optional(count())),
        ("feature_clone_failed_count", optional(count())),
        ("feature_clone_canceled_count", optional(count())),
        ("feature_clone_unavailable_count", optional(count())),
        ("feature_tree_initiated_count", optional(count())),
        ("feature_tree_completed_count", optional(count())),
        ("feature_tree_failed_count", optional(count())),
        ("feature_tree_canceled_count", optional(count())),
        ("feature_tree_unavailable_count", optional(count())),
        ("feature_feedback_initiated_count", optional(count())),
        ("feature_feedback_completed_count", optional(count())),
        ("feature_feedback_failed_count", optional(count())),
        ("feature_feedback_canceled_count", optional(count())),
        ("feature_feedback_unavailable_count", optional(count())),
        ("feature_rewind_initiated_count", optional(count())),
        ("feature_rewind_completed_count", optional(count())),
        ("feature_rewind_failed_count", optional(count())),
        ("feature_rewind_canceled_count", optional(count())),
        ("feature_rewind_unavailable_count", optional(count())),
        ("feature_summary_initiated_count", optional(count())),
        ("feature_summary_completed_count", optional(count())),
        ("feature_summary_failed_count", optional(count())),
        ("feature_summary_canceled_count", optional(count())),
        ("feature_summary_unavailable_count", optional(count())),
    ],
};

/// `agent command used` (v1): builtin command names only, never arguments.
/// The TUI client sends it with the base properties and the command name
/// alone (TS `captureAgentCommandUsed`), so it carries no session id.
const AGENT_COMMAND_USED: EventRule = EventRule {
    name: "agent command used",
    since: 1,
    properties: &[("command_name", required(free_string(64)))],
};

/// Adoption of the automatic compaction percentage setting; no session content.
const AUTOCOMPACT_THRESHOLD_CHANGED: EventRule = EventRule {
    name: "autocompact threshold changed",
    since: 2,
    properties: &[(
        "threshold_percent",
        required(PropKind::Number {
            max: 100,
            integer: false,
            nullable: false,
        }),
    )],
};

/// The onboarding journey's real stages only.
const ONBOARDING_STAGE: EventRule = EventRule {
    name: "onboarding stage",
    since: 2,
    properties: &[
        ("onboarding_id", required(uuid())),
        ("stage", required(enum_rule(ONBOARDING_STAGES, "unknown"))),
        (
            "outcome",
            required(enum_rule(ONBOARDING_OUTCOMES, "unknown")),
        ),
        ("duration_ms", optional(duration())),
        (
            "auth_category",
            optional(enum_rule(AUTH_CATEGORIES, "none")),
        ),
        (
            "acquisition_method",
            optional(enum_rule(ACQUISITION_METHODS, "unknown")),
        ),
        (
            "validation_scope",
            optional(enum_rule(VALIDATION_SCOPES, "unchecked")),
        ),
        (
            "entry_reason",
            optional(enum_rule(ONBOARDING_ENTRY_REASONS, "unknown")),
        ),
        (
            "timing_scope",
            optional(enum_rule(TIMING_SCOPES, "system_work")),
        ),
    ],
};

/// The onboarding flow's terminal outcome.
const ONBOARDING_COMPLETED: EventRule = EventRule {
    name: "onboarding completed",
    since: 1,
    properties: &[
        ("duration_ms", required(duration())),
        ("outcome", required(enum_rule(RUN_OUTCOMES, "error"))),
        (
            "auth_category",
            required(enum_rule(AUTH_CATEGORIES, "none")),
        ),
        (
            "provider_category",
            optional(enum_rule(PROVIDER_CATEGORIES, "unknown")),
        ),
        ("onboarding_id", optional(uuid())),
    ],
};

/// `agent startup stage` (v2): startup-phase timing per stage.
const AGENT_STARTUP_STAGE: EventRule = EventRule {
    name: "agent startup stage",
    since: 2,
    properties: &[
        ("stage", required(enum_rule(STARTUP_STAGES, "unknown"))),
        ("outcome", required(enum_rule(STARTUP_OUTCOMES, "failed"))),
        ("duration_ms", required(duration())),
        (
            "startup_kind",
            optional(enum_rule(STARTUP_KINDS, "unknown")),
        ),
        (
            "timing_scope",
            optional(enum_rule(TIMING_SCOPES, "system_work")),
        ),
    ],
};

/// `agent installation stage` (v2): installer/updater stage outcomes.
const AGENT_INSTALLATION_STAGE: EventRule = EventRule {
    name: "agent installation stage",
    since: 2,
    properties: &[
        ("installation_attempt_id", required(uuid())),
        (
            "installation_action",
            required(enum_rule(INSTALLATION_ACTIONS, "update")),
        ),
        (
            "installation_source",
            required(enum_rule(INSTALLATION_SOURCES, "cli")),
        ),
        ("stage", required(enum_rule(INSTALLATION_STAGES, "unknown"))),
        (
            "outcome",
            required(enum_rule(INSTALLATION_OUTCOMES, "unknown")),
        ),
        (
            "reason",
            optional(enum_rule(INSTALLATION_REASONS, "unknown")),
        ),
        ("from_version", optional(version())),
        ("target_version", optional(version())),
        ("observed_version", optional(version())),
        ("duration_ms", optional(duration())),
        (
            "exit_code",
            optional(PropKind::Number {
                max: 255,
                integer: true,
                nullable: true,
            }),
        ),
        ("error_id", optional(uuid())),
        (
            "ready_kind",
            optional(enum_rule(READY_KINDS, "interactive")),
        ),
        ("session_restore_total", optional(count())),
        ("session_restore_failed", optional(count())),
    ],
};

/// `startup` (v1): process entry to ready interactive session environment.
const STARTUP: EventRule = EventRule {
    name: "startup",
    since: 1,
    properties: &[
        ("duration_ms", required(duration())),
        ("phase_timings", required(PropKind::PrimitiveMap)),
        ("execution_mode", required(free_string(32))),
    ],
};

/// Supervision lifecycle, counts only.
const DAEMON_EVENT: EventRule = EventRule {
    name: "daemon event",
    since: 1,
    properties: &[
        ("kind", required(free_string(64))),
        (
            "exit_reason",
            optional(enum_rule(&["normal", "crash"], "crash")),
        ),
        ("count", optional(count())),
        ("source", optional(free_string(32))),
        ("boot", optional(enum_rule(&["plain", "update"], "plain"))),
        ("adopted_live", optional(count())),
        ("revived", optional(count())),
        ("skipped_idle", optional(count())),
        ("stopped", optional(count())),
        ("failed", optional(count())),
        // The `summary` kind: the frequent supervision events, counted
        // over a window of at most an hour.
        ("window_ms", optional(duration())),
        ("worker_exited_normal_count", optional(count())),
        ("worker_exited_crash_count", optional(count())),
        ("worker_restarted_count", optional(count())),
        ("worker_overloaded_count", optional(count())),
        ("attach_count", optional(count())),
        ("reattach_count", optional(count())),
        ("detach_count", optional(count())),
        ("registration_refused_count", optional(count())),
        ("session_rebound_count", optional(count())),
        ("root_identity_persist_failed_count", optional(count())),
        ("saved_sessions_list_count", optional(count())),
        ("saved_sessions_usage_rows_max", optional(count())),
    ],
};

/// The settings allowlist guardrail.
const MODEL_REFUSED: EventRule = EventRule {
    name: "model refused",
    since: 1,
    properties: &[
        (
            "surface",
            required(enum_rule(
                &[
                    "set_model",
                    "cycle_model",
                    "spawn",
                    "create_session",
                    "session_start",
                ],
                "session_start",
            )),
        ),
        (
            "provider_category",
            optional(enum_rule(PROVIDER_CATEGORIES, "custom")),
        ),
        (
            "model_category",
            optional(enum_rule(MODEL_CATEGORIES, "custom")),
        ),
    ],
};

/// `session archived` (v1): the daemon `kill` path.
const SESSION_ARCHIVED: EventRule = EventRule {
    name: "session archived",
    since: 1,
    properties: &[
        ("session_id", required(uuid())),
        ("duration_ms", required(duration())),
    ],
};

/// A settled ipython cell that rendered as bash (v2): its executed
/// `bash()` line share and command count, never command text. One of the
/// standalone TUI events (with `agent command used`),
/// tracked per render by the upstream #3307
/// addition and kept intact.
const TUI_IPYTHON_BASH_RENDERED: EventRule = EventRule {
    name: "tui ipython bash rendered",
    since: 2,
    properties: &[
        ("bash_lines", required(count())),
        ("cell_lines", required(count())),
        ("count", required(count())),
    ],
};

/// One resume-picker run ended (v2): the folder scope it ended on (for an
/// opened session, the scope it was resumed from), the folder toggle's
/// flips, and whether a session opened. Standalone because `tui exit`
/// sits at the backend's 128-key cap. Never a path.
const TUI_RESUME_PICKER_CLOSED: EventRule = EventRule {
    name: "tui resume picker closed",
    since: 2,
    properties: &[
        (
            "scope",
            required(enum_rule(
                &["current_folder", "all_folders"],
                "current_folder",
            )),
        ),
        ("scope_toggle_count", required(count())),
        (
            "outcome",
            required(enum_rule(&["opened", "exited"], "exited")),
        ),
    ],
};

/// One image paste attempt (v2): where the image came from (clipboard data, a clipboard file
/// copy, a pasted or dropped path), how it ended (`text`: Ctrl+V found no image and pasted the
/// clipboard's text), and the attached format. Standalone because
/// `tui exit` sits at the backend's 128-key cap (an attach also counts `tui_image_paste_count`
/// there). Never a path, a size, or image content.
const TUI_IMAGE_PASTE: EventRule = EventRule {
    name: "tui image paste",
    since: 2,
    properties: &[
        (
            "source",
            required(enum_rule(
                &["clipboard", "clipboard_file", "path"],
                "clipboard",
            )),
        ),
        (
            "outcome",
            required(enum_rule(
                &["attached", "text", "no_image", "too_large", "unreadable"],
                "no_image",
            )),
        ),
        (
            "format",
            required(enum_rule(&["png", "jpeg", "gif", "webp", "none"], "none")),
        ),
    ],
};

/// One `/login` settled a credential save (v2): the credential's kind, the slot it went to (the
/// provider's default slot or a new named credential beside it), the surface that ran the login,
/// and how it ended. Never a provider, a label, an account id, or any credential material.
const AGENT_CREDENTIAL_ADDED: EventRule = EventRule {
    name: "agent credential added",
    since: 2,
    properties: &[
        (
            "kind",
            required(enum_rule(&["api_key", "oauth"], "api_key")),
        ),
        (
            "slot",
            required(enum_rule(&["default", "named"], "default")),
        ),
        (
            "surface",
            required(enum_rule(&["login", "model_picker", "onboarding"], "login")),
        ),
        (
            "outcome",
            required(enum_rule(&["saved", "failed", "cancelled"], "failed")),
        ),
    ],
};

/// One interactive run started its session pinned to a stored named credential (v2): `source` is
/// how the pin was chosen (`flag`: `--credential`). Never a provider, a label, or a credential id.
const AGENT_CREDENTIAL_PINNED: EventRule = EventRule {
    name: "agent credential pinned",
    since: 2,
    properties: &[("source", required(enum_rule(&["flag"], "flag")))],
};

/// One interactive run showed inline transcript images (v2), emitted at the run's exit when
/// any image was shown: the distinct images drawn per protocol and the protocol the terminal
/// resolved to. Standalone because `tui exit` sits at the backend's 128-key cap. Counts only,
/// never image content, dimensions, or a path.
const TUI_INLINE_IMAGES_RENDERED: EventRule = EventRule {
    name: "tui inline images rendered",
    since: 2,
    properties: &[
        (
            "protocol",
            required(enum_rule(
                &["kitty", "iterm2", "sixel", "half_block"],
                "half_block",
            )),
        ),
        ("kitty_count", required(count())),
        ("iterm2_count", required(count())),
        ("sixel_count", required(count())),
        ("half_block_count", required(count())),
    ],
};

/// One interactive run used the todo tray or the kernel-question panel (v2), emitted at the
/// run's exit when any count is nonzero. Standalone because `tui exit` sits at the backend's
/// 128-key cap. Counts only: never item text, a question, a choice, or an answer.
const TUI_TODOS_AND_QUESTIONS: EventRule = EventRule {
    name: "tui todos and questions",
    since: 2,
    properties: &[
        ("todo_tray_toggle_count", required(count())),
        ("question_answer_count", required(count())),
        ("question_digit_answer_count", required(count())),
        ("question_other_answer_count", required(count())),
        ("question_decline_count", required(count())),
        ("question_click_count", required(count())),
    ],
};

/// One `/usage` page run ended (v2): how each half landed (the quota cards and the local spend
/// history), the card count, whether the details view opened, the refreshes, and the tray's
/// rate-limit reset chips shown since the last close. Standalone
/// because `tui exit` sits at the backend's 128-key cap. Never an account, an amount, or a URL.
const TUI_USAGE_PAGE_CLOSED: EventRule = EventRule {
    name: "tui usage page closed",
    since: 2,
    properties: &[
        ("provider_count", required(count())),
        (
            "quotas",
            required(enum_rule(
                &["loaded", "partial", "failed", "empty", "loading"],
                "loading",
            )),
        ),
        (
            "spend",
            required(enum_rule(
                &["loaded", "failed", "empty", "loading"],
                "loading",
            )),
        ),
        ("details_opened", required(boolean())),
        ("refresh_count", required(count())),
        ("reset_chip_shown_count", required(count())),
        // The shown 30 days' recorded calls by billing share (counts only, never a cost).
        ("billed_call_count", optional(count())),
        ("plan_call_count", optional(count())),
        ("free_call_count", optional(count())),
    ],
};

/// One context-window run ended (v2): where it opened from (the tray gauge's click, `/context`,
/// or its key), how its breakdown request landed, and the category and subagent rows it showed.
/// Standalone because `tui exit` sits at the backend's 128-key cap. Counts and enums only.
const TUI_CONTEXT_WINDOW_CLOSED: EventRule = EventRule {
    name: "tui context window closed",
    since: 2,
    properties: &[
        (
            "source",
            required(enum_rule(&["click", "command", "shortcut"], "command")),
        ),
        (
            "breakdown",
            required(enum_rule(
                &["loaded", "unavailable", "failed", "loading"],
                "loading",
            )),
        ),
        ("category_count", required(count())),
        ("subagent_count", required(count())),
    ],
};

/// `tui exit` (v1, enriched): one per interactive session run (each agents
/// view handoff ends one), carrying that run's adoption counters (the TUI
/// interactions, the client-side feature outcomes, the input-stage counts
/// and maxima) instead of one event per interaction.
const TUI_EXIT: EventRule = EventRule {
    name: "tui exit",
    since: 1,
    properties: &[
        (
            "exit_reason",
            required(enum_rule(
                &["ctrl_c_twice", "ctrl_d", "session_request", "daemon_closed"],
                "daemon_closed",
            )),
        ),
        ("turn_active", required(boolean())),
        ("tui_scroll_count", optional(count())),
        ("tui_live_tool_timer_count", optional(count())),
        ("tui_stall_shown_count", optional(count())),
        ("tui_mention_pill_count", optional(count())),
        ("tui_draft_recall_count", optional(count())),
        ("tui_selection_count", optional(count())),
        ("tui_click_count", optional(count())),
        ("tui_menu_open_count", optional(count())),
        ("tui_slash_accept_count", optional(count())),
        ("tui_menu_search_count", optional(count())),
        ("tui_activity_open_count", optional(count())),
        ("tui_subagents_open_count", optional(count())),
        ("tui_scoped_agent_count", optional(count())),
        ("tui_image_paste_count", optional(count())),
        ("tui_input_queued_count", optional(count())),
        ("tui_queue_edit_count", optional(count())),
        ("tui_queue_dispatch_count", optional(count())),
        ("tui_prompt_stash_count", optional(count())),
        ("tui_bash_shortcut_count", optional(count())),
        ("tui_bash_bang_count", optional(count())),
        ("tui_external_editor_count", optional(count())),
        ("tui_scoped_models_count", optional(count())),
        ("tui_suspend_count", optional(count())),
        ("tui_agents_action_count", optional(count())),
        ("tui_enhanced_keys_kitty", optional(boolean())),
        ("tui_enhanced_keys_modify_other_keys", optional(boolean())),
        ("tui_hyperlinks_enabled", optional(boolean())),
        ("feature_model_initiated_count", optional(count())),
        ("feature_model_completed_count", optional(count())),
        ("feature_model_failed_count", optional(count())),
        ("feature_model_canceled_count", optional(count())),
        ("feature_model_unavailable_count", optional(count())),
        ("feature_login_initiated_count", optional(count())),
        ("feature_login_completed_count", optional(count())),
        ("feature_login_failed_count", optional(count())),
        ("feature_login_canceled_count", optional(count())),
        ("feature_login_unavailable_count", optional(count())),
        ("feature_logout_initiated_count", optional(count())),
        ("feature_logout_completed_count", optional(count())),
        ("feature_logout_failed_count", optional(count())),
        ("feature_logout_canceled_count", optional(count())),
        ("feature_logout_unavailable_count", optional(count())),
        ("feature_effort_initiated_count", optional(count())),
        ("feature_effort_completed_count", optional(count())),
        ("feature_effort_failed_count", optional(count())),
        ("feature_effort_canceled_count", optional(count())),
        ("feature_effort_unavailable_count", optional(count())),
        ("feature_goal_initiated_count", optional(count())),
        ("feature_goal_completed_count", optional(count())),
        ("feature_goal_failed_count", optional(count())),
        ("feature_goal_canceled_count", optional(count())),
        ("feature_goal_unavailable_count", optional(count())),
        ("feature_new_initiated_count", optional(count())),
        ("feature_new_completed_count", optional(count())),
        ("feature_new_failed_count", optional(count())),
        ("feature_new_canceled_count", optional(count())),
        ("feature_new_unavailable_count", optional(count())),
        ("feature_resume_initiated_count", optional(count())),
        ("feature_resume_completed_count", optional(count())),
        ("feature_resume_failed_count", optional(count())),
        ("feature_resume_canceled_count", optional(count())),
        ("feature_resume_unavailable_count", optional(count())),
        ("feature_fork_initiated_count", optional(count())),
        ("feature_fork_completed_count", optional(count())),
        ("feature_fork_failed_count", optional(count())),
        ("feature_fork_canceled_count", optional(count())),
        ("feature_fork_unavailable_count", optional(count())),
        ("feature_clone_initiated_count", optional(count())),
        ("feature_clone_completed_count", optional(count())),
        ("feature_clone_failed_count", optional(count())),
        ("feature_clone_canceled_count", optional(count())),
        ("feature_clone_unavailable_count", optional(count())),
        ("feature_tree_initiated_count", optional(count())),
        ("feature_tree_completed_count", optional(count())),
        ("feature_tree_failed_count", optional(count())),
        ("feature_tree_canceled_count", optional(count())),
        ("feature_tree_unavailable_count", optional(count())),
        ("feature_feedback_initiated_count", optional(count())),
        ("feature_feedback_completed_count", optional(count())),
        ("feature_feedback_failed_count", optional(count())),
        ("feature_feedback_canceled_count", optional(count())),
        ("feature_feedback_unavailable_count", optional(count())),
        ("feature_rewind_initiated_count", optional(count())),
        ("feature_rewind_completed_count", optional(count())),
        ("feature_rewind_failed_count", optional(count())),
        ("feature_rewind_canceled_count", optional(count())),
        ("feature_rewind_unavailable_count", optional(count())),
        ("feature_summary_initiated_count", optional(count())),
        ("feature_summary_completed_count", optional(count())),
        ("feature_summary_failed_count", optional(count())),
        ("feature_summary_canceled_count", optional(count())),
        ("feature_summary_unavailable_count", optional(count())),
        ("input_received_count", optional(count())),
        ("input_queued_count", optional(count())),
        ("input_preparation_count", optional(count())),
        ("input_dispatch_count", optional(count())),
        ("input_admitted_count", optional(count())),
        ("input_terminal_count", optional(count())),
        ("input_submitted_count", optional(count())),
        ("input_rejected_count", optional(count())),
        ("input_first_visible_status_count", optional(count())),
        ("input_cancellation_to_idle_count", optional(count())),
        ("input_received_max_ms", optional(duration())),
        ("input_queued_max_ms", optional(duration())),
        ("input_preparation_max_ms", optional(duration())),
        ("input_dispatch_max_ms", optional(duration())),
        ("input_admitted_max_ms", optional(duration())),
        ("input_terminal_max_ms", optional(duration())),
        ("input_submitted_max_ms", optional(duration())),
        ("input_rejected_max_ms", optional(duration())),
        ("input_first_visible_status_max_ms", optional(duration())),
        ("input_cancellation_to_idle_max_ms", optional(duration())),
    ],
};

/// `update completed` plus the per-phase events (one per status
/// transition, the same names the `phase` property carries).
const UPDATE_EVENTS: &[EventRule] = &[
    EventRule {
        name: "update completed",
        since: 1,
        properties: &[
            (
                "outcome",
                required(enum_rule(
                    &["complete", "skipped", "aborted", "failed"],
                    "failed",
                )),
            ),
            ("sessions_total", required(count())),
            ("sessions_restored", required(count())),
            ("sessions_failed", required(count())),
        ],
    },
    EventRule {
        name: "update_download_started",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
            ("sessions_total", optional(count())),
            ("sessions_restored", optional(count())),
            ("sessions_failed", optional(count())),
        ],
    },
    EventRule {
        name: "update_staged",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_prepare_started",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_prepared",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_stopping",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_restarting",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_restoring",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_complete",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
            ("sessions_total", optional(count())),
            ("sessions_restored", optional(count())),
            ("sessions_failed", optional(count())),
        ],
    },
    EventRule {
        name: "update_rollback",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
            ("sessions_total", optional(count())),
            ("sessions_restored", optional(count())),
            ("sessions_failed", optional(count())),
        ],
    },
    // The installer funnel refuses a Homebrew-owned executable before
    // downloading or running the installer. `kind` is the package layout,
    // never an executable path; invalid values normalize to `formula`.
    EventRule {
        name: "update_homebrew_refusal",
        since: 2,
        properties: &[("kind", required(enum_rule(&["formula", "cask"], "formula")))],
    },
    EventRule {
        name: "update_aborted",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
    EventRule {
        name: "update_failed",
        since: 1,
        properties: &[
            ("phase", required(free_string(64))),
            ("duration_ms", required(duration())),
        ],
    },
];

const HARNESS_REFINEMENTS_CHANGED: EventRule = EventRule {
    name: "harness refinements changed",
    since: 2,
    properties: &[("enabled", required(boolean()))],
};

/// Adoption of the web search provider setting: the chosen provider's id only.
const WEB_SEARCH_PROVIDER_CHANGED: EventRule = EventRule {
    name: "web search provider changed",
    since: 2,
    properties: &[(
        "provider",
        required(enum_rule(
            &[
                "auto",
                "serper",
                "eco",
                "brave",
                "tavily",
                "exa",
                "kagi",
                "perplexity",
                "parallel",
                "jina",
                "firecrawl",
                "searxng",
                "duckduckgo",
                "startpage",
                "mojeek",
                "ecosia",
                "hackernews",
                "public",
            ],
            "auto",
        )),
    )],
};

/// The kernel `todo.*` and `ask(...)` adoption counters for one session (v2), emitted at session
/// close beside `agent session ended` (which sits at the 128-key cap) when the session used
/// either. Counts only: never item text, a question, a choice, or an answer.
const AGENT_TODOS_AND_QUESTIONS: EventRule = EventRule {
    name: "agent todos and questions",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("todo_list_set_count", required(count())),
        ("todo_item_done_count", required(count())),
        ("question_asked_count", required(count())),
        ("question_answered_count", required(count())),
        ("question_declined_count", required(count())),
        ("question_timed_out_count", required(count())),
        ("question_no_user_count", required(count())),
        ("question_cancelled_count", required(count())),
    ],
};

/// Provider request admission for one session's process (v2), emitted at session close beside
/// `agent session ended` (which sits at the 128-key cap) when any request waited for a permit or
/// a provider pushed back: requests that waited, their summed wait, throttled/overloaded
/// responses seen, and the adaptive controller's multiplicative decreases. Counts only: never a
/// provider URL, model, or credential.
const AGENT_PROVIDER_ADMISSION: EventRule = EventRule {
    name: "agent provider admission",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("admission_waited_count", required(count())),
        ("admission_wait_ms", required(duration())),
        ("provider_throttled_count", required(count())),
        ("admission_decrease_count", required(count())),
    ],
};

/// The opt-in quota fallback for one session's process (v2), emitted at session close beside
/// `agent session ended` (which sits at the 128-key cap) when a provider's declared
/// `auth.fallback` was consulted: fallbacks chosen for a quota-exhausted request, and quota
/// failures no fallback could serve. Counts only: never a provider, model, or credential.
const AGENT_QUOTA_FALLBACK: EventRule = EventRule {
    name: "agent quota fallback",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("quota_fallback_chosen_count", required(count())),
        ("quota_fallback_exhausted_count", required(count())),
    ],
};

/// Session-boot admission for one session's process (v2), emitted at session close beside
/// `agent session ended` (which sits at the 128-key cap) when a session boot (a subagent's
/// worker spawn or a kernel start) waited for the daemon-wide boot budget or finished under
/// CPU or memory pressure: boots that waited, their summed wait, and boots under pressure.
/// Counts only.
const AGENT_SESSION_BOOT_ADMISSION: EventRule = EventRule {
    name: "agent session boot admission",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("boot_admission_waited_count", required(count())),
        ("boot_admission_wait_ms", required(duration())),
        ("boot_pressure_count", required(count())),
    ],
};

/// Provider connection re-warm for one session's process (v2), emitted at session close beside
/// `agent session ended` (which sits at the 128-key cap) when a turn start re-opened an idle
/// provider connection: warm-ups sent, those that found the connection closed and reconnected
/// (a handshake the turn's first request no longer paid), and the summed time of those
/// reconnects. Counts only: never a provider URL or origin.
const AGENT_CONNECTION_REWARM: EventRule = EventRule {
    name: "agent connection rewarm",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("connection_rewarm_count", required(count())),
        ("connection_rewarm_reopened_count", required(count())),
        ("connection_rewarm_reopened_ms", required(duration())),
    ],
};

/// Goal verification adoption for one session (v2), emitted at session end beside `agent session
/// ended` only when a goal ran a verifier or hit a budget: `check` runs, failures, and dedupe skips
/// (an unchanged workspace), judge calls and NOT MET verdicts, and token/dollar budget stops.
/// Standalone because `agent session ended` sits near the backend's 128-key cap. Counts only:
/// never a command, objective, or judge text.
const AGENT_GOAL_VERIFICATION: EventRule = EventRule {
    name: "agent goal verification",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("goal_check_run_count", required(count())),
        ("goal_check_failed_count", required(count())),
        ("goal_check_skipped_count", required(count())),
        ("goal_judge_call_count", required(count())),
        ("goal_judge_not_met_count", required(count())),
        ("goal_token_budget_stop_count", required(count())),
        ("goal_cost_budget_stop_count", required(count())),
    ],
};

/// `/better` adoption for one session (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a rewrite ran: runs and how each ended
/// (completed, cancelled, failed), and what the user did with an inserted rewrite (sent as is,
/// edited, discarded), the kernel's brief-mode runs and how many needed no fix, and the refine passes verification ran. Counts only:
/// never a prompt, a rewrite, or a path.
const AGENT_BETTER_PROMPT: EventRule = EventRule {
    name: "agent better prompt",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("better_run_count", required(count())),
        ("better_completed_count", required(count())),
        ("better_cancelled_count", required(count())),
        ("better_failed_count", required(count())),
        ("better_sent_as_is_count", required(count())),
        ("better_edited_count", required(count())),
        ("better_discarded_count", required(count())),
        ("better_brief_run_count", required(count())),
        ("better_brief_unchanged_count", required(count())),
        ("better_verification_pass_count", required(count())),
    ],
};

/// A distribution build's one-time history import (v2), emitted by the daemon once the import
/// that copies the official build's chats into the build's own agent dir settles (its automatic
/// run after boot, or `import-sessions`). Counts and a size bucket only: never a path, a session
/// id, or a file name.
const AGENT_SESSIONS_IMPORTED: EventRule = EventRule {
    name: "agent sessions imported",
    since: 2,
    properties: &[
        (
            "trigger",
            required(enum_rule(&["automatic", "command"], "automatic")),
        ),
        (
            "outcome",
            required(enum_rule(&["completed", "failed"], "failed")),
        ),
        ("sessions_imported_count", required(count())),
        ("sessions_kept_count", required(count())),
        ("files_imported_count", required(count())),
        ("config_entries_added_count", required(count())),
        (
            "bytes_bucket",
            required(enum_rule(
                &["lt_1mb", "lt_100mb", "lt_1gb", "lt_10gb", "ge_10gb"],
                "lt_1mb",
            )),
        ),
        ("duration_ms", required(duration())),
    ],
};

/// Responses transport adoption for one session (v2), emitted at session end beside `agent session
/// ended` (which sits at the backend's 128-key cap) only when a request chose a Responses
/// transport: WebSocket requests (continuation deltas vs full context), SSE fallbacks, dropped
/// continuations resent in full, the `auto` choice by reason, endpoint capability probes, and
/// WebSocket requests on providers without the mode. Counts only: never an endpoint or a body.
const AGENT_RESPONSES_TRANSPORT: EventRule = EventRule {
    name: "agent responses transport",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("responses_websocket_request_count", required(count())),
        ("responses_delta_request_count", required(count())),
        ("responses_full_context_request_count", required(count())),
        ("responses_sse_fallback_count", required(count())),
        (
            "responses_stale_continuation_retry_count",
            required(count()),
        ),
        ("responses_auto_websocket_count", required(count())),
        ("responses_auto_sse_one_shot_count", required(count())),
        (
            "responses_auto_sse_no_websocket_wire_count",
            required(count()),
        ),
        ("responses_auto_sse_unsupported_count", required(count())),
        ("responses_auto_sse_demoted_count", required(count())),
        ("responses_probe_count", required(count())),
        ("responses_probe_failed_count", required(count())),
        ("responses_websocket_without_wire_count", required(count())),
    ],
};

/// Background endpoint discovery in the session's worker (v2), emitted at session end beside
/// `agent session ended` only when discovery probed or moved an endpoint: zero-cost probes sent,
/// probes that found the Responses route or its WebSocket mode, endpoints whose built-in models
/// moved from Chat Completions to the Responses wire, and moved endpoints demoted after the route
/// failed. Counts only: never a host or a URL.
const AGENT_ENDPOINT_DISCOVERY: EventRule = EventRule {
    name: "agent endpoint discovery",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("endpoint_probe_count", required(count())),
        ("endpoint_found_responses_count", required(count())),
        ("endpoint_found_websocket_count", required(count())),
        ("endpoint_upgrade_count", required(count())),
        ("endpoint_demotion_count", required(count())),
    ],
};

/// Kernel `web.read` adoption for one session (v2), emitted at session end beside `agent session
/// ended` (which sits near the backend's 128-key cap) only when the session read a URL: reads,
/// failures, reads by kind (a site route such as arXiv/GitHub/Stack Exchange, a PDF, a generic
/// page), truncated reads, and the slowest read. Counts only: never a URL, a title, or content.
const AGENT_WEB_READS: EventRule = EventRule {
    name: "agent web reads",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("web_read_count", required(count())),
        ("web_read_failed_count", required(count())),
        ("web_read_site_count", required(count())),
        ("web_read_pdf_count", required(count())),
        ("web_read_page_count", required(count())),
        ("web_read_truncated_count", required(count())),
        ("web_read_max_ms", required(duration())),
    ],
};

/// Kernel native file search (`fs.grep`/`fs.glob`/`fs.find`) adoption for one session (v2),
/// emitted at session end beside `agent session ended` (which sits near the backend's 128-key cap)
/// only when the session searched: calls by operation, failures, capped calls, results returned,
/// and the slowest call. Counts only: never a pattern, a path, or a match.
const AGENT_FS_SEARCH: EventRule = EventRule {
    name: "agent fs search",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("fs_grep_count", required(count())),
        ("fs_glob_count", required(count())),
        ("fs_find_count", required(count())),
        ("fs_search_failed_count", required(count())),
        ("fs_search_truncated_count", required(count())),
        ("fs_search_result_count", required(count())),
        ("fs_search_max_ms", required(duration())),
    ],
};

/// Kernel anchored (hashline) file reads and edits (`fs.read`/`fs.edit`) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits near the backend's 128-key
/// cap) only when the session used them: reads and lines shown, failed reads, patches applied and
/// the edits in them, patches rejected for stale anchors and the stale anchors, and failed edits.
/// Counts only: never a path, an anchor, or file content.
const AGENT_FS_EDIT: EventRule = EventRule {
    name: "agent fs edit",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("fs_read_count", required(count())),
        ("fs_read_line_count", required(count())),
        ("fs_read_failed_count", required(count())),
        ("fs_edit_count", required(count())),
        ("fs_edit_op_count", required(count())),
        ("fs_edit_rejected_count", required(count())),
        ("fs_edit_stale_anchor_count", required(count())),
        ("fs_edit_failed_count", required(count())),
    ],
};

/// Kernel session-owned scheduled jobs (`schedule.add/list/cancel`) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits at the backend's 128-key
/// cap) only when the session used them: jobs added by kind, listings, cancels, and refused calls.
/// Counts only: never a prompt, a schedule, or a job id.
const AGENT_REPL_SCHEDULE: EventRule = EventRule {
    name: "agent repl schedule",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("schedule_once_added_count", required(count())),
        ("schedule_recurring_added_count", required(count())),
        ("schedule_list_count", required(count())),
        ("schedule_cancelled_count", required(count())),
        ("schedule_rejected_count", required(count())),
    ],
};

/// Kernel workspace checkpoints (`checkpoint.create/list/restore`) adoption for one session (v2),
/// emitted at session end beside `agent session ended` (which sits at the backend's 128-key cap)
/// only when the session used them: checkpoints taken, listings, and restores by outcome
/// (restored, declined or unanswered, refused by the off-by-default setting) plus failed calls.
/// Counts only: never a label, an id, or a path.
const AGENT_REPL_CHECKPOINT: EventRule = EventRule {
    name: "agent repl checkpoint",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("checkpoint_created_count", required(count())),
        ("checkpoint_list_count", required(count())),
        ("checkpoint_restored_count", required(count())),
        ("checkpoint_restore_declined_count", required(count())),
        ("checkpoint_restore_disabled_count", required(count())),
        ("checkpoint_failed_count", required(count())),
    ],
};

/// Kernel `usage.get()` adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when the session read
/// its usage. A count only: never a cost, a token number, a provider, or a quota.
const AGENT_USAGE_READS: EventRule = EventRule {
    name: "agent usage reads",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("usage_read_count", required(count())),
    ],
};

/// Paged-out output for one session (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when the session spilled or read paged-out
/// output: spills (cut cell output and thinned results written to the session's spill store), their
/// size as a bucket, spills the store could not keep, evictions, and the kernel `ctx.*` reads.
/// Counts and a size bucket only: never a handle, a pattern, or any output text.
const AGENT_CONTEXT_PAGING: EventRule = EventRule {
    name: "agent context paging",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("spill_count", required(count())),
        (
            "spill_bytes_bucket",
            required(enum_rule(
                &[
                    "none", "lt_1mb", "lt_16mb", "lt_64mb", "lt_256mb", "ge_256mb",
                ],
                "none",
            )),
        ),
        ("spill_not_stored_count", required(count())),
        ("spill_evicted_count", required(count())),
        ("ctx_page_count", required(count())),
        ("ctx_grep_count", required(count())),
        ("ctx_tail_count", required(count())),
        ("ctx_info_count", required(count())),
    ],
};

/// How a session's recorded assistant calls are paid for (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) when the session recorded a
/// classified call: the calls per billing mode. Counts only: never a cost, a provider, a model,
/// or a credential.
const AGENT_BILLING_MODES: EventRule = EventRule {
    name: "agent billing modes",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("billing_pay_as_you_go_count", required(count())),
        ("billing_plan_count", required(count())),
        ("billing_flat_rate_count", required(count())),
        ("billing_free_count", required(count())),
        ("billing_unknown_count", required(count())),
    ],
};

/// Agent-to-agent messaging (`agent_message.send`) for one session (v2), emitted at session end
/// beside `agent session ended` (which sits near the backend's 128-key cap) only when a delivery
/// was refused: by the sender's per-session rate bucket (`agent_message_throttled_count`) or by an
/// idle receiver's per-sender wake budget (`agent_message_wake_refused_count`); a broadcast counts
/// each refused recipient. Counts only: never a target, a name, or a message body.
const AGENT_MESSAGES: EventRule = EventRule {
    name: "agent messages",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("agent_message_throttled_count", required(count())),
        ("agent_message_wake_refused_count", required(count())),
    ],
};

/// Kernel `agent_message.wait` adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when the session waited:
/// the waits by outcome (a message arrived, the timeout passed, the wait would have closed a
/// cycle, no family member matched the sender). Counts only: never a sender, a name, or a body.
const AGENT_MESSAGE_WAITS: EventRule = EventRule {
    name: "agent message waits",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("agent_message_wait_message_count", required(count())),
        ("agent_message_wait_timeout_count", required(count())),
        ("agent_message_wait_cycle_count", required(count())),
        ("agent_message_wait_no_sender_count", required(count())),
    ],
};

/// Kernel `rlm.abort` (stop a child's current turn, keep the child) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits at the backend's 128-key
/// cap) only when the session aborted a child: aborts that stopped a turn, aborts of an idle
/// child, and failed calls. Counts only: never a child id or name.
const AGENT_RLM_CHILD_ABORT: EventRule = EventRule {
    name: "agent rlm child abort",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("rlm_abort_aborted_count", required(count())),
        ("rlm_abort_idle_count", required(count())),
        ("rlm_abort_failed_count", required(count())),
    ],
};

/// Python cells that continued in the background (v2), emitted at session end beside `agent
/// session ended` (which sits near the backend's 128-key cap) only when a cell was armed with a
/// yield window or outlived one: cells backgrounded, how they ended (value, exception,
/// interrupted), and where each armed window came from (learned from the project's recent cells,
/// the model's `timeout`, the user's `pythonCellTimeoutMs`, or the cold 60 s default). Counts only:
/// never code, output, a value, a cell id, or a duration.
const AGENT_REPL_BACKGROUND_CELLS: EventRule = EventRule {
    name: "agent repl background cells",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("cell_backgrounded_count", required(count())),
        ("cell_background_ok_count", required(count())),
        ("cell_background_error_count", required(count())),
        ("cell_background_interrupted_count", required(count())),
        ("cell_window_learned_count", required(count())),
        ("cell_window_timeout_count", required(count())),
        ("cell_window_setting_count", required(count())),
        ("cell_window_default_count", required(count())),
    ],
};

/// Kernel `frida.*` (Frida dynamic instrumentation) adoption for one session (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// the session used it: device and process listings, kills, attaches and spawns, scripts loaded,
/// traces, discovers, console-tool runs (`frida.tool` and `frida.compile`), `gum-graft` runs, and
/// calls refused because frida was unavailable or still installing. Counts only: never a target,
/// a pid, a device, a script, or a path.
const AGENT_REPL_FRIDA: EventRule = EventRule {
    name: "agent repl frida",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("frida_devices_count", required(count())),
        ("frida_ps_count", required(count())),
        ("frida_kill_count", required(count())),
        ("frida_attach_count", required(count())),
        ("frida_spawn_count", required(count())),
        ("frida_script_load_count", required(count())),
        ("frida_trace_count", required(count())),
        ("frida_discover_count", required(count())),
        ("frida_tool_count", required(count())),
        ("frida_graft_count", required(count())),
        ("frida_unavailable_count", required(count())),
        ("frida_installing_count", required(count())),
    ],
};

/// Kernel `fs.symbol` adoption (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when the session asked a query: queries answered from the
/// project's symbol index, those answered before its first full pass (partial), and the definitions
/// and references returned. Counts only: never a name or a path.
const AGENT_SYMBOL_INDEX: EventRule = EventRule {
    name: "agent symbol index",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("symbol_query_count", required(count())),
        ("symbol_query_partial_count", required(count())),
        ("symbol_definition_count", required(count())),
        ("symbol_reference_count", required(count())),
    ],
};

/// Error enrichment (v2), emitted at session end beside `agent session ended` (which sits near the
/// backend's 128-key cap) only when a cell failed or awaited a failed `bash()` command: results that
/// got the source lines around their error locations, the locations attached, the parsed locations
/// left out (outside the workspace, unreadable, or past the cap), source reads that ran out of
/// time, and the failed commands seen. Counts only: never a path, a line, a command, or output.
const AGENT_ERROR_ENRICHMENT: EventRule = EventRule {
    name: "agent error enrichment",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("error_enrichment_count", required(count())),
        ("error_location_count", required(count())),
        ("error_location_skipped_count", required(count())),
        ("error_enrichment_timeout_count", required(count())),
        ("error_failed_command_count", required(count())),
    ],
};

/// The kernel's read-only command cache (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a lookup ran: allowlisted read-only
/// `bash()` commands (`git status`/`log`/`diff`, `ls`, `rg`/`grep`, `cat`) answered from the cache
/// or run, and `fs.read` calls answered from the cache or read. Counts only: never a command, a
/// path, or output.
const AGENT_READONLY_CACHE: EventRule = EventRule {
    name: "agent readonly cache",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("readonly_cache_hit_count", required(count())),
        ("readonly_cache_miss_count", required(count())),
        ("readonly_cache_fs_read_hit_count", required(count())),
        ("readonly_cache_fs_read_miss_count", required(count())),
    ],
};

/// Edit-then-check (v2), emitted at session end beside `agent session ended` (which sits near the
/// backend's 128-key cap) only when an edit was checked: checks run after an edit (`fs.edit` or the
/// `edit` skill), those that found diagnostics, those that outlived the edit's wait (their
/// diagnostics arrive as a notice), those dropped because their learned cost exceeds their benefit,
/// and those that could not run. Counts only: never a path, a command, or output.
const AGENT_EDIT_CHECKS: EventRule = EventRule {
    name: "agent edit checks",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("edit_check_run_count", required(count())),
        ("edit_check_found_count", required(count())),
        ("edit_check_late_count", required(count())),
        ("edit_check_skipped_count", required(count())),
        ("edit_check_failed_count", required(count())),
    ],
};

/// Typed test-run summaries (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when the kernel summarized an awaited test run (cargo test,
/// pytest, unittest, go test, jest, vitest): runs summarized, runs with failures, the failed tests
/// they counted, the failures that got their own output handle, and runs whose whole output got a
/// handle. Counts only: never a test name, a path, a command, or output.
const AGENT_TEST_SUMMARIES: EventRule = EventRule {
    name: "agent test summaries",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("test_summary_count", required(count())),
        ("test_summary_failing_count", required(count())),
        ("test_summary_failed_test_count", required(count())),
        ("test_summary_failure_handle_count", required(count())),
        ("test_summary_stored_count", required(count())),
    ],
};

/// One descriptor-driven provider sign-in settled (v2): a models.json provider whose `auth`
/// block declares `oauth`, signed in through `login <provider>` (`surface` `cli`) or the TUI's
/// `/login` picker (`tui`). The flow the sign-in ran (`none` when it never started), how it
/// ended, and how long it took. Never a provider id, a URL, a client id, a code, or a token.
const AGENT_PROVIDER_OAUTH_LOGIN: EventRule = EventRule {
    name: "agent provider oauth login",
    since: 2,
    properties: &[
        ("surface", required(enum_rule(&["cli", "tui"], "cli"))),
        (
            "flow",
            required(enum_rule(&["browser", "device_code", "none"], "none")),
        ),
        (
            "outcome",
            required(enum_rule(&["completed", "failed", "canceled"], "failed")),
        ),
        ("duration_ms", required(duration())),
    ],
};

/// Kernel `notify(...)` and `model.set_effort(...)` adoption for one session (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// the session pinged or switched effort: pings, pings nobody was attached to see, and effort
/// switches applied and refused. Counts only: never a message or a level.
const AGENT_REPL_NOTIFY: EventRule = EventRule {
    name: "agent repl notify",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("notify_count", required(count())),
        ("notify_undelivered_count", required(count())),
        ("effort_set_count", required(count())),
        ("effort_refused_count", required(count())),
    ],
};

/// Kernel `rlm.reload()` and `settings.get()` adoption for one session (v2), emitted at session
/// end beside `agent session ended` (which sits near the backend's 128-key cap) only when the
/// session reloaded or read its settings: reloads, the skills they added, and settings reads.
/// Counts only: never a skill name, a path, or a setting value.
const AGENT_REPL_RELOAD: EventRule = EventRule {
    name: "agent repl reload",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("repl_reload_count", required(count())),
        ("repl_reload_skills_added_count", required(count())),
        ("repl_settings_get_count", required(count())),
    ],
};

/// Kernel `mcp.login` and past-session recall (`rlm.session.search`, `rlm.session.messages(
/// session=...)`) adoption for one session (v2), emitted at session end beside `agent session
/// ended` (which sits at the backend's 128-key cap) only when the session used them: logins by
/// outcome (logged in, failed, no interactive login in the run), searches, the hits they
/// returned, saved-transcript reads, and failed recall calls. Counts only: never a server, a
/// query, a session, or a snippet.
const AGENT_KERNEL_LOGIN_AND_RECALL: EventRule = EventRule {
    name: "agent kernel login and recall",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("kernel_mcp_login_count", required(count())),
        ("kernel_mcp_login_failed_count", required(count())),
        ("kernel_mcp_login_unavailable_count", required(count())),
        ("session_search_count", required(count())),
        ("session_search_hit_count", required(count())),
        ("session_search_read_count", required(count())),
        ("session_search_failed_count", required(count())),
    ],
};

/// Every catalogued event, flattened.
#[must_use]
pub fn catalog() -> Vec<&'static EventRule> {
    let mut all: Vec<&'static EventRule> = vec![
        &AGENT_GOAL_VERIFICATION,
        &AGENT_BETTER_PROMPT,
        &HARNESS_REFINEMENTS_CHANGED,
        &WEB_SEARCH_PROVIDER_CHANGED,
        &AGENT_STARTED,
        &AGENT_RUN_COMPLETED,
        &AGENT_SESSION_ENDED,
        &AGENT_COMMAND_USED,
        &AUTOCOMPACT_THRESHOLD_CHANGED,
        &ONBOARDING_STAGE,
        &ONBOARDING_COMPLETED,
        &AGENT_STARTUP_STAGE,
        &AGENT_INSTALLATION_STAGE,
        &STARTUP,
        &DAEMON_EVENT,
        &MODEL_REFUSED,
        &SESSION_ARCHIVED,
        &TUI_EXIT,
        &TUI_IPYTHON_BASH_RENDERED,
        &TUI_RESUME_PICKER_CLOSED,
        &TUI_USAGE_PAGE_CLOSED,
        &TUI_IMAGE_PASTE,
        &AGENT_CREDENTIAL_ADDED,
        &AGENT_CREDENTIAL_PINNED,
        &TUI_CONTEXT_WINDOW_CLOSED,
        &TUI_INLINE_IMAGES_RENDERED,
        &TUI_TODOS_AND_QUESTIONS,
        &AGENT_TODOS_AND_QUESTIONS,
        &AGENT_SESSIONS_IMPORTED,
        &AGENT_WEB_READS,
        &AGENT_FS_SEARCH,
        &AGENT_FS_EDIT,
        &AGENT_REPL_SCHEDULE,
        &AGENT_REPL_CHECKPOINT,
        &AGENT_USAGE_READS,
        &AGENT_CONTEXT_PAGING,
        &AGENT_BILLING_MODES,
        &AGENT_MESSAGES,
        &AGENT_MESSAGE_WAITS,
        &AGENT_RLM_CHILD_ABORT,
        &AGENT_REPL_BACKGROUND_CELLS,
        &AGENT_REPL_FRIDA,
        &AGENT_SYMBOL_INDEX,
        &AGENT_ERROR_ENRICHMENT,
        &AGENT_READONLY_CACHE,
        &AGENT_EDIT_CHECKS,
        &AGENT_TEST_SUMMARIES,
        &AGENT_PROVIDER_OAUTH_LOGIN,
        &AGENT_REPL_NOTIFY,
        &AGENT_REPL_RELOAD,
        &AGENT_KERNEL_LOGIN_AND_RECALL,
        &AGENT_RESPONSES_TRANSPORT,
        &AGENT_PROVIDER_ADMISSION,
        &AGENT_SESSION_BOOT_ADMISSION,
        &AGENT_CONNECTION_REWARM,
        &AGENT_QUOTA_FALLBACK,
        &AGENT_ENDPOINT_DISCOVERY,
    ];
    all.extend(UPDATE_EVENTS.iter());
    all
}

/// The `feature_<name>_<outcome>_count` counter key for a feature outcome
/// in the fixed vocabulary (`None` outside it).
#[must_use]
pub fn feature_outcome_key(feature_name: &str, outcome: &str) -> Option<String> {
    (FEATURE_NAMES.contains(&feature_name) && FEATURE_OUTCOMES.contains(&outcome))
        .then(|| format!("feature_{feature_name}_{outcome}_count"))
}

/// The `input_<stage>_*` key prefix for an input stage in the fixed
/// vocabulary (`None` outside it).
#[must_use]
pub fn input_stage_key(stage: &str) -> Option<String> {
    INPUT_STAGES
        .contains(&stage)
        .then(|| format!("input_{stage}"))
}

/// Look up one event's rule.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static EventRule> {
    catalog().into_iter().find(|rule| rule.name == name)
}

/// Normalize one event's properties against its catalog rule: unknown
/// keys are dropped, out-of-vocabulary enums fall back, numbers clamp to
/// their caps, strings cap at their byte budget. Events outside the
/// catalog pass through unchanged.
///
/// Returns the number of properties adjusted or dropped.
pub fn sanitize(name: &str, properties: &mut Properties) -> usize {
    let Some(rule) = lookup(name) else {
        // Not catalogued: the primitive-only boundary still applies.
        return 0;
    };
    let mut adjusted = 0usize;
    let mut normalized = Properties::new();
    for (key, value) in properties.iter() {
        if let Some((_, base_rule)) = BASE_PROPERTIES.iter().find(|(known, _)| known == key) {
            if let Some(value) = base_rule.kind.normalize(value.clone()) {
                normalized.insert_validated(key, value);
            } else {
                adjusted += 1;
            }
            continue;
        }
        let Some((_, property_rule)) = rule.properties.iter().find(|(known, _)| known == key)
        else {
            // Unknown property: the privacy contract keeps the catalog
            // the complete property set.
            tracing::debug!(event = name, key, "dropped uncatalogued telemetry property");
            adjusted += 1;
            continue;
        };
        if let Some(value) = property_rule.kind.normalize(value.clone()) {
            if &value != value_ref(properties, key) {
                adjusted += 1;
            }
            // Already validated against the rule; the primitive-map
            // exception inserts through the internal path (the public
            // `set` boundary stays primitive-only).
            normalized.insert_validated(key, value);
        } else {
            tracing::debug!(event = name, key, "dropped invalid telemetry property");
            adjusted += 1;
        }
    }
    for (key, property_rule) in rule.properties {
        if property_rule.required && normalized.get(key).is_none() {
            tracing::debug!(event = name, key, "missing required telemetry property");
        }
    }
    *properties = normalized;
    adjusted
}

/// Read back one property (sanitize bookkeeping).
fn value_ref<'a>(properties: &'a Properties, key: &str) -> &'a Value {
    properties.get(key).unwrap_or(&Value::Null)
}

/// True for a hex uuid in the canonical dashed shape.
pub(crate) fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let dashes = [8, 13, 18, 23];
    dashes.iter().all(|&p| bytes[p] == b'-')
        && (0..36)
            .filter(|&i| !dashes.contains(&i))
            .all(|i| bytes[i].is_ascii_hexdigit())
}

/// Cap a string at `max` bytes on a char boundary.
fn cap_string(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalog_is_the_low_frequency_set() {
        assert_eq!(SCHEMA_VERSION, 2);
        let names: Vec<&str> = catalog().iter().map(|rule| rule.name).collect();
        for name in [
            "agent started",
            "agent run completed",
            "agent session ended",
            "agent command used",
            "onboarding completed",
            "onboarding stage",
            "agent startup stage",
            "agent installation stage",
            "session archived",
            "startup",
            "daemon event",
            "model refused",
            "update completed",
            "tui exit",
        ] {
            assert!(names.contains(&name), "{name} stays catalogued");
        }
        // The per-occurrence events folded into run/session/client counters.
        for name in [
            "agent run started",
            "agent error",
            "agent timing",
            "agent tool summary",
            "agent feature outcome",
            "agent input stage",
            "tool executed",
            "skill used",
            "mcp connector used",
            "rlm child usage attributed",
            "kernel bootstrap",
            "tui scroll used",
        ] {
            assert!(!names.contains(&name), "{name} is folded, not an event");
        }
    }

    /// Every property key matches the platform backend's key pattern
    /// (`^[a-zA-Z][a-zA-Z0-9_]*$`, at most 64 chars; a bad key drops the
    /// whole event there) and every event stays under its 128-key cap.
    #[test]
    fn every_key_passes_the_backend_pattern() {
        for rule in catalog() {
            assert!(
                rule.properties.len() + BASE_PROPERTIES.len() <= 128,
                "{} exceeds the backend's 128 keys",
                rule.name
            );
            for (key, _) in rule.properties.iter().chain(BASE_PROPERTIES.iter()) {
                let mut chars = key.chars();
                assert!(
                    key.len() <= 64
                        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "{}: {key}",
                    rule.name
                );
            }
            assert!(
                rule.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == ' ' || c == '_'),
                "{}",
                rule.name
            );
        }
    }

    #[test]
    fn counter_keys_cover_the_fixed_vocabularies() {
        assert_eq!(
            feature_outcome_key("goal", "completed").as_deref(),
            Some("feature_goal_completed_count")
        );
        assert_eq!(feature_outcome_key("goal", "exploded"), None);
        assert_eq!(input_stage_key("queued").as_deref(), Some("input_queued"));
        assert_eq!(input_stage_key("nope"), None);
        let ended = lookup("agent session ended").unwrap();
        let exit = lookup("tui exit").unwrap();
        for name in FEATURE_NAMES {
            for outcome in FEATURE_OUTCOMES {
                let key = feature_outcome_key(name, outcome).unwrap();
                assert!(ended.properties.iter().any(|(known, _)| *known == key));
                assert!(exit.properties.iter().any(|(known, _)| *known == key));
            }
        }
        for stage in INPUT_STAGES {
            let prefix = input_stage_key(stage).unwrap();
            for suffix in ["count", "max_ms"] {
                let key = format!("{prefix}_{suffix}");
                assert!(exit.properties.iter().any(|(known, _)| *known == key));
            }
        }
    }

    #[test]
    fn sanitize_drops_unknown_keys_and_falls_back_enums() {
        let mut properties = Properties::new();
        properties.set("session_id", json!("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"));
        properties.set("outcome", json!("success"));
        properties.set("duration_ms", json!(5));
        properties.set("trigger", json!("spontaneous")); // out of vocabulary
        properties.set("tool_bash_call_count", json!(3u64));
        properties.set("tool_name", json!("private_tool")); // not a catalogued property
        let adjusted = sanitize("agent run completed", &mut properties);
        assert_eq!(properties.get("trigger"), Some(&json!("unknown")));
        assert_eq!(properties.get("tool_bash_call_count"), Some(&json!(3u64)));
        assert!(properties.get("tool_name").is_none(), "unknown key dropped");
        assert_eq!(adjusted, 2, "one fallback + one dropped key");
    }

    #[test]
    fn sanitize_clamps_numbers_and_caps_strings() {
        let mut properties = Properties::new();
        properties.set("session_id", json!("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"));
        properties.set("outcome", json!("success"));
        properties.set("duration_ms", json!(5));
        properties.set("turn_count", json!(u64::MAX)); // over the count cap
        properties.set("retry_count", json!(3.5)); // fractional where integer
        let _ = sanitize("agent run completed", &mut properties);
        assert_eq!(properties.get("turn_count"), Some(&json!(1_000_000u64)));
        assert!(properties.get("retry_count").is_none(), "fraction dropped");
    }

    #[test]
    fn sanitize_keeps_null_only_where_nullable() {
        let mut properties = Properties::new();
        properties.set("session_id", json!("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"));
        properties.set("outcome", json!("error"));
        properties.set("duration_ms", json!(120));
        properties.set("error_category", Value::Null); // nullable
        properties.set("compaction_count", Value::Null); // NOT nullable
        let _ = sanitize("agent run completed", &mut properties);
        assert_eq!(properties.get("error_category"), Some(&Value::Null));
        assert!(properties.get("compaction_count").is_none());
    }

    #[test]
    fn sanitize_passes_uncatalogued_events_through() {
        let mut properties = Properties::new();
        properties.set("anything", json!("kept"));
        assert_eq!(sanitize("a future event", &mut properties), 0);
        assert_eq!(properties.get("anything"), Some(&json!("kept")));
    }

    #[test]
    fn uuid_shape_and_string_caps() {
        assert!(is_uuid("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1"));
        assert_eq!(cap_string("abcdef", 3), "abc");
        let capped = cap_string("private tool text", 7);
        assert_eq!(capped, "private");
    }

    #[test]
    fn primitive_map_allows_only_primitives() {
        let mut timings = Properties::new();
        timings.set("daemon_ready", json!(120));
        let mut nested = Properties::new();
        nested.set_map("phase_timings", &timings);
        let mut properties = Properties::new();
        properties.set("duration_ms", json!(200));
        properties.merge(&nested);
        properties.set("execution_mode", json!("interactive"));
        let _ = sanitize("startup", &mut properties);
        assert!(properties.get("phase_timings").is_some());
    }
}
