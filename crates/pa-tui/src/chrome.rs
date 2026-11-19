//! Chat chrome: the pinned top bar, brand splash header, prompt context
//! line, and the tray line under the editor.

use crate::width::{line_width, str_width, truncate_line};
use crate::{Line, Span};
use ratatui::style::Style;
use serde_json::Value;

use crate::theme::{Theme, ThemeBg, ThemeColor};

/// The compact 7-row brand butterfly.
pub const PRIME_COMPACT_BUTTERFLY_LOGO: &str = concat!(
    "                 \u{2597}\u{2584}\u{2584}\u{2588}\u{2580}\n",
    "   \u{2588}\u{2588}\u{2588}\u{2584}       \u{2597}\u{2584}\u{2588}\u{2588}\u{2588}\u{2580}\n",
    "  \u{2597}\u{2588}\u{259b}\u{2590}\u{2588}\u{2599}   \u{2597}\u{2584}\u{2588}\u{2580}\u{2597}\u{2588}\u{2580}\n",
    " \u{2597}\u{2588}\u{259b} \u{259f}\u{2588}\u{2588}\u{2599}\u{2584}\u{2588}\u{2588}\u{259b} \u{259f}\u{259b}\n",
    " \u{2597}\u{259f}\u{258c} \u{2590}\u{2588}\u{2588}\u{2588}\u{259b}\u{2598}\u{2597}\u{2584}\u{2588}\u{2596}\n",
    "\u{259f}\u{2588}\u{2588}\u{2588}\u{2584}  \u{2584}\u{2584}\u{259f}\u{2588}\u{2588}\u{2588}\u{2580}\n",
    "\u{259c}\u{2588}\u{259b}\u{2580}\u{2598}  \u{259c}\u{2588}\u{259b}\u{2580}\u{2598}",
);

/// Truncate a plain string to a visible width (cut on grapheme
/// boundaries, appending the ellipsis).
fn truncate_to_width(value: &str, max_width: usize, ellipsis: &str) -> String {
    if str_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in value.chars() {
        if str_width(&out) + crate::width::char_width(ch) > max_width {
            break;
        }
        out.push(ch);
    }
    format!("{out}{ellipsis}")
}

/// Where the active model runs, read from its configuration alone (no network): the tray marks
/// a local or private endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelHost {
    #[default]
    Remote,
    /// A known local provider, or a base URL on this machine or a private network.
    Local,
}

/// Providers that only ever serve from the user's own machine.
const LOCAL_PROVIDERS: &[&str] = &[
    "ollama",
    "lmstudio",
    "lm-studio",
    "llama.cpp",
    "llamacpp",
    "localai",
];

impl ModelHost {
    /// The host of one model: a known local provider, or a base URL whose host is `localhost`
    /// (or a `.localhost`/`.local` name), a loopback, private, link-local or unspecified IPv4
    /// address, or a loopback, unique-local or link-local IPv6 address.
    #[must_use]
    pub fn of(provider: &str, base_url: &str) -> Self {
        let local_provider = LOCAL_PROVIDERS
            .iter()
            .any(|known| provider.eq_ignore_ascii_case(known));
        let local_url = url::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host().map(|host| host.to_owned()))
            .is_some_and(|host| match host {
                url::Host::Domain(name) => {
                    let name = name.to_ascii_lowercase();
                    let last_label = name.rsplit('.').next().unwrap_or_default();
                    name == "localhost"
                        || (name.contains('.') && matches!(last_label, "localhost" | "local"))
                }
                url::Host::Ipv4(ip) => {
                    ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
                }
                url::Host::Ipv6(ip) => {
                    let first = ip.segments()[0];
                    ip.is_loopback()
                        || ip.is_unspecified()
                        || first & 0xfe00 == 0xfc00
                        || first & 0xffc0 == 0xfe80
                }
            });
        if local_provider || local_url {
            ModelHost::Local
        } else {
            ModelHost::Remote
        }
    }
}

/// Where a session runs; drives labels that depend on persistence.
#[derive(Debug, Clone, Default)]
pub struct ChromeState {
    /// Product version shown in the splash (`prime agent vX`).
    pub version: String,
    /// Session working directory (splash `cwd` line; `~`-compressed).
    pub cwd: String,
    /// Current model id (splash `model` line; `None` hides the line).
    pub model_id: Option<String>,
    /// The current model's provider: two providers can carry the same id,
    /// so only the provider disambiguates the picker's match.
    pub model_provider: Option<String>,
    /// Where the current model runs: a local one shows `⌂` and the success tone in the tray.
    pub model_host: ModelHost,
    /// Extra metadata lines under the splash (`label value` each); empty
    /// renders none.
    pub extra_metadata: Vec<(String, String)>,
    /// Top-bar chat name (session name or the cwd basename).
    pub chat_name: String,
    /// Session spend (USD) beside the chat name: the family rollup (the
    /// session's spend plus every subagent descendant's).
    pub cost_usd: Option<f64>,
    /// Context usage: tokens, window, percent (tray right label).
    pub context: Option<ContextUsage>,
    /// The auto-compaction threshold (percent of the window) the gauge marks with a `┃` tick;
    /// `None` (auto-compaction off) draws no tick.
    pub compaction_percent: Option<f64>,
    /// `← manage` hint: shown for persisted (attachable) sessions.
    pub show_manage: bool,
    /// The attached session's RLM depth: a subagent session renders
    /// `depth N` after the manage hint, a root none.
    pub tray_depth: Option<u32>,
    /// Thinking effort suffix rendered as `model:effort` in the tray;
    /// `None` keeps the bare model id.
    pub thinking_suffix: Option<String>,
    /// The session's service tier as its wire name: `fast` for priority, the
    /// tier name for any other non-default tier; `None` or `default` renders no badge.
    pub service_tier: Option<String>,
    /// Startup warning (tmux keyboard setup), rendered as a status row.
    pub tmux_notice: Option<String>,
    /// Tray override label: while the Ctrl+C exit hint is armed, it
    /// replaces the tray's location label.
    pub tray_override: Option<String>,
    /// The compact, borderless activity dock under the editor: a live
    /// session always mounts it; `None` means no session owns this view.
    pub activity: Option<ActivityDock>,
    /// The rate-limit reset chip (`limit resets in 1h 2m`): set while the session's provider
    /// last answered with a rate limit whose reset is still ahead; `None` renders none.
    pub reset_chip: Option<String>,
    /// The footer's tok/sec readout: the dim bottom row's text; `None`
    /// renders no row (kept until the first completed response).
    pub speed_text: Option<String>,
    /// Hide the splash `cwd` line (the scoped agents view keeps its
    /// metadata rows centered against the logo).
    pub splash_hide_cwd: bool,
}

/// Which actionable group owns the dock's selection. Every group is
/// arrow-traversable whether or not it has rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityGroup {
    #[default]
    Subagents,
    Heartbeats,
    Bash,
    /// The factory page: live factory runs. Its group renders exactly
    /// while the daemon advertises the `factory_activity` lane (the
    /// opt-in gate; an advertised empty one reads its zero count) and
    /// opens the factory page over the lane — the same navigation family
    /// as the subagents, heartbeats, and shells pages.
    Factory,
    /// The active goal: its group is mounted while a goal is being
    /// pursued and opens the read-only goal panel (the objective and
    /// its facts); a goal that ended unmounts the row with it.
    Goal,
}

/// Which way an Up/Down press steps along the dock's rendered groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityDirection {
    /// The previous group, wrapping past the first.
    Prev,
    /// The next group, wrapping past the last.
    Next,
}

/// The bottom activity dock: it renders in every session, all-zero
/// included (TS `SubagentSummaryLine` renders nothing at zero — a
/// sanctioned divergence). Unfocused it is one line of group counts;
/// focused it expands into one row per group with each group's details
/// (the operator's 2026-10-16 spec).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityDock {
    /// The directly-running children right now (one addend of the dock's
    /// single running total).
    pub subagents_running_direct: usize,
    /// The further running descendants below them: the total's other
    /// addend. Idle and dead rows never count.
    pub subagents_running_nested: usize,
    /// The idle descendants (the expanded subagents row's breakdown).
    pub subagents_idle: usize,
    /// The inactive descendants (the expanded subagents row's breakdown).
    pub subagents_inactive: usize,
    /// The current session's heartbeats (nested sessions' jobs never
    /// surface here).
    pub heartbeats: usize,
    /// The current session's active heartbeats, soonest next fire first,
    /// each `<countdown> <label>` (the expanded row).
    pub heartbeats_active: Vec<String>,
    /// How many of the scoped heartbeats are paused.
    pub heartbeats_paused: usize,
    /// The bash processes actively running right now, one item each (the
    /// current session's kernel registry only): finished runs stay rows
    /// inside the bash view.
    pub shells: Vec<String>,
    /// How many of the running shells are persistent.
    pub shells_persistent: usize,
    /// The current session's stored factory specs (the expanded factory
    /// row's breakdown; the collapsed segment counts live runs only).
    pub factory_specs: usize,
    /// The live factory runs, paused first, each `<name> <state>` (a live
    /// state — running, stopping, or paused — or a terminal run whose
    /// children are still in flight; the current session's kernel registry
    /// only): fully terminal runs never inflate the group, exactly like
    /// the shells group's running-only scoping.
    pub factory_runs: Vec<String>,
    /// The same live runs counted running vs paused (the collapsed count
    /// and its paused cluster).
    pub factory_counts: crate::factory_view::FactoryRunCounts,
    /// Whether the factory group renders at all: the daemon's
    /// `factory_activity` advertisement (the factory's opt-in gate —
    /// `factory.enabled`, off by default). A daemon without the lane
    /// mounts no factory group anywhere: no row, no traversal, no
    /// click, no page.
    pub factory_group: bool,
    /// The active goal's dock label — `Pursuing goal (12m 05s)`-style,
    /// the elapsed-time form (the operator's 2026-09-24 directive: the
    /// row reads the time, the token budget lives inside the goal
    /// panel); `None` unless the goal is actively being pursued (a
    /// completed or idle goal carries no dock segment).
    pub goal_label: Option<String>,
    pub selected: ActivityGroup,
    pub focused: bool,
}

impl ActivityDock {
    /// The groups this dock renders — left to right collapsed, top to
    /// bottom expanded — and the Up/Down traversal order. The subagents,
    /// heartbeats, and shells groups always render (an empty one reads its
    /// zero count or its dimmed row and stays traversable); the factory
    /// group renders exactly while the daemon advertises the
    /// `factory_activity` lane (the opt-in gate), and the goal group
    /// exactly while a live goal keeps its row mounted.
    #[must_use]
    pub fn groups(&self) -> Vec<ActivityGroup> {
        let mut groups = vec![
            ActivityGroup::Subagents,
            ActivityGroup::Heartbeats,
            ActivityGroup::Bash,
        ];
        if self.factory_group {
            groups.push(ActivityGroup::Factory);
        }
        if self.goal_label.is_some() {
            groups.push(ActivityGroup::Goal);
        }
        groups
    }

    /// One step: the neighbor in `direction`, wrapping at the ends; a
    /// `current` that no longer renders steps from the first group.
    #[must_use]
    pub fn step(&self, current: ActivityGroup, direction: ActivityDirection) -> ActivityGroup {
        let groups = self.groups();
        let len = groups.len();
        let position = groups
            .iter()
            .position(|group| *group == current)
            .unwrap_or(0);
        let neighbor = match direction {
            ActivityDirection::Prev => position + len - 1,
            ActivityDirection::Next => position + 1,
        };
        groups[neighbor % len]
    }
}

/// Context usage for the tray label (`N/W P%`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub context_window: u64,
    pub reading: ContextReading,
}

/// How current the tray's context usage is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextReading {
    /// The daemon measured `tokens` for the context as it stands.
    #[default]
    Measured,
    /// The last measured `tokens`, carried over while the daemon cannot measure the context
    /// (right after a compaction, until the next reply) — the tray marks it `~`.
    Stale,
    /// Nothing measured yet (before the first reply): `tokens` is meaningless; the tray shows
    /// `?/W`.
    Unknown,
}

impl ContextUsage {
    /// The wire shape (TS `SessionStats["contextUsage"]`, served by `get_session_stats` and
    /// carried by the attach snapshot's state). Unknown or zero tokens with a known window — a
    /// session right after a compaction, or before its first reply — read as
    /// [`ContextReading::Unknown`]; only a missing window yields `None`.
    #[must_use]
    pub fn from_wire(usage: &Value) -> Option<Self> {
        let context_window = usage.get("contextWindow").and_then(Value::as_u64)?;
        let tokens = usage
            .get("tokens")
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0);
        Some(Self {
            tokens: tokens.unwrap_or_default(),
            context_window,
            reading: match tokens {
                Some(_) => ContextReading::Measured,
                None => ContextReading::Unknown,
            },
        })
    }

    /// Fold a fresh reading over the last one shown for the SAME session: an unknown reading,
    /// or a reply without the field at all (a worker that restarted before its model resolved),
    /// keeps the last known tokens marked stale instead of blanking the tray.
    #[must_use]
    pub fn settle(next: Option<Self>, previous: Option<Self>) -> Option<Self> {
        let known = previous.filter(|previous| previous.reading != ContextReading::Unknown);
        match (next, known) {
            (Some(next), Some(previous)) if next.reading == ContextReading::Unknown => Some(Self {
                tokens: previous.tokens,
                context_window: next.context_window,
                reading: ContextReading::Stale,
            }),
            (None, Some(previous)) => Some(Self {
                reading: ContextReading::Stale,
                ..previous
            }),
            (Some(next), Some(_) | None) => Some(next),
            (None, None) => previous,
        }
    }

    #[must_use]
    pub fn percent(&self) -> f64 {
        if self.context_window == 0 {
            0.0
        } else {
            (self.tokens as f64 / self.context_window as f64) * 100.0
        }
    }
}

/// The session's own accent: the theme accent's lightness (saturation at least 55%) at one of
/// five hues picked by the session's name (stable per name), all within 150°-294° so it never
/// reads as the gauge's amber or red warnings. A theme without a true-color accent keeps its
/// accent.
#[must_use]
pub fn session_accent(name: &str, theme: &Theme) -> Style {
    let accent = theme.fg_style(ThemeColor::Accent);
    let (Some(ratatui::style::Color::Rgb(red, green, blue)), false) = (accent.fg, name.is_empty())
    else {
        return accent;
    };
    // The theme accent's lightness and saturation (HSL).
    let [red, green, blue] = [red, green, blue].map(|channel| f64::from(channel) / 255.0);
    let (max, min) = (red.max(green).max(blue), red.min(green).min(blue));
    let lightness = f64::midpoint(max, min);
    let spread = 1.0 - (2.0 * lightness - 1.0).abs();
    // A muted accent still yields hues that read as distinct colors.
    let saturation = if spread < f64::EPSILON {
        0.0
    } else {
        ((max - min) / spread).max(0.55)
    };
    // FNV-1a over the name picks one of five hues 36° apart (green-teal, cyan, sky blue,
    // violet, purple), so two sessions either match or clearly differ.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    let hue = 150.0 + 36.0 * f64::from(hash % 5);
    let chroma = spread * saturation;
    let second = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let sector = (hue / 60.0).floor();
    let (red, green, blue) = if sector < 3.0 {
        (0.0, chroma, second)
    } else if sector < 4.0 {
        (0.0, second, chroma)
    } else if sector < 5.0 {
        (second, 0.0, chroma)
    } else {
        (chroma, 0.0, second)
    };
    let offset = lightness - chroma / 2.0;
    let channel = |value: f64| {
        let scaled = ((value + offset) * 255.0).round().clamp(0.0, 255.0);
        // Clamped to 0..=255 above, so the cast is exact.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=255"
        )]
        let byte = scaled as u8;
        byte
    };
    accent.fg(ratatui::style::Color::Rgb(
        channel(red),
        channel(green),
        channel(blue),
    ))
}

#[must_use]
pub fn format_token_count(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1_000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1_000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// The top-bar chat name for an unnamed session: the cwd basename.
#[must_use]
pub fn display_name(cwd: &str) -> String {
    std::path::Path::new(cwd).file_name().map_or_else(
        || cwd.to_string(),
        |name| name.to_string_lossy().to_string(),
    )
}

/// The `~`-compressed cwd for the splash line.
#[must_use]
pub fn format_splash_cwd(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.replace('\\', "/");
    };
    let home = home.replace('\\', "/");
    let normalized = cwd.replace('\\', "/");
    if home.is_empty() {
        return normalized;
    }
    if normalized == home {
        return "~".to_string();
    }
    if let Some(rest) = normalized.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    normalized
}

/// Middle-truncate a path: keep the last two segments (`~/…/parent/leaf`).
pub fn truncate_path_middle(value: &str, width: usize) -> String {
    if str_width(value) <= width {
        return value.to_string();
    }
    if width <= 1 {
        return truncate_to_width(value, width, "");
    }
    let normalized = value.replace('\\', "/");
    let prefix = if normalized.starts_with("~/") {
        "~/"
    } else if normalized.starts_with('/') {
        "/"
    } else {
        ""
    };
    let body = normalized[prefix.len()..].to_string();
    let mut parts: Vec<&str> = body.split('/').filter(|part| !part.is_empty()).collect();
    let last = parts.pop().unwrap_or_default().to_string();
    let previous = parts.pop().map(str::to_string);
    let suffix = previous
        .map(|previous| format!("{previous}/{last}"))
        .unwrap_or(last);
    let candidate = format!("{prefix}\u{2026}/{suffix}");
    if str_width(&candidate) <= width {
        return candidate;
    }
    truncate_to_width(&candidate, width, "…")
}

/// The pinned top bar: chat name centered, spend beside it.
pub fn render_top_bar(state: &ChromeState, theme: &Theme, width: usize) -> Line {
    let name = state
        .chat_name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace(char::is_whitespace, " ")
        .trim()
        .to_string();
    if name.is_empty() {
        return Vec::new();
    }
    let mut line: Line = Vec::new();
    let name_width = str_width(&name);
    let text = theme.fg_style(ThemeColor::Text);
    let dim = theme.fg_style(ThemeColor::Dim);
    let start = (width.saturating_sub(name_width)) / 2;
    line.push(Span::styled(" ".repeat(start), Style::default()));
    line.push(Span::styled(name, text));
    if let Some(cost) = state.cost_usd.filter(|cost| *cost >= 0.0) {
        line.push(Span::styled("  ".to_string(), Style::default()));
        line.push(Span::styled(format!("${cost:.2}"), dim));
    }
    line
}

/// The brand splash: butterfly logo beside the version/model/cwd metadata
/// (TS `BrandSplashHeader`, `topPadding` always on).
#[must_use]
pub fn render_splash(state: &ChromeState, theme: &Theme, width: usize) -> Vec<Line> {
    let safe_width = width.max(1);
    let padding_x = usize::from(safe_width > 1);
    let content_width = safe_width.saturating_sub(padding_x * 2).max(1);
    let logo_raw: Vec<&str> = PRIME_COMPACT_BUTTERFLY_LOGO.split('\n').collect();
    let logo_canvas_width = logo_raw
        .iter()
        .map(|line| str_width(line))
        .max()
        .unwrap_or(0);
    let gutter = 3usize;
    let show_logo =
        logo_canvas_width > 0 && content_width.saturating_sub(logo_canvas_width + gutter) >= 24;
    let meta_width = if show_logo {
        content_width.saturating_sub(logo_canvas_width + gutter)
    } else {
        content_width
    };

    let text = theme.fg_style(ThemeColor::Text);
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let title = "prime agent";
    let version = format!("v{}", state.version);
    let mut meta_lines: Vec<Line> = Vec::new();
    if str_width(&format!("{title} {version}")) <= meta_width {
        meta_lines.push(vec![
            Span::styled(title.to_string(), text),
            Span::styled(" ".to_string(), Style::default()),
            Span::styled(version, muted),
        ]);
    } else {
        meta_lines.push(vec![Span::styled(title.to_string(), text)]);
        meta_lines.push(vec![Span::styled(version, muted)]);
    }
    for (label_text, value_text) in &state.extra_metadata {
        let label = format!("{label_text} ");
        let value = truncate_to_width(
            value_text,
            meta_width.saturating_sub(str_width(&label)).max(1),
            "",
        );
        meta_lines.push(vec![Span::styled(label, dim), Span::styled(value, muted)]);
    }
    if let Some(model_id) = &state.model_id {
        let label = "model ";
        let value = truncate_to_width(
            model_id,
            meta_width.saturating_sub(str_width(label)).max(1),
            "",
        );
        meta_lines.push(vec![
            Span::styled(label.to_string(), dim),
            Span::styled(value, muted),
        ]);
    }
    if !state.splash_hide_cwd {
        let cwd_label = "cwd ";
        let home = pa_types::platform::home_dir().map(|home| home.to_string_lossy().into_owned());
        let cwd = truncate_path_middle(
            &format_splash_cwd(&state.cwd, home.as_deref()),
            meta_width.saturating_sub(str_width(cwd_label)).max(1),
        );
        meta_lines.push(vec![
            Span::styled(cwd_label.to_string(), dim),
            Span::styled(cwd, muted),
        ]);
    }

    let mut lines: Vec<Line> = vec![Vec::new()];
    let row_count = logo_raw.len().max(meta_lines.len());
    let meta_start = if show_logo {
        (row_count - meta_lines.len()) / 2
    } else {
        0
    };
    let logo_text = theme.fg_style(ThemeColor::Text);
    for index in 0..row_count {
        let mut spans: Line = Vec::new();
        let pad = if padding_x > 0 { " " } else { "" };
        spans.push(Span::styled(pad.to_string(), Style::default()));
        if show_logo {
            let logo_line = logo_raw.get(index).copied().unwrap_or("");
            spans.push(Span::styled(logo_line.to_string(), logo_text));
            let fill = logo_canvas_width.saturating_sub(str_width(logo_line)) + gutter;
            spans.push(Span::styled(" ".repeat(fill), Style::default()));
        }
        let meta_index = index as isize - meta_start as isize;
        if meta_index >= 0 {
            if let Some(meta_line) = meta_lines.get(meta_index as usize) {
                spans.extend(meta_line.iter().cloned());
            }
        }
        let used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
        spans.push(Span::styled(
            " ".repeat(safe_width.saturating_sub(used + padding_x)),
            Style::default(),
        ));
        lines.push(spans);
    }
    lines.push(Vec::new());
    lines
}

/// The plain row above the prompt: the detail status right (TS
/// `PromptContextLine`, always `["", row]`).
#[must_use]
pub fn render_prompt_context(detail_label: &str, theme: &Theme, width: usize) -> Vec<Line> {
    if width < 1 {
        return Vec::new();
    }
    let padding_x = usize::from(width > 2);
    let content_width = width.saturating_sub(padding_x * 2);
    let dim = theme.fg_style(ThemeColor::Dim);
    let label_width = str_width(detail_label);
    let label = if label_width > content_width {
        truncate_to_width(detail_label, content_width, "")
    } else {
        detail_label.to_string()
    };
    let space = content_width.saturating_sub(str_width(&label));
    let row = vec![
        Span::raw(" ".repeat(padding_x)),
        Span::raw(" ".repeat(space)),
        Span::styled(label, dim),
        Span::raw(" ".repeat(padding_x)),
    ];
    vec![Vec::new(), row]
}

/// The conversation-detail status label: "Expanded" (all output), "Details"
/// (thinking + diffs), or "Collapsed"; only Expanded flips the key hint to "collapse".
#[must_use]
pub fn conversation_detail_status(all_output: bool, details: bool, key_display: &str) -> String {
    let label = if all_output {
        "Expanded"
    } else if details {
        "Details"
    } else {
        "Collapsed"
    };
    let action = if all_output { "collapse" } else { "expand" };
    format!("{label} mode ({key_display} to {action})")
}

/// The tray row under the editor: location label left, context label
/// right, over the full width.
#[must_use]
pub fn render_tray(state: &ChromeState, theme: &Theme, width: usize) -> Line {
    render_tray_with_hint(state, theme, width).line
}

/// The tray's shrink order as the width drops. Every step keeps all the previous steps'
/// cuts and gives up one more segment, whole: the gauge (the gap between the lanes) shrinks
/// first and vanishes below six cells, then the tier badge, the subagent depth, the context
/// label's `tokens/window` (the percent stays), the model's effort suffix (with its thinking
/// glyph), the `manage` word (the `←` stays), the model, the context percent, and the rate-limit
/// reset chip last. The context percent outlives the model: a crowded tray (a long model id on a
/// narrow pane) must never lose the indicator of how full the context is. The chip outlives the
/// percent: while the provider refuses requests, when it lets them through again is the tray's
/// one actionable fact. Nothing is cut mid-word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TrayShrink {
    Full,
    NoTier,
    NoDepth,
    PercentOnly,
    NoEffort,
    BareManage,
    NoModel,
    NoContext,
    NoResetChip,
}

const TRAY_SHRINK: [TrayShrink; 9] = [
    TrayShrink::Full,
    TrayShrink::NoTier,
    TrayShrink::NoDepth,
    TrayShrink::PercentOnly,
    TrayShrink::NoEffort,
    TrayShrink::BareManage,
    TrayShrink::NoModel,
    TrayShrink::NoContext,
    TrayShrink::NoResetChip,
];

/// The tray row with its two click regions' column spans (start inclusive, end exclusive).
#[derive(Debug, Clone, PartialEq)]
pub struct TrayRow {
    pub line: Line,
    /// The `← manage` hint's cells: `None` when the left label is the override or the hint is
    /// hidden.
    pub manage: Option<std::ops::Range<usize>>,
    /// The context gauge and its `tokens/window %` label (oh-my-pi's clickable context meter):
    /// `None` while the session reports no context usage or the label shrank away.
    pub context: Option<std::ops::Range<usize>>,
}

/// The tray's lanes at one shrink step, with the click spans inside them.
struct TrayLanes {
    left: Line,
    right: Line,
    /// The manage hint's cells within the left lane.
    hint: Option<std::ops::Range<usize>>,
    /// The context label's width at the right lane's start (0 when it is not shown).
    context_label: usize,
}

/// The tray row plus its click regions: a plain click on the hint's cells performs the hinted
/// action, one on the gauge or the context label opens the context window. The row degrades in
/// the [`TrayShrink`] order until both lanes fit with a cell between them.
#[must_use]
pub fn render_tray_with_hint(state: &ChromeState, theme: &Theme, width: usize) -> TrayRow {
    let lanes = |shrink: TrayShrink| tray_lanes(state, theme, shrink);
    let fits = |lanes: &TrayLanes| {
        let (left, right) = (line_width(&lanes.left), line_width(&lanes.right));
        let between = usize::from(left > 0 && right > 0);
        left + right + between <= width
    };
    let TrayLanes {
        left,
        right,
        hint,
        context_label,
    } = TRAY_SHRINK
        .iter()
        .map(|shrink| lanes(*shrink))
        .find(fits)
        .unwrap_or_else(|| lanes(TrayShrink::NoResetChip));
    let left_width = line_width(&left);
    let right_width = line_width(&right);
    let gap = width.saturating_sub(left_width + right_width);
    let mut line: Line = Vec::new();
    line.extend(left);
    // The gap between the lanes is the context gauge: `━` for the used share, `─` for the
    // rest, amber from 70% and red from 90% (a space pads each end), with a `┃` tick at the
    // auto-compaction threshold.
    let gauge = state.context.as_ref().filter(|_| gap >= 6);
    if let Some(context) = gauge {
        let (cells, percent) = (gap - 2, context.percent());
        let used = ((cells as f64 * percent / 100.0).round() as usize).min(cells);
        let tone = if percent >= 90.0 {
            theme.fg_style(ThemeColor::Error)
        } else if percent >= 70.0 {
            theme.fg_style(ThemeColor::Warning)
        } else {
            session_accent(&state.chat_name, theme)
        };
        let free = theme.fg_style(ThemeColor::BorderMuted);
        let tick = state
            .compaction_percent
            .map(|threshold| ((cells as f64 * threshold / 100.0).round() as usize).min(cells - 1));
        let run = |from: usize, to: usize| -> Vec<Span> {
            let used_cells = used.clamp(from, to) - from;
            vec![
                Span::styled("\u{2501}".repeat(used_cells), tone),
                Span::styled("\u{2500}".repeat(to - from - used_cells), free),
            ]
        };
        line.push(Span::styled(" ".to_string(), Style::default()));
        match tick {
            Some(at) => {
                line.extend(run(0, at));
                line.push(Span::styled(
                    "\u{2503}".to_string(),
                    theme.fg_style(ThemeColor::Muted),
                ));
                line.extend(run(at + 1, cells));
            }
            None => line.extend(run(0, cells)),
        }
        line.push(Span::styled(" ".to_string(), Style::default()));
    } else {
        line.push(Span::styled(" ".repeat(gap), Style::default()));
    }
    line.extend(right);
    // The context region spans the drawn gauge (its padding cells included) and the label.
    let label_start = left_width + gap;
    let context = (context_label > 0).then(|| {
        let start = if gauge.is_some() {
            left_width
        } else {
            label_start
        };
        start.min(width)..(label_start + context_label).min(width)
    });
    TrayRow {
        // Only an override label wider than the row itself can still overflow: clip it.
        line: truncate_line(&line, width, ""),
        manage: hint,
        context: context.filter(|span| !span.is_empty()),
    }
}

/// The tray's two lanes at one shrink step: the location label (override, or the manage hint and
/// depth) on the left with the hint's own cells, the context label, model, and tier on the right.
fn tray_lanes(state: &ChromeState, theme: &Theme, shrink: TrayShrink) -> TrayLanes {
    let dim = theme.fg_style(ThemeColor::Dim);
    let muted = theme.fg_style(ThemeColor::Muted);
    let mut hint: Option<std::ops::Range<usize>> = None;
    let mut left: Line = Vec::new();
    if let Some(override_label) = &state.tray_override {
        // The override label replaces the location label on the left
        // while it is set.
        left.push(Span::styled(override_label.clone(), muted));
    } else if state.show_manage {
        left.push(Span::styled("\u{2190}".to_string(), dim));
        if shrink < TrayShrink::BareManage {
            left.push(Span::styled(" manage".to_string(), muted));
        }
        // The hint's own cells are the clickable region; the depth label
        // that may follow is metadata, not part of the hinted action.
        hint = Some(0..line_width(&left));
        // A subagent session joins its `depth N` label onto the manage
        // hint; a root session renders none.
        if let Some(depth) = state
            .tray_depth
            .filter(|depth| *depth > 0 && shrink < TrayShrink::NoDepth)
        {
            left.push(Span::styled("  ".to_string(), muted));
            left.push(Span::styled(format!("depth {depth}"), muted));
        }
    }
    // The reset chip rides the location lane in the session accent (the gauge's own tone).
    if let Some(chip) = state
        .reset_chip
        .as_ref()
        .filter(|_| state.tray_override.is_none() && shrink < TrayShrink::NoResetChip)
    {
        if !left.is_empty() {
            left.push(Span::styled("  ".to_string(), muted));
        }
        left.push(Span::styled(
            chip.clone(),
            session_accent(&state.chat_name, theme),
        ));
    }
    let mut right: Line = Vec::new();
    let separator = || Span::styled(" \u{00b7} ".to_string(), dim);
    // The context label leads the right lane so it sits against its gauge.
    if let Some(context) = state
        .context
        .as_ref()
        .filter(|_| shrink < TrayShrink::NoContext)
    {
        let window = format_token_count(context.context_window);
        let full = shrink < TrayShrink::PercentOnly;
        match context.reading {
            ContextReading::Unknown => {
                let label = if full {
                    format!("?/{window}")
                } else {
                    "?".to_string()
                };
                right.push(Span::styled(label, dim));
            }
            ContextReading::Measured | ContextReading::Stale => {
                let stale = if context.reading == ContextReading::Stale {
                    "~"
                } else {
                    ""
                };
                if full {
                    right.push(Span::styled(
                        format!("{stale}{}/{window} ", format_token_count(context.tokens)),
                        dim,
                    ));
                } else {
                    right.push(Span::styled(stale.to_string(), dim));
                }
                // Past the window the percent reads `>100%` in the warning tone.
                if context.tokens > context.context_window {
                    right.push(Span::styled(
                        ">100%".to_string(),
                        theme.fg_style(ThemeColor::Warning),
                    ));
                } else {
                    right.push(Span::styled(format!("{:.0}%", context.percent()), dim));
                }
            }
        }
    }
    let context_label = line_width(&right);
    if let Some(model) = state
        .model_id
        .as_ref()
        .filter(|_| shrink < TrayShrink::NoModel)
    {
        let mut label = model.clone();
        if let Some(suffix) = state
            .thinking_suffix
            .as_ref()
            .filter(|_| shrink < TrayShrink::NoEffort)
        {
            label.push(':');
            label.push_str(suffix);
        }
        if !right.is_empty() {
            right.push(separator());
        }
        // The thinking level as a circle that fills with effort, in the level's theme color. It
        // is the effort segment's glyph, so it leaves with the effort suffix.
        let level = state
            .thinking_suffix
            .as_deref()
            .filter(|_| shrink < TrayShrink::NoEffort)
            .and_then(pa_types::ai::thinking_level_from_str);
        if let Some(level) = level {
            use pa_types::ai::ModelThinkingLevel as L;
            let (glyph, color) = match level {
                L::Off => ("\u{25cb}", ThemeColor::ThinkingOff),
                L::Minimal => ("\u{25d4}", ThemeColor::ThinkingMinimal),
                L::Low => ("\u{25d1}", ThemeColor::ThinkingLow),
                L::Medium => ("\u{25d5}", ThemeColor::ThinkingMedium),
                L::High => ("\u{25cf}", ThemeColor::ThinkingHigh),
                L::Xhigh | L::Max => ("\u{25c9}", ThemeColor::ThinkingXhigh),
            };
            right.push(Span::styled(format!("{glyph} "), theme.fg_style(color)));
        }
        match state.model_host {
            ModelHost::Local => {
                let success = theme.fg_style(ThemeColor::Success);
                right.push(Span::styled("\u{2302} ".to_string(), success));
                right.push(Span::styled(label, success));
            }
            ModelHost::Remote => right.push(Span::styled(label, dim)),
        }
    }
    let tier_badge = match state.service_tier.as_deref() {
        Some("priority") => Some("fast"),
        Some(tier) if tier != "default" => Some(tier),
        _ => None,
    };
    if let Some(badge) = tier_badge.filter(|_| shrink < TrayShrink::NoTier) {
        if !right.is_empty() {
            right.push(separator());
        }
        right.push(Span::styled(badge.to_string(), dim));
    }
    TrayLanes {
        left,
        right,
        hint,
        context_label,
    }
}

/// The tray model label's thinking-effort suffix: a model without
/// reasoning or a level outside the wire vocabulary renders the bare id;
/// the tray shows `model:off`, the agents-view Model column hides "off".
pub(crate) fn tray_thinking_suffix(state: &Value) -> Option<String> {
    let reasoning = state
        .get("model")
        .and_then(|model| model.get("reasoning"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !reasoning {
        return None;
    }
    state
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .and_then(pa_types::ai::thinking_level_from_str)
        .map(|level| level.wire_name().to_string())
}

/// Truncate a styled span row to a visible width, replacing the tail with
/// the ellipsis when it does not fit.
fn truncate_spans_to_width(spans: &[crate::Span], width: usize) -> Vec<crate::Span> {
    let mut out: Vec<crate::Span> = Vec::new();
    let mut remaining = width;
    for (index, span) in spans.iter().enumerate() {
        if remaining == 0 {
            // The frame filled on a span boundary: the later spans still
            // exist, so the ellipsis must land.
            if content_follows(spans, index) {
                land_marker(&mut out, width);
            }
            break;
        }
        let mut text = String::new();
        let mut consumed = 0usize;
        for ch in span.content.chars() {
            let char_width = crate::width::char_width(ch);
            if consumed + char_width > remaining {
                break;
            }
            text.push(ch);
            consumed += char_width;
        }
        if text.is_empty() {
            // This span's first character cannot fit: nothing of it
            // renders, and the ellipsis must still mark the cut.
            if content_follows(spans, index) {
                land_marker(&mut out, width);
            }
            break;
        }
        if consumed < str_width(&span.content) {
            // The span could not fit whole: the ellipsis borrows its column from
            // the span's last kept character, so the row always ends INSIDE the width.
            let ellipsis = crate::width::char_width('\u{2026}');
            while consumed + ellipsis > remaining {
                match text.pop() {
                    Some(dropped) => consumed -= crate::width::char_width(dropped),
                    None => break,
                }
            }
            text.push('\u{2026}');
            let mut piece = span.clone();
            piece.content = text;
            out.push(piece);
            break;
        }
        let mut piece = span.clone();
        piece.content = text;
        out.push(piece);
        remaining -= consumed;
    }
    out
}

/// Whether any span from `index` (inclusive) still carries content — a
/// cut there must leave a marker.
fn content_follows(spans: &[crate::Span], index: usize) -> bool {
    spans[index..].iter().any(|span| !span.content.is_empty())
}

/// Land the truncation marker on a row that filled the frame on a span
/// boundary: the ellipsis borrows a column from the last kept character.
fn land_marker(out: &mut Vec<crate::Span>, width: usize) {
    let ellipsis = crate::width::char_width('\u{2026}');
    let row_width = |out: &Vec<crate::Span>| {
        out.iter()
            .map(|piece| crate::width::str_width(&piece.content))
            .sum::<usize>()
    };
    while row_width(out) + ellipsis > width {
        match out.last_mut() {
            Some(piece) => {
                if piece.content.pop().is_none() {
                    out.pop();
                }
            }
            None => break,
        }
    }
    match out.last_mut() {
        Some(piece) => piece.content.push('\u{2026}'),
        None => {
            if ellipsis <= width {
                out.push(crate::Span::raw("\u{2026}"));
            }
        }
    }
}

/// The framed activity dock: a muted separator rule above the
/// actionable groups — one line of counts while unfocused, one row per
/// group while focused. The dock renders in every session, all-zero
/// included. The TS summary line wraps its content in an
/// accent-colored box (`╭─ subagents ─╮`); the inline design language
/// keeps the separation with the same muted `─` rule that frames the
/// pickers' search fields, not an accent box.
///
/// The collapsed line color-codes live activity (the operator's 2026-09-24
/// directive): every count-holding segment goes green while its count
/// is above zero (subagents, heartbeats, shells, factory, the active
/// goal) and stays neutral at zero. The subagents segment is one
/// consolidated item — `◆ x subagents` (the operator's 2026-09-25
/// consolidation: the separate running cluster was redundant).
#[must_use]
pub fn render_activity_dock(dock: &ActivityDock, theme: &Theme, width: usize) -> Vec<Line> {
    render_activity_dock_segments(dock, theme, width).0
}

/// One rendered group's click surface: a plain click on the group's own
/// cells opens that group's view (the dock's Enter route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityDockSegment {
    pub group: ActivityGroup,
    /// The index of the group's line in the rendered frame (the rule is
    /// line 0).
    pub row: usize,
    /// The row columns the group's spans occupy.
    pub cols: std::ops::Range<usize>,
}

/// The framed activity dock plus its groups' click segments (see
/// [`render_activity_dock`]): collapsed, the segments cover exactly the
/// cells each group renders, clamped to the row the truncation kept;
/// expanded, each group's whole row.
#[must_use]
pub fn render_activity_dock_segments(
    dock: &ActivityDock,
    theme: &Theme,
    width: usize,
) -> (Vec<Line>, Vec<ActivityDockSegment>) {
    let rule = vec![theme.fg_span(ThemeColor::BorderMuted, "─".repeat(width))];
    if dock.focused {
        let mut lines = vec![rule];
        let mut segments = Vec::new();
        for group in dock.groups() {
            segments.push(ActivityDockSegment {
                group,
                row: lines.len(),
                cols: 0..width,
            });
            lines.push(render_expanded_dock_row(dock, group, theme, width));
        }
        return (lines, segments);
    }
    // The status-dot vocabulary (`● running / ◐ idle / ○ inactive`): the half
    // circle marks waiting work; only the heartbeat and factory pauses keep a dot.
    let cluster = |text: &str, color: ThemeColor| {
        vec![
            theme.fg_span(ThemeColor::Dim, " · ".to_string()),
            theme.fg_span(color, text.to_string()),
        ]
    };
    // The live-only number: actively-running subagents right now, direct
    // children and nested descendants together — ONE number.
    let running = dock.subagents_running_direct + dock.subagents_running_nested;
    let shells = dock.shells.len();
    let factory = dock.factory_counts;
    // One lane's label at a shrink step: the glyph and count, then the word until the lane
    // words drop.
    let lane_label = |glyph: &str, count: usize, word: &str, plural: &str, shrink: DockShrink| {
        if shrink < DockShrink::CountsOnly {
            format!(
                "{glyph} {count} {word}{}",
                if count == 1 { "" } else { plural }
            )
        } else {
            format!("{glyph} {count}")
        }
    };
    let row_at = |shrink: DockShrink| {
        let separator = if shrink < DockShrink::TightSeparators {
            "  \u{b7}  "
        } else {
            " \u{b7} "
        };
        let clusters = shrink < DockShrink::NoClusters;
        let mut line = vec![Span::raw(" ")];
        let mut segments: Vec<ActivityDockSegment> = Vec::new();
        for group in dock.groups() {
            // A zero-count lane with nothing waiting says nothing once the row is tight; the
            // subagents lane anchors the dock and always stays.
            let idle = match group {
                ActivityGroup::Subagents | ActivityGroup::Goal => false,
                ActivityGroup::Heartbeats => dock.heartbeats + dock.heartbeats_paused == 0,
                ActivityGroup::Bash => shells == 0,
                ActivityGroup::Factory => factory.running + factory.paused == 0,
            };
            let hidden = match group {
                ActivityGroup::Goal => shrink >= DockShrink::NoGoal,
                ActivityGroup::Subagents
                | ActivityGroup::Heartbeats
                | ActivityGroup::Bash
                | ActivityGroup::Factory => idle && shrink >= DockShrink::NoIdleLanes,
            };
            if hidden {
                continue;
            }
            if !segments.is_empty() {
                line.push(theme.fg_span(ThemeColor::Dim, separator));
            }
            // The segment's column span is what the terminal paints (the
            // truncation below clamps its end).
            let start = line.iter().map(|s| str_width(&s.content)).sum();
            match group {
                ActivityGroup::Subagents => {
                    line.push(theme.fg_span(
                        activity_color(running),
                        lane_label("\u{25c6}", running, "subagent", "s", shrink),
                    ));
                }
                ActivityGroup::Heartbeats => {
                    line.push(theme.fg_span(
                        activity_color(dock.heartbeats),
                        lane_label("\u{25f7}", dock.heartbeats, "heartbeat", "s", shrink),
                    ));
                    if dock.heartbeats_paused > 0 && clusters {
                        line.extend(cluster(
                            &format!("\u{25d0} {} paused", dock.heartbeats_paused),
                            ThemeColor::Warning,
                        ));
                    }
                }
                // The label reads "shell(s)": the tool name stays bash() everywhere else.
                ActivityGroup::Bash => {
                    line.push(theme.fg_span(
                        activity_color(shells),
                        lane_label("\u{25b8}", shells, "shell", "s", shrink),
                    ));
                    if dock.shells_persistent > 0 && clusters {
                        line.extend(cluster(
                            &format!("{} persistent", dock.shells_persistent),
                            activity_color(shells),
                        ));
                    }
                }
                // The factory indicator counts the running runs, and the paused
                // ones in their own amber cluster (the heartbeats' grammar): done
                // and failed runs stay as panels inside the factory page.
                ActivityGroup::Factory => {
                    line.push(theme.fg_span(
                        activity_color(factory.running),
                        lane_label("\u{2699}", factory.running, "factory", "", shrink),
                    ));
                    if factory.paused > 0 && clusters {
                        line.extend(cluster(
                            &format!("\u{25d0} {} paused", factory.paused),
                            ThemeColor::Warning,
                        ));
                    }
                }
                ActivityGroup::Goal => line.push(goal_span(dock, theme)),
            }
            let end = line.iter().map(|s| str_width(&s.content)).sum();
            segments.push(ActivityDockSegment {
                group,
                row: 1,
                cols: start..end,
            });
        }
        (line, segments)
    };
    let (line, segments) = DOCK_SHRINK
        .iter()
        .map(|shrink| row_at(*shrink))
        .find(|(line, _)| line.iter().map(|s| str_width(&s.content)).sum::<usize>() <= width)
        .unwrap_or_else(|| row_at(DockShrink::NoGoal));
    // Only a subagent count wider than the whole row still truncates.
    let row = truncate_spans_to_width(&line, width);
    let rendered = row
        .iter()
        .map(|s| str_width(&s.content))
        .sum::<usize>()
        .min(width);
    // A segment that no longer renders stays out of the click surface,
    // and a partially kept one ends at the last painted column.
    let segments = segments
        .into_iter()
        .filter(|segment| segment.cols.start < rendered)
        .map(|mut segment| {
            segment.cols.end = segment.cols.end.min(rendered);
            segment
        })
        .collect();
    (vec![rule, row], segments)
}

/// The collapsed dock's shrink order as the width drops (the tray's [`TrayShrink`] grammar).
/// Every step keeps the previous steps' cuts and gives up one more thing, whole: the wide
/// `  ·  ` separators tighten to ` · `, the lane words go (`◆ 2 subagents` → `◆ 2`), the
/// zero-count lanes drop (the subagents lane stays), the paused/persistent clusters drop, and
/// the goal label goes last. Nothing is cut mid-word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DockShrink {
    Full,
    TightSeparators,
    CountsOnly,
    NoIdleLanes,
    NoClusters,
    NoGoal,
}

const DOCK_SHRINK: [DockShrink; 6] = [
    DockShrink::Full,
    DockShrink::TightSeparators,
    DockShrink::CountsOnly,
    DockShrink::NoIdleLanes,
    DockShrink::NoClusters,
    DockShrink::NoGoal,
];

/// A count-holding segment's color: green while its count is above
/// zero, neutral at zero.
fn activity_color(count: usize) -> ThemeColor {
    if count > 0 {
        ThemeColor::Success
    } else {
        ThemeColor::Muted
    }
}

/// The goal group's label: an actively pursued goal reads green, and the
/// paused and budget-limited states read amber (the paused heartbeat
/// cluster's own warning color) — the dock is the goal's one chrome
/// surface, so every live state stays visible.
fn goal_span(dock: &ActivityDock, theme: &Theme) -> Span {
    let goal = dock.goal_label.as_deref().unwrap_or_default();
    let color = if goal.starts_with("Pursuing goal") {
        ThemeColor::Success
    } else {
        ThemeColor::Warning
    };
    theme.fg_span(color, goal.to_string())
}

/// The widest group name in the expanded dock (`heartbeats`): every
/// row's details start in the same column.
const EXPANDED_NAME_CELLS: usize = 10;

/// The separator between two items of one expanded row.
const EXPANDED_ITEM_SEPARATOR: &str = "  ·  ";

/// One expanded dock row; the selected row carries the shared selection
/// band across the full width.
fn render_expanded_dock_row(
    dock: &ActivityDock,
    group: ActivityGroup,
    theme: &Theme,
    width: usize,
) -> Line {
    let row = match group {
        ActivityGroup::Subagents => {
            let running = dock.subagents_running_direct + dock.subagents_running_nested;
            let breakdown =
                (running + dock.subagents_idle + dock.subagents_inactive > 0).then(|| {
                    format!(
                        "{running} running · {} idle · {} inactive",
                        dock.subagents_idle, dock.subagents_inactive
                    )
                });
            expanded_group_row(
                theme,
                width,
                ("◆", "subagents"),
                running,
                breakdown.as_slice(),
            )
        }
        ActivityGroup::Heartbeats => expanded_group_row(
            theme,
            width,
            ("◷", "heartbeats"),
            dock.heartbeats_active.len(),
            &dock.heartbeats_active,
        ),
        // The breakdown leads, then each running command.
        ActivityGroup::Bash => {
            let breakdown = (!dock.shells.is_empty()).then(|| {
                format!(
                    "{} running · {} persistent",
                    dock.shells.len(),
                    dock.shells_persistent
                )
            });
            let items: Vec<String> = breakdown.into_iter().chain(dock.shells.clone()).collect();
            expanded_group_row(theme, width, ("▸", "shells"), dock.shells.len(), &items)
        }
        // The stored specs and live runs lead, then each live run.
        ActivityGroup::Factory => {
            let counts = crate::factory_view::FactoryCounts {
                specs: dock.factory_specs,
                runs: dock.factory_counts,
            };
            let items: Vec<String> = counts
                .label()
                .into_iter()
                .chain(dock.factory_runs.clone())
                .collect();
            expanded_group_row(
                theme,
                width,
                ("⚙", "factory"),
                dock.factory_counts.running,
                &items,
            )
        }
        ActivityGroup::Goal => vec![Span::raw(" "), goal_span(dock, theme)],
    };
    let mut row = truncate_spans_to_width(&row, width);
    if dock.selected == group {
        let filled: usize = row.iter().map(|s| str_width(&s.content)).sum();
        row.push(Span::raw(" ".repeat(width.saturating_sub(filled))));
        let band = theme.selection_row_style();
        for span in &mut row {
            span.style = span.style.patch(band);
        }
    }
    row
}

/// A group row: the glyph and name, then the details (as many items as
/// fit, `+N` for the rest). An empty group renders its glyph and name
/// dimmed with no details.
fn expanded_group_row(
    theme: &Theme,
    width: usize,
    (glyph, name): (&str, &str),
    count: usize,
    items: &[String],
) -> Line {
    let title = format!("{glyph} {name}");
    if items.is_empty() {
        return vec![Span::raw(" "), theme.fg_span(ThemeColor::Dim, title)];
    }
    let mut row = vec![
        Span::raw(" "),
        theme.fg_span(activity_color(count), title),
        Span::raw(" ".repeat(EXPANDED_NAME_CELLS - str_width(name) + 2)),
    ];
    let used: usize = row.iter().map(|s| str_width(&s.content)).sum();
    let available = width.saturating_sub(used);
    let shown = fitting_items(items, available);
    let overflow = (shown < items.len()).then(|| format!("  +{}", items.len() - shown));
    let marker = overflow.as_deref().map_or(0, str_width);
    for (index, item) in items[..shown].iter().enumerate() {
        if index > 0 {
            row.push(theme.fg_span(ThemeColor::Dim, EXPANDED_ITEM_SEPARATOR));
        }
        // Only an oversized first item gets clipped here, to leave the `+N` marker its room.
        let item = if index == 0 {
            crate::width::truncate_to_width(item, available.saturating_sub(marker), "\u{2026}")
        } else {
            item.clone()
        };
        row.push(theme.fg_span(ThemeColor::Muted, item));
    }
    if let Some(overflow) = overflow {
        row.push(theme.fg_span(ThemeColor::Dim, overflow));
    }
    row
}

/// How many leading items fit `available` columns together with the
/// `+N` overflow marker for the rest; the first item always shows (the
/// caller clips an oversized one beside the marker).
fn fitting_items(items: &[String], available: usize) -> usize {
    let separator = str_width(EXPANDED_ITEM_SEPARATOR);
    let mut listed = 0;
    let mut fitting = 1;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            listed += separator;
        }
        listed += str_width(item);
        if listed > available {
            break;
        }
        let shown = index + 1;
        let overflow = if shown < items.len() {
            str_width(&format!("  +{}", items.len() - shown))
        } else {
            0
        };
        if listed + overflow <= available {
            fitting = shown;
        }
    }
    fitting
}

/// The footer's tok/sec row: one dim line — the dock's last row —
/// truncated with no ellipsis when it overflows the width.
#[must_use]
pub fn render_speed_footer(text: &str, theme: &Theme, width: usize) -> Line {
    let dim = theme.fg_style(ThemeColor::Dim);
    let text = truncate_to_width(text, width, "");
    vec![Span::styled(text, dim)]
}

/// The editor surface background: `userMessageBg` (TS `getEditorTheme`).
#[must_use]
pub fn editor_background(theme: &Theme) -> ratatui::style::Style {
    theme.bg_style(ThemeBg::UserMessageBg)
}
#[cfg(test)]
mod tests;
