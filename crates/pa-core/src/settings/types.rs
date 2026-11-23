//! Settings types. All fields optional; JSON values of the wrong type load
//! as `None` (the TS access-time typechecks), never a load error.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MermaidRenderingMode {
    Off,
    Final,
    Streaming,
}

/// What an unfocused terminal is sent when a run ends or a question opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnEndNotification {
    Off,
    /// The terminal bell (BEL).
    Bell,
    /// A desktop notification through the terminal (OSC 9).
    Notification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    Stable,
    Nightly,
}

/// Thinking levels in wire form ("off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingLevelSetting {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevelSetting {
    /// The reverse of [`ThinkingLevelSetting::model_level`]: a live switch (daemon
    /// `set_thinking_level`) persists a model-vocabulary level as the settings default.
    #[must_use]
    pub fn from_model_level(level: pa_types::ai::ModelThinkingLevel) -> ThinkingLevelSetting {
        match level {
            pa_types::ai::ModelThinkingLevel::Off => ThinkingLevelSetting::Off,
            pa_types::ai::ModelThinkingLevel::Minimal => ThinkingLevelSetting::Minimal,
            pa_types::ai::ModelThinkingLevel::Low => ThinkingLevelSetting::Low,
            pa_types::ai::ModelThinkingLevel::Medium => ThinkingLevelSetting::Medium,
            pa_types::ai::ModelThinkingLevel::High => ThinkingLevelSetting::High,
            pa_types::ai::ModelThinkingLevel::Xhigh => ThinkingLevelSetting::Xhigh,
            pa_types::ai::ModelThinkingLevel::Max => ThinkingLevelSetting::Max,
        }
    }

    /// The same level in the shared model vocabulary: the settings default
    /// feeds session thinking-level resolution.
    #[must_use]
    pub fn model_level(self) -> pa_types::ai::ModelThinkingLevel {
        match self {
            ThinkingLevelSetting::Off => pa_types::ai::ModelThinkingLevel::Off,
            ThinkingLevelSetting::Minimal => pa_types::ai::ModelThinkingLevel::Minimal,
            ThinkingLevelSetting::Low => pa_types::ai::ModelThinkingLevel::Low,
            ThinkingLevelSetting::Medium => pa_types::ai::ModelThinkingLevel::Medium,
            ThinkingLevelSetting::High => pa_types::ai::ModelThinkingLevel::High,
            ThinkingLevelSetting::Xhigh => pa_types::ai::ModelThinkingLevel::Xhigh,
            ThinkingLevelSetting::Max => pa_types::ai::ModelThinkingLevel::Max,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueModeSetting {
    All,
    #[serde(alias = "oneAtATime")]
    OneAtATime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportSetting {
    Auto,
    Sse,
    WebSocket,
    #[serde(rename = "websocket-cached")]
    WebSocketCached,
}

fn deserialize_threshold_percent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_f64()
        .filter(|percent| pa_types::compaction::is_valid_threshold_percent(*percent)))
}

/// `number | "unlimited"` autonomous limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AutonomousLimitSetting {
    Number(f64),
    Unlimited(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: Option<bool>,
    /// Percentage ceiling for automatic compaction; invalid values behave as unset.
    #[serde(default, deserialize_with = "deserialize_threshold_percent")]
    pub threshold_percent: Option<f64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub agent_callable: Option<bool>,
}

/// `contextThinning`: the request-view thinning of aged tool results
/// (`session_engine::context_thinning`); unset fields take its defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextThinningSettings {
    pub enabled: Option<bool>,
    /// Estimated context share (percent of the window) past which aged results thin.
    pub threshold_percent: Option<f64>,
    /// Newest tool rounds always kept whole.
    pub keep_recent_rounds: Option<u64>,
    /// Results at or under this many characters are never thinned.
    pub min_chars: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    pub reserve_tokens: Option<u64>,
    pub skip_prompt: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoRefineSettings {
    pub enabled: Option<bool>,
    pub turn_interval: Option<u64>,
    pub compact: Option<bool>,
    pub cooldown_ms: Option<u64>,
}

/// Retired quota-wait settings (`retry.provider.waitForUsage`), kept so
/// existing settings files still load. Nothing reads them: a quota-exhausted
/// turn fails at once with the provider's raw error; no session parks until
/// a reset or resumes on its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderWaitSettings {
    pub enabled: Option<bool>,
    pub base_delay_ms: Option<u64>,
    pub max_delay_ms: Option<u64>,
    pub max_attempts: Option<u64>,
    pub max_wait_ms: Option<u64>,
    pub pause_until_reset: Option<bool>,
    pub max_pause_ms: Option<u64>,
    pub max_parks: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    pub timeout_ms: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
    pub wait_for_usage: Option<ProviderWaitSettings>,
}

/// Retired provider-failover settings (`retry.failover`), kept so existing
/// settings files still load. Nothing reads them: a session never moves to
/// another provider on its own (no cross-provider failover), and retries
/// stay on the session's provider under the `retry` schedule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryFailoverSettings {
    pub enabled: Option<bool>,
    pub max_retries: Option<u64>,
    pub base_delay_ms: Option<u64>,
    pub max_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    pub enabled: Option<bool>,
    pub max_retries: Option<u64>,
    pub base_delay_ms: Option<u64>,
    pub failover: Option<RetryFailoverSettings>,
    pub provider: Option<ProviderRetrySettings>,
}

/// `loopGuard`: the stream loop guard, which cuts a model stream that starts repeating itself
/// and nudges a model that re-runs the same tool call round after round.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopGuardSettings {
    /// Unset means on.
    pub enabled: Option<bool>,
    /// Unset means `auto`.
    pub sensitivity: Option<LoopGuardSensitivity>,
}

/// How closely the loop guard watches streamed output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopGuardSensitivity {
    /// The model family's default: off for Claude, `semantic` for the Gemini, `DeepSeek` and
    /// Grok families, `exact` for every other model.
    Auto,
    /// Exact back-to-back repeats only.
    Exact,
    /// Exact repeats plus near-duplicate and stalled paragraphs.
    Semantic,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    pub show_images: Option<bool>,
    pub clear_on_shrink: Option<bool>,
    pub show_terminal_progress: Option<bool>,
    pub fullscreen_mouse: Option<bool>,
    /// Unset means `off`.
    pub notify_on_turn_end: Option<TurnEndNotification>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    pub auto_resize: Option<bool>,
    pub block_images: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgetsSettings {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutonomousSettings {
    pub max_continuations: Option<AutonomousLimitSetting>,
    pub max_turns: Option<AutonomousLimitSetting>,
    pub max_tokens: Option<AutonomousLimitSetting>,
    pub timeout_ms: Option<AutonomousLimitSetting>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    pub code_block_indent: Option<String>,
    pub mermaid: Option<MermaidRenderingMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundledSkillsSettings {
    pub websearch: Option<bool>,
}

/// `webSearchProvider`: the provider `web.search()` and the bundled
/// websearch skill use (`auto`, the default, or one engine id); the catalog
/// lives with the native engine.
pub use crate::web_search::WebSearchProvider;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningSettings {
    pub anthropic_extra_usage: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTracesSettings {
    pub enabled: Option<bool>,
}

/// `factory` (the agent factory's opt-in gate): `factory.enabled` is
/// unset/false by default, and the `/factory on` client command persists
/// it. Both the daemon's `factory_activity` lane advertisement and the
/// kernel's factory gate read the same shared settings key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FactorySettings {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettings {
    pub enabled: Option<bool>,
    pub notice_shown: Option<bool>,
    /// Local JSONL mirror at `<agentDir>/telemetry.jsonl` (default on:
    /// user-observable transparency).
    pub local_mirror: Option<bool>,
}

/// `cacheRetention`: the prompt-cache lifetime requested from providers with
/// explicit cache markers. `short` (the default) is the provider's default
/// lifetime (five minutes on Anthropic, writes at 1.25x input); `long` asks
/// for the extended one (one hour, writes at 2x input). The
/// `PI_CACHE_RETENTION` environment variable overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheRetentionSetting {
    Short,
    Long,
}

/// `cacheWarming`: when a session keeps its prompt cache alive by re-sending
/// its last request (one output token) shortly before the cache entry
/// expires, and only when the expected saving clears the policy threshold.
/// `active` (the default) warms while the next request is near-certain: a
/// tool, a background `bash()` job, or a subagent is still running, or a
/// subagent waits on its parent. `idle` also warms an idle session, weighted
/// by the low chance of a next turn. `off` never warms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheWarmingSetting {
    Off,
    Active,
    Idle,
}

/// The yield window for the model's kernel cells before a project's window
/// is learned (and the previous fixed default): 60 seconds.
pub const DEFAULT_CELL_TIMEOUT_MS: u64 = 60_000;

/// The settings document: every known field optional; unknown keys preserved.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub onboarding_shown: Option<bool>,
    pub onboarding_completed: Option<bool>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub subagent_default_model: Option<String>,
    pub update_channel: Option<UpdateChannel>,
    pub recent_models: Option<Vec<String>>,
    /// The `/model` picker's favorites (`provider/id` keys, in the order they were added).
    pub favorite_models: Option<Vec<String>>,
    pub auxiliary_model: Option<String>,
    pub default_thinking_level: Option<ThinkingLevelSetting>,
    pub default_service_tier: Option<String>,
    pub rlm_max_depth: Option<u64>,
    /// `number | "off" | "none"`; raw JSON because the TS getter validates at access time.
    pub idle_eviction_minutes: Option<serde_json::Value>,
    pub transport: Option<TransportSetting>,
    pub steering_mode: Option<QueueModeSetting>,
    pub follow_up_mode: Option<QueueModeSetting>,
    pub theme: Option<String>,
    pub compaction: Option<CompactionSettings>,
    pub context_thinning: Option<ContextThinningSettings>,
    pub auto_refine: Option<AutoRefineSettings>,
    /// Master switch for harness creation and context loading; unset preserves enabled behavior.
    pub harness_refinements: Option<bool>,
    pub agent_traces: Option<AgentTracesSettings>,
    pub factory: Option<FactorySettings>,
    pub telemetry: Option<TelemetrySettings>,
    pub branch_summary: Option<BranchSummarySettings>,
    pub retry: Option<RetrySettings>,
    pub provider_backup_model: Option<String>,
    /// Model ("provider/model-id" or a bare model id) that serves turns attaching images when the
    /// session model does not accept image input. Default: none - image turns on a text-only model
    /// fail with a configuration hint instead of silently dropping the images.
    pub image_model: Option<String>,
    /// Fixed yield window for the model's kernel cells, in milliseconds
    /// (`0` disables). Unset, the window is learned per project from recent
    /// cells (starting at [`DEFAULT_CELL_TIMEOUT_MS`]). A cell running
    /// longer continues in the background, never stopped for time; the model
    /// can pass a per-call `timeout` instead. Daemonless sessions never use
    /// it: their cells run to the end in the call. The key keeps the name
    /// settings files already carry.
    #[serde(rename = "pythonCellTimeoutMs")]
    pub cell_timeout_ms: Option<u64>,
    pub autonomous: Option<AutonomousSettings>,
    /// Custom shell that runs every kernel `bash()` and user-bash command as
    /// `<shell> -c <command>`: an absolute path to an existing executable,
    /// validated once at session start (a bad value fails session creation
    /// naming this setting). Setting it selects a shell process for the
    /// kernel's `bash` unless `shell` says `"builtin"`. Only global settings
    /// can select this executable.
    pub shell_path: Option<String>,
    /// What runs the kernel's `bash` commands: `"builtin"` (Prime Agent's
    /// own shell and utilities, inside the process) or `"system"` (a shell
    /// process per command: `shellPath`, or the platform's default shell).
    /// Unset, the built-in shell is used on every platform, unless
    /// `shellPath` is set. Only global settings select it.
    pub shell: Option<String>,
    pub quiet_startup: Option<bool>,
    pub shell_command_prefix: Option<String>,
    pub npm_command: Option<Vec<String>>,
    /// Execution reads this from global settings only; project overrides are ignored.
    pub mcp_servers: Option<serde_json::Map<String, serde_json::Value>>,
    /// Extra local MCP service-catalog files (~-relative allowed), merged after the
    /// compiled built-ins; first source wins per id, and no bundled id can be shadowed.
    /// Read from global settings only, like executable MCP server configuration.
    pub mcp_catalog_sources: Option<Vec<String>>,
    pub packages: Option<Vec<serde_json::Value>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    pub enable_skill_commands: Option<bool>,
    pub bundled_skills: Option<BundledSkillsSettings>,
    /// Read from global settings only, like the stored keys it selects between.
    pub web_search_provider: Option<WebSearchProvider>,
    pub enable_builtin_skills: Option<bool>,
    pub terminal: Option<TerminalSettings>,
    pub images: Option<ImageSettings>,
    pub enabled_models: Option<Vec<String>>,
    /// Rust-only daemon-level model allowlist: a model outside fails loudly instead
    /// of resolving, with no fallback. `None` is unrestricted.
    pub allowed_models: Option<Vec<String>>,
    pub tree_filter_mode: Option<String>,
    /// `chatDetail`: the conversation-detail level Ctrl+O cycles and persists
    /// ("overview"/"details"/"all"); `None` reads as the `overview` startup level.
    pub chat_detail: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    pub editor_padding_x: Option<u64>,
    /// How many times the user has used the key the empty composer's self-retiring hint
    /// names; the hint stops showing once the count reaches its retirement threshold.
    pub composer_hint_uses: Option<u64>,
    pub autocomplete_max_visible: Option<u64>,
    pub show_hardware_cursor: Option<bool>,
    /// Typing `:name` in the prompt editor suggests matching emoji (unset means on).
    pub emoji_shortcodes: Option<bool>,
    pub markdown: Option<MarkdownSettings>,
    pub warnings: Option<WarningSettings>,
    pub session_dir: Option<String>,
    /// Log per-request provider timing phases to the diagnostic log
    /// (unset means OFF, exactly the TS default).
    pub request_timing: Option<bool>,
    pub cache_retention: Option<CacheRetentionSetting>,
    pub cache_warming: Option<CacheWarmingSetting>,
    /// The stream loop guard (unset means on, at each model family's default sensitivity).
    pub loop_guard: Option<LoopGuardSettings>,
    /// Lets the kernel's `checkpoint.restore` put the workspace back to a checkpoint the
    /// session took, after the user confirms (unset means off). Global settings only.
    pub repl_can_restore_checkpoints: Option<bool>,
    /// Lets the kernel's `model.set_effort(...)` change the session's thinking level (unset
    /// means off: it changes cost). Read from the global scope only.
    pub repl_can_change_effort: Option<bool>,
    /// Unknown keys survive load/save round-trips (forward compatibility).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
