//! Layered system-prompt assembly: static layer files first (cacheable),
//! every session-specific value last. See the parent module docs.

use crate::skills::{format_skills_for_prompt, Skill};

use super::layers;

/// The bundled skill the refinement trigger guidance keys on.
pub const REFINE_SKILL_NAME: &str = "refine";

/// Whether a prompt segment belongs to the cache-stable prefix or the
/// session-specific tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Cacheable static prefix content (layer files, or a user replacement).
    Static,
    /// Session-specific tail content; must never leak into [`SegmentKind::Static`].
    Dynamic,
}

/// One named slice of the assembled prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSegment {
    /// Stable segment name (`core`, `usage`, `packages`, `environment`, ...).
    pub name: &'static str,
    pub kind: SegmentKind,
    /// Provenance: the layer file, or what generated the segment.
    pub source: &'static str,
    pub text: String,
}

impl PromptSegment {
    fn static_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Static,
            source,
            text,
        }
    }

    fn dynamic_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Dynamic,
            source,
            text,
        }
    }
}

/// The assembled prompt plus its per-layer breakdown. `cached_prefix_len` is
/// the byte length of the static prefix: `assembled[..cached_prefix_len]` is
/// byte-identical across every session that shares the same custom-prompt
/// and model-selection inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptBreakdown {
    pub segments: Vec<PromptSegment>,
    pub assembled: String,
    pub cached_prefix_len: usize,
}

/// Inputs for one session's prompt. Static inputs (`custom_prompt`,
/// `model_prompt_extras`) select the cached prefix; everything else feeds
/// the dynamic tail.
#[derive(Debug, Default)]
pub struct BuildSystemPromptOptions<'a> {
    /// Replaces the layered static prefix with user text. The dynamic tail
    /// still applies.
    pub custom_prompt: Option<String>,
    pub model_prompt_extras: Option<&'a str>,
    /// Whether the resolved model accepts image input, when known.
    pub vision_capable: Option<bool>,
    /// Active tools. Tool schemas carry tool descriptions outside the prompt.
    pub selected_tools: Option<Vec<&'a str>>,
    pub append_system_prompt: Option<String>,
    pub cwd: String,
    /// Conversation log path.
    pub messages_path: Option<String>,
    /// Pre-loaded context files (path, content).
    pub context_files: Vec<(String, String)>,
    pub skills: Vec<Skill>,
    /// How each skill has been reused, read once when the prompt is built
    /// (session start, `/reload`), so the inventory's marks never change
    /// within a session's prompt.
    pub skill_standings: std::collections::BTreeMap<String, crate::reuse::Standing>,
    /// Whether to include the subagent surface in this session.
    pub allow_recursion: Option<bool>,
    /// Fixed recursive-agent depth for this session.
    pub rlm_depth: Option<u32>,
    /// Human-readable parent name or id for child communication doctrine.
    pub rlm_parent_agent: Option<&'a str>,
    /// Whether this child runs one factory state: the factory reads its
    /// final message, so the child does not reply to the parent.
    pub factory_child: bool,
    /// Whether the session runs without a daemon (no scheduler, no
    /// heartbeat firing). The wake-up facts the daemonless line states
    /// must match the embedding's wiring.
    pub daemonless: bool,
    /// Whether `rlm-spawn` can create children in this session (the
    /// daemonless print host or a daemon worker); a daemonless session
    /// without one cannot spawn at all.
    pub can_spawn: bool,
    /// Enabled user-configured generic MCP servers.
    pub generic_mcp_servers: Vec<String>,
    /// The project card (build/test commands, key dirs, learned commands),
    /// as the session read it at start.
    pub project_card: Option<String>,
    /// The machine the session's commands run on and the rights they run
    /// with: the environment section's `Machine:` line. `None` states
    /// nothing.
    pub host: Option<HostAccess>,
    /// What the user's `web.crawl` settings change in what `web-crawl`
    /// presents to sites (robots.txt, user agent, proxies, ...), in words:
    /// the environment section's `Web crawls:` line. Empty states nothing.
    pub web_crawl_identity: Vec<&'static str>,
}

/// Where a session's commands run: the user's own computer, on this
/// platform, with the daemon process's rights (every session of a daemon
/// runs in its process, so they share its elevation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAccess {
    pub route: pa_types::platform::ElevationRoute,
    pub elevation: pa_types::platform::Elevation,
}

impl HostAccess {
    /// This process's platform and elevation.
    #[must_use]
    pub fn current() -> Self {
        Self {
            route: pa_types::platform::ElevationRoute::CURRENT,
            elevation: pa_types::platform::process_elevation(),
        }
    }

    /// The environment section's line: the session is on the user's own
    /// machine, not in a sandbox, and how a step that needs more rights gets
    /// them here (or that every command already has them).
    fn line(self) -> String {
        use pa_types::platform::{Elevation, ElevationRoute};
        let system = match self.route {
            ElevationRoute::Uac => "Windows",
            ElevationRoute::MacosSudo => "macOS",
            ElevationRoute::UnixSudo => "Linux",
        };
        let rights = match (self.elevation, self.route) {
            (Elevation::Elevated, ElevationRoute::Uac) => HOST_ELEVATED_ADMINISTRATOR.to_string(),
            (Elevation::Elevated, ElevationRoute::MacosSudo | ElevationRoute::UnixSudo) => {
                HOST_ELEVATED_ROOT.to_string()
            }
            (Elevation::Standard, ElevationRoute::Uac) => {
                format!("{HOST_STANDARD_UAC} {HOST_FALLBACK} {HOST_DETAILS}")
            }
            (Elevation::Standard, ElevationRoute::MacosSudo | ElevationRoute::UnixSudo) => {
                format!("{HOST_STANDARD_SUDO} {HOST_FALLBACK} {HOST_DETAILS}")
            }
        };
        format!(
            "Machine: the user's own computer ({system}), not a sandbox or container: what a command does, it does to their real system. {rights}"
        )
    }
}

const HOST_DETAILS: &str = "`(api-help \"shell\")` has the details.";
const HOST_ELEVATED_ADMINISTRATOR: &str = "This session runs elevated, as Administrator: every command has administrator rights with no consent prompt, so a mistake reaches the whole machine. Use those rights only for what the task needs.";
const HOST_ELEVATED_ROOT: &str = "This session runs as root: every command has root rights with no `sudo`, so a mistake reaches the whole machine, and files it creates belong to root. Use those rights only for what the task needs.";
const HOST_STANDARD_UAC: &str = "This session runs with the user's own rights, not elevated. A step that needs administrator rights runs with `(bash command (hash 'elevated #t))`: Windows asks the user to approve it in a UAC prompt first, and its output and exit code come back as any command's.";
const HOST_STANDARD_SUDO: &str = "This session runs with the user's own rights, not as root. A step that needs root runs with `(bash command (hash 'elevated #t))`: the system asks the user for their password in a dialog of its own first (the password never passes through you), and its output and exit code come back as any command's.";
const HOST_FALLBACK: &str = "If the user declines, or no prompt can be shown here, the result says so (`elevation declined:` or `elevation unavailable:`): then give the user the exact command to run themselves.";

/// Build the system prompt (assembled text only).
#[must_use]
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    system_prompt_breakdown(options).assembled
}

/// Build the system prompt with its per-layer breakdown.
#[must_use]
pub fn system_prompt_breakdown(options: &BuildSystemPromptOptions) -> SystemPromptBreakdown {
    let mut segments: Vec<PromptSegment> = Vec::new();

    // The static prefix: the user's replacement prompt, or the layered files.
    match options.custom_prompt.as_deref() {
        Some(custom) if !custom.is_empty() => {
            segments.push(PromptSegment::static_segment(
                "custom",
                "--system-prompt",
                custom.to_string(),
            ));
        }
        _ => {
            segments.push(PromptSegment::static_segment(
                "core",
                layers::layer_source("core").unwrap_or_default(),
                layers::CORE_LAYER.trim().to_string(),
            ));
            segments.push(PromptSegment::static_segment(
                "usage",
                layers::layer_source("usage").unwrap_or_default(),
                layers::USAGE_LAYER.trim().to_string(),
            ));
            segments.push(PromptSegment::static_segment(
                "opinionated",
                layers::layer_source("opinionated").unwrap_or_default(),
                layers::OPINIONATED_LAYER.trim().to_string(),
            ));
            if let Some(extras) = options.model_prompt_extras {
                segments.push(PromptSegment::static_segment(
                    "model-prompts",
                    layers::layer_source("model-prompts").unwrap_or_default(),
                    extras.to_string(),
                ));
            }
        }
    }
    let cached_prefix_len = segments
        .iter()
        .map(|segment| segment.text.len())
        .sum::<usize>()
        + 2 * segments.len().saturating_sub(1);

    // The dynamic tail, in fixed order: project context -> project card -> skills inventory ->
    // MCP servers -> environment -> session role -> appended prompt. The Scheme kernel has no
    // packages, so the tail has no packages segment.
    let tools: Vec<&str> = options
        .selected_tools
        .clone()
        .unwrap_or_else(|| vec![pa_types::ai::SCHEME_TOOL_NAME]);
    let has_kernel = tools.contains(&pa_types::ai::KERNEL_TOOL_NAME);
    let has_file_access = has_kernel || tools.contains(&"bash");

    let context = context_files_section(&options.context_files);
    if !context.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "project-context",
            "AGENTS.md discovery",
            context,
        ));
    }

    if let Some(card) = options
        .project_card
        .as_deref()
        .filter(|card| !card.is_empty())
    {
        segments.push(PromptSegment::dynamic_segment(
            "project-card",
            "repository manifests and learned commands",
            card.to_string(),
        ));
    }

    let visible_skills: Vec<&Skill> = options
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if has_file_access && !visible_skills.is_empty() {
        let inventory = format_skills_for_prompt(&options.skills, &options.skill_standings)
            .trim()
            .to_string();
        segments.push(PromptSegment::dynamic_segment(
            "skills-inventory",
            "skill discovery",
            inventory,
        ));
    }

    if has_kernel {
        let mcp = format_generic_mcp_guidance(&options.generic_mcp_servers);
        if !mcp.is_empty() {
            segments.push(PromptSegment::dynamic_segment(
                "mcp-servers",
                "generic MCP settings",
                mcp,
            ));
        }
    }

    segments.push(PromptSegment::dynamic_segment(
        "environment",
        "session configuration",
        environment_section(options),
    ));

    let role = session_role_section(options, has_kernel);
    if !role.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "session-role",
            "RLM recursion state",
            role,
        ));
    }

    if let Some(extra) = options.append_system_prompt.as_deref() {
        if !extra.is_empty() {
            segments.push(PromptSegment::dynamic_segment(
                "appended-prompt",
                "--append-system-prompt",
                extra.to_string(),
            ));
        }
    }

    let assembled = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    SystemPromptBreakdown {
        segments,
        assembled,
        cached_prefix_len,
    }
}

fn context_files_section(context_files: &[(String, String)]) -> String {
    if context_files.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Project Context".to_string(),
        String::new(),
        "Project-specific instructions and guidelines:".to_string(),
        String::new(),
    ];
    for (file_path, content) in context_files {
        lines.push(format!("## {file_path}\n\n{content}"));
    }
    lines.join("\n")
}

fn environment_section(options: &BuildSystemPromptOptions) -> String {
    let cwd = options.cwd.replace('\\', "/");
    let messages_path = options
        .messages_path
        .clone()
        .unwrap_or_else(|| "not persisted".to_string())
        .replace('\\', "/");
    let mut lines = vec![
        format!("Current date: {}", today()),
        format!("Working directory: {cwd}"),
        format!("Conversation log: {messages_path}"),
    ];
    match options.vision_capable {
        Some(true) => lines.push(IMAGE_INPUT_SEEN.to_string()),
        Some(false) => lines.push(IMAGE_INPUT_UNSEEN.to_string()),
        None => {}
    }
    if let Some(host) = options.host {
        lines.push(host.line());
    }
    if !options.web_crawl_identity.is_empty() {
        lines.push(format!(
            "Web crawls (the user's settings, which only they change): {}.",
            options.web_crawl_identity.join(", ")
        ));
    }
    lines.join("\n")
}

/// The environment line of a session whose model accepts image input.
const IMAGE_INPUT_SEEN: &str = "Image input: this model can see images; to look at an image file (a screenshot, diagram, chart, or photo), call `(attach-image path)`: reading its bytes with `fs-read` or `bash` does not show it to you.";
/// The environment line of a session whose model is text-only.
const IMAGE_INPUT_UNSEEN: &str =
    "Image input: this model cannot see images; `attach-image` raises for it.";

/// `prompt` with its environment image-input line stating `vision_capable`:
/// a model switch changes what the session model can see, and
/// a stale line keeps a vision model from ever calling `attach-image` (or
/// sends a text-only one into its refusal). The environment section follows
/// the project context, so the last occurrence is the line; a prompt without
/// it (a custom `--system-prompt`) comes back unchanged.
#[must_use]
pub(crate) fn with_image_input(prompt: &str, vision_capable: bool) -> String {
    let (stale, current) = if vision_capable {
        (IMAGE_INPUT_UNSEEN, IMAGE_INPUT_SEEN)
    } else {
        (IMAGE_INPUT_SEEN, IMAGE_INPUT_UNSEEN)
    };
    match prompt.rfind(stale) {
        Some(at) => format!("{}{current}{}", &prompt[..at], &prompt[at + stale.len()..]),
        None => prompt.to_string(),
    }
}

fn session_role_section(options: &BuildSystemPromptOptions, has_kernel: bool) -> String {
    let depth = options.rlm_depth.unwrap_or(0);
    let mut lines = vec![format!(
        "Recursive agent depth: {depth}{}",
        if depth == 0 { " (root)" } else { " (not root)" }
    )];
    if options.daemonless {
        lines.push(if options.can_spawn {
            "This session has no daemon: heartbeats never fire, and background `bash` completions wake you with a queued `[bash-done]` turn; the run waits for live background commands and your subagents before it exits. Subagent replies and exit notices still wake you."
                .to_string()
        } else {
            "This session has no daemon: `rlm-spawn` is unavailable, heartbeats never fire, and background `bash` completions wake you with a queued `[bash-done]` turn."
                .to_string()
        });
    }
    if has_kernel {
        lines.push(
            "`(bash cmd)` waits up to 10 seconds and returns the moment the command finishes. For a command you expect to take longer (a build or test suite), ask for a longer wait up front: `(bash cmd (hash 'wait-ms 120000))` (up to 600000). If the command is still running when the wait runs out, it returns a snapshot: `running` is `#t`, `exit-code` is `#f`, `output` is the newest output, `duration-ms` is the elapsed time, and `job` is the id `bash-wait`, `bash-tail`, and `bash-kill` take. A completed result has `running` `#f` and an integer `exit-code`. The command keeps running after a snapshot; end the turn and collect the final result on the `[bash-done]` notification."
                .to_string(),
        );
        lines.push(if options.daemonless {
            "A cell still running when its `timeout` passes is interrupted: do not run long blocking work in a cell; start a long command with `(bash cmd (hash 'background #t))` and check on it in later cells."
                .to_string()
        } else {
            "A cell still running when its `timeout` passes continues in the background instead of being stopped, and keeps the engine: no other cell runs until it ends. Do not wait on it: end your turn or do work that needs no `eval`, and its `[cell-done]` notice brings the result."
                .to_string()
        });
    } else {
        lines.push(
            "This session has no Scheme kernel (`eval` tool): the programmatic tools described above are unavailable here."
                .to_string(),
        );
    }
    if options.allow_recursion == Some(false) {
        lines.push("Subagent spawning is disabled in this session.".to_string());
    }
    if depth > 0 {
        lines.push(format!(
            "You are a child agent spawned by {}. Task prompts are labeled `[task from parent]`.",
            options.rlm_parent_agent.unwrap_or("your parent agent")
        ));
        if options.factory_child {
            lines.push(
                "Your final message is this state's output, which the factory reads; do not message the parent.".to_string(),
            );
        } else if has_kernel {
            lines.push(
                "When a task calls for an answer, reply explicitly with `(agent-message-send message #:receiver-role \"parent\")`. Not every message or task needs a reply; continue cleanup after sending and go idle normally.".to_string(),
            );
        }
        if has_kernel {
            lines.push(
                "For long-running work, report brief progress with `(rlm-progress-note \"...\")` (at most 512 characters, throttled to about one note per 10 seconds); the parent sees notes without needing a reply.".to_string(),
            );
        }
    }
    lines.join("\n")
}

fn today() -> String {
    // UTC date in YYYY-MM-DD form; the prompt is date context only.
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_generic_mcp_guidance(servers: &[String]) -> String {
    let mut enabled: Vec<&String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for server in servers {
        if seen.insert(server) {
            enabled.push(server);
        }
    }
    enabled.sort();
    if enabled.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Generic MCP Connections".to_string(),
        String::new(),
        "Generic MCP connections are accessed through the `mcp-*` functions of the Scheme kernel, not as top-level native tool namespaces or installed skills.".to_string(),
        format!(
            "Enabled generic MCP servers: {}.",
            enabled
                .iter()
                .map(|server| format!("`{server}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for server in &enabled {
        lines.push(format!(
            "For `{server}`, first discover its tools with `(mcp-list-tools \"{server}\")`, then call one with `(mcp-call-tool \"{server}\" \"<tool>\" arguments)`."
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{create_synthetic_source_info, SkillKind, SourceScope};
    use std::path::PathBuf;

    fn skill(name: &str, kind: SkillKind) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("Skill {name}"),
            file_path: PathBuf::from("/skills/SKILL.md"),
            base_dir: PathBuf::from("/skills"),
            source_info: create_synthetic_source_info("/skills", "user", SourceScope::User, None),
            disable_model_invocation: false,
            kind,
        }
    }

    fn base_options() -> BuildSystemPromptOptions<'static> {
        BuildSystemPromptOptions {
            cwd: "/w".to_string(),
            messages_path: Some("/log.jsonl".to_string()),
            skills: vec![
                skill("web-search", SkillKind::Scheme),
                skill("refine", SkillKind::Scheme),
                skill("agent-message", SkillKind::Scheme),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn default_prompt_is_layered_with_static_prefix_first() {
        let breakdown = system_prompt_breakdown(&base_options());
        let prompt = &breakdown.assembled;
        assert!(prompt.starts_with("# prime-agent harness"));
        assert_eq!(breakdown.segments[0].kind, SegmentKind::Static);
        assert_eq!(breakdown.segments[0].name, "core");
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(
            names[..4],
            ["core", "usage", "opinionated", "skills-inventory"],
            "core/usage/opinionated layers, then the dynamic tail"
        );
        assert_eq!(
            &prompt[..breakdown.cached_prefix_len],
            format!(
                "{}\n\n{}\n\n{}",
                layers::CORE_LAYER.trim(),
                layers::USAGE_LAYER.trim(),
                layers::OPINIONATED_LAYER.trim()
            )
        );
        let tail = &prompt[breakdown.cached_prefix_len..];
        assert!(tail.contains("Working directory: /w"));
        assert!(tail.contains("Conversation log: /log.jsonl"));
        assert!(tail.contains("<available_skills>"));
        assert!(tail.contains("Recursive agent depth: 0 (root)"));
    }

    /// The model is told it runs on the user's machine with their rights, and how a step that
    /// needs more gets them on this platform; an elevated session is told it has them.
    #[test]
    fn the_environment_states_the_machine_and_its_rights() {
        use pa_types::platform::{Elevation, ElevationRoute};
        let environment = |route, elevation| {
            let mut options = base_options();
            options.host = Some(HostAccess { route, elevation });
            system_prompt_breakdown(&options)
                .segments
                .into_iter()
                .find(|segment| segment.name == "environment")
                .map(|segment| segment.text)
        };
        let opening = format!(
            "Current date: {}\nWorking directory: /w\nConversation log: /log.jsonl\n",
            today()
        );
        assert_eq!(
            [
                environment(ElevationRoute::Uac, Elevation::Standard),
                environment(ElevationRoute::UnixSudo, Elevation::Elevated),
            ],
            [
                Some(format!(
                    "{opening}Machine: the user's own computer (Windows), not a sandbox or container: what a command does, it does to their real system. This session runs with the user's own rights, not elevated. A step that needs administrator rights runs with `(bash command (hash 'elevated #t))`: Windows asks the user to approve it in a UAC prompt first, and its output and exit code come back as any command's. If the user declines, or no prompt can be shown here, the result says so (`elevation declined:` or `elevation unavailable:`): then give the user the exact command to run themselves. `(api-help \"shell\")` has the details."
                )),
                Some(format!(
                    "{opening}Machine: the user's own computer (Linux), not a sandbox or container: what a command does, it does to their real system. This session runs as root: every command has root rights with no `sudo`, so a mistake reaches the whole machine, and files it creates belong to root. Use those rights only for what the task needs."
                )),
            ]
        );
    }

    #[test]
    fn dynamic_tail_isolates_the_cached_prefix() {
        let mut other = base_options();
        other.cwd = "/somewhere/else".to_string();
        other.messages_path = None;
        other.rlm_depth = Some(2);
        other.rlm_parent_agent = Some("the lead");
        other.generic_mcp_servers = vec!["slack".to_string()];
        other.context_files = vec![("AGENTS.md".to_string(), "Rule one.".to_string())];
        let left = system_prompt_breakdown(&base_options());
        let right = system_prompt_breakdown(&other);
        assert_eq!(
            left.assembled[..left.cached_prefix_len],
            right.assembled[..right.cached_prefix_len]
        );
        assert_eq!(left.cached_prefix_len, right.cached_prefix_len);
        assert!(right.assembled[right.cached_prefix_len..]
            .contains("Enabled generic MCP servers: `slack`"));
        assert!(right.assembled[right.cached_prefix_len..]
            .contains("You are a child agent spawned by the lead."));
    }

    #[test]
    fn factory_child_gets_the_factory_line_instead_of_the_reply_line() {
        const REPLY: &str = "reply explicitly with `(agent-message-send message";
        const FACTORY: &str = "Your final message is this state's output";
        let mut child = base_options();
        child.rlm_depth = Some(1);
        let normal = build_system_prompt(&child);
        assert!(normal.contains(REPLY));
        assert!(!normal.contains(FACTORY));
        child.factory_child = true;
        let factory = build_system_prompt(&child);
        assert!(factory.contains(FACTORY));
        assert!(!factory.contains(REPLY));
    }

    #[test]
    fn custom_prompt_replaces_layers_keeps_tail() {
        let mut options = base_options();
        options.custom_prompt = Some("Be terse.".to_string());
        options.model_prompt_extras = Some("Tier-2 guidance.");
        options.context_files = vec![("AGENTS.md".to_string(), "Rule one.".to_string())];
        let breakdown = system_prompt_breakdown(&options);
        let prompt = &breakdown.assembled;
        assert!(prompt.starts_with("Be terse."));
        assert_eq!(breakdown.segments[0].name, "custom");
        assert!(prompt.contains("## AGENTS.md\n\nRule one."));
        assert!(prompt.contains("Working directory: /w"));
        assert!(prompt.contains("Current date: "));
        assert!(!prompt.contains("# prime-agent harness"));
        assert!(!prompt.contains("Tier-2 guidance."));
    }

    #[test]
    fn mcp_servers_are_a_tail_segment() {
        let mut options = base_options();
        options.generic_mcp_servers = vec!["t".to_string(), "t".to_string()];
        let breakdown = system_prompt_breakdown(&options);
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert!(names.contains(&"mcp-servers"));
        assert!(breakdown.assembled.contains("# Generic MCP Connections"));
        assert!(breakdown
            .assembled
            .contains("Enabled generic MCP servers: `t`."));
    }

    #[test]
    fn daemonless_line_drops_the_stale_no_wake_claim() {
        // The line's one durable contract: it no longer tells daemonless
        // agents that background completions never wake them.
        let mut spawning = base_options();
        spawning.daemonless = true;
        spawning.can_spawn = true;
        assert!(!build_system_prompt(&spawning).contains("completions do not wake you"));

        let mut bare = base_options();
        bare.daemonless = true;
        assert!(!build_system_prompt(&bare).contains("completions do not wake you"));
    }

    #[test]
    fn model_prompt_extras_form_the_last_static_segment() {
        let mut options = base_options();
        options.model_prompt_extras = Some("Tier-2 guidance.");
        let breakdown = system_prompt_breakdown(&options);
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(
            names[..5],
            [
                "core",
                "usage",
                "opinionated",
                "model-prompts",
                "skills-inventory"
            ],
            "extras are the last static segment, directly before the dynamic tail"
        );
    }

    /// The environment names what the user's `web.crawl` settings change in
    /// a crawl, and says nothing while they change nothing.
    #[test]
    fn the_environment_names_what_the_users_crawl_settings_change() {
        let line = |settings: &crate::settings::WebCrawlSettings| {
            let mut options = base_options();
            options.web_crawl_identity = crate::web_crawl::identity_changes(settings);
            system_prompt_breakdown(&options)
                .segments
                .into_iter()
                .find(|segment| segment.name == "environment")
                .and_then(|segment| {
                    segment
                        .text
                        .lines()
                        .find(|line| line.starts_with("Web crawls"))
                        .map(str::to_string)
                })
        };
        assert_eq!(
            [
                line(&crate::settings::WebCrawlSettings::default()),
                line(&crate::settings::WebCrawlSettings {
                    respect_robots_txt: Some(false),
                    proxies: Some(vec!["socks5://127.0.0.1:9050".to_string()]),
                    stealth: Some(true),
                    ..Default::default()
                }),
            ],
            [
                None,
                Some(
                    "Web crawls (the user's settings, which only they change): robots.txt ignored, proxies, stealth."
                        .to_string()
                ),
            ]
        );
    }
}
