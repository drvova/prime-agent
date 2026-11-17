//! Transcript component rendering: status rows, the user-message block,
//! assistant messages (text, thinking, and their error rows), and the
//! working loader line. Tool-call cards live in `crate::tool_card`.

mod geometry;
pub(crate) use geometry::{assistant_row_count, user_block_row_count};

use crate::snapshot::RetryStartReason;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};

/// How much detail the conversation shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// `overview`: thinking hidden, tool output and edit diffs collapsed.
    Overview,
    /// `details`: thinking visible, edit diffs expanded, tool output collapsed.
    Details,
    /// `all`: thinking visible, edit diffs and tool output expanded.
    All,
}

impl Detail {
    /// The Ctrl+O cycle: overview -> details -> all -> overview.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Detail::Overview => Detail::Details,
            Detail::Details => Detail::All,
            Detail::All => Detail::Overview,
        }
    }

    #[must_use]
    pub fn show_thinking(self) -> bool {
        !matches!(self, Detail::Overview)
    }

    #[must_use]
    pub fn tool_output_expanded(self) -> bool {
        matches!(self, Detail::All)
    }

    #[must_use]
    pub fn edit_diffs_expanded(self) -> bool {
        !matches!(self, Detail::Overview)
    }

    /// The stored wire name (TS `ChatDetail`): "overview" | "details" | "all".
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Detail::Overview => "overview",
            Detail::Details => "details",
            Detail::All => "all",
        }
    }

    /// The level for a stored wire name: an unset or unknown value reads
    /// as the `overview` startup level (operator ruling 2026-09-28).
    #[must_use]
    pub fn from_wire_name(name: &str) -> Self {
        match name {
            "details" => Detail::Details,
            "all" => Detail::All,
            _ => Detail::Overview,
        }
    }
}

/// One rendered chat component.
#[derive(Debug, Clone, PartialEq)]
/// The style tier of a status row.
pub enum StatusKind {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEntry {
    /// Startup notices, client notes, turn errors.
    Status { text: String, kind: StatusKind },
    /// The user's submitted prompt.
    /// A user prompt; `sent_at` (epoch ms) renders as the slab's right-aligned clock.
    User { text: String, sent_at: Option<i64> },
    /// A durable session-command echo row (`session_slash_command`):
    /// the command as typed, laid out like a user message.
    SlashCommand { text: String },
    /// The compaction summary row (TS `CompactionSummaryMessageComponent`):
    /// `◆ Context compacted` with the summary below.
    CompactionSummary {
        summary: String,
        /// The context size before the compaction (the expanded metadata).
        tokens_before: u64,
        /// `/compact <instructions>` focus guidance.
        custom_instructions: Option<String>,
    },
    /// One assistant message: ordered content blocks.
    Assistant(Box<AssistantMessage>),
    /// One tool call and its execution state (rendered by
    /// [`crate::tool_card`]).
    Tool(Box<ToolCallCard>),
    /// One agent-message summary row: the received transcript rows.
    AgentMessage(Box<crate::custom_message::AgentMessageRow>),
    /// One skill-invocation card: the expandable `<skill>`-block card a
    /// user message carrying a skill invocation parses into.
    SkillInvocation(Box<crate::custom_message::SkillInvocationRow>),
    /// One injected prompt row.
    InjectedPrompt(Box<crate::custom_message::InjectedPromptRow>),
    /// One `!`/`!!` bash run: the live `bash_start`/`bash_output`/`bash_end` events, the replayed
    /// `bashExecution` row, and the pending-while-streaming hold all render through this card.
    BashExecution(Box<crate::bash_card::BashExecutionCard>),
    /// One background-shell completion row.
    ShellCompletion(Box<crate::custom_message::ShellCompletionRow>),
    /// One refinement outcome row.
    RefinementOutcome(Box<crate::custom_message::RefinementOutcomeRow>),
    /// One generic custom row.
    CustomPanel(Box<crate::custom_message::CustomPanelRow>),
}

impl ChatEntry {
    /// A user prompt with no known send time.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        ChatEntry::User {
            text: text.into(),
            sent_at: None,
        }
    }
}

// The card types live in `tool_card`; re-exported here because the
// transcript vocabulary (`ChatEntry`) is this module's.
pub use crate::tool_card::{render_tool_card, ToolCallCard, ToolResultView};
// The compaction rows (loader + summary) live in `compaction_row`; same
// re-export rule as the tool cards.
pub use crate::compaction_row::{
    render_compaction_loader, render_compaction_summary, CompactionReason, CompactionState,
};

/// An assistant message's visible content (tool calls move to cards).
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub blocks: Vec<MessageBlock>,
    /// `toolUse` when the message carried tool calls (drives spacers).
    pub has_tool_calls: bool,
    /// The message is still streaming (an update may replace its blocks).
    pub streaming: bool,
    /// A failed assistant message's error row: `aborted` always renders, `error` only
    /// without tool calls (their cards carry the failure).
    pub error: Option<String>,
    /// `stopReason: "aborted"` (drives the tool-call trailing spacer).
    pub aborted: bool,
}

impl AssistantMessage {
    /// The tool-call separator renders for visible bodies, aborted
    /// messages, and messages not following tool activity.
    #[must_use]
    pub fn has_trailing_space(&self, preceded_by_tool_activity: bool) -> bool {
        let has_visible_content = self.blocks.iter().any(|block| match block {
            MessageBlock::Thinking(text) | MessageBlock::Text(text) => !text.trim().is_empty(),
        });
        self.has_tool_calls && (has_visible_content || self.aborted || !preceded_by_tool_activity)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageBlock {
    Thinking(String),
    Text(String),
}

/// The working loader (TS `Loader`): spinner + activity label, or a
/// tool-owned working message while one is set (TS `workingMessage`).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkingState {
    pub activity: &'static str,
    /// A transient message owned by the running tool (set by the python-kernel
    /// bootstrap): replaces the activity label and the token count until the tool clears it.
    pub message: Option<String>,
    /// Streaming direction: `true` while tokens flow down.
    pub download: bool,
    pub tokens: u64,
    /// Whole seconds since the loader started.
    pub elapsed_secs: u64,
    /// The latest thought while the model thinks (its newest closed sentence, or the line still
    /// arriving before one closes): one dim italic row under the loader where the transcript
    /// folds thinking. `None` once the message moves on to text or tool calls.
    pub thought: Option<String>,
    /// Whole seconds since the session last sent anything (the stall timer).
    pub quiet_secs: u64,
    /// The running tool's title, the live label while a tool executes. Prime's tools carry no
    /// model-stated intent field, so this is the call's collapsed-card title.
    pub tool_label: Option<String>,
}

/// A silence this long while the model streams turns the loader into `Waiting on the model`.
const QUIET_AFTER_SECS: u64 = 3;

/// The activity a running tool sets. Tools are silent by nature, so it never reads as a stall.
pub(crate) const TOOL_ACTIVITY: &str = "Executing";

/// The live abort row's text (TS `message_end`'s aborted arm): the retry
/// count and the working-elapsed suffix ride the client, never the wire
/// (the rebuild path keeps the stored "Operation aborted").
#[must_use]
pub fn live_abort_text(retry_attempt: u32, elapsed_secs: Option<u64>) -> String {
    let elapsed_suffix = elapsed_secs
        .map(|secs| format!(" \u{00b7} {}", format_working_elapsed(secs)))
        .unwrap_or_default();
    if retry_attempt > 0 {
        format!(
            "Aborted after {retry_attempt} retry attempt{}{elapsed_suffix}",
            if retry_attempt > 1 { "s" } else { "" }
        )
    } else {
        format!("Operation aborted{elapsed_suffix}")
    }
}

#[must_use]
pub fn format_working_elapsed(total_secs: u64) -> String {
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let hours = total_mins / 60;
    if total_mins == 0 {
        return format!("{secs}s");
    }
    if hours == 0 {
        return format!("{mins}m {secs:02}s");
    }
    let days = hours / 24;
    if days == 0 {
        return format!("{hours}h {mins:02}m {secs:02}s");
    }
    format!("{days}d {:02}h {mins:02}m {secs:02}s", hours % 24)
}

impl WorkingState {
    /// The model went quiet past the threshold while no tool runs (a running tool or a
    /// tool-owned message never stalls): the loader says it is waiting on the model.
    #[must_use]
    pub fn stalled(&self) -> bool {
        self.message.is_none()
            && self.activity != TOOL_ACTIVITY
            && self.quiet_secs >= QUIET_AFTER_SECS
    }

    #[must_use]
    pub fn label(&self) -> String {
        // A tool-provided working message owns the loader line: plain
        // "<message> <elapsed>" (TS `getWorkingLoaderMessage`).
        if let Some(message) = &self.message {
            return format!("{message} {}", format_working_elapsed(self.elapsed_secs));
        }
        let stalled = self.stalled();
        let head = if stalled {
            "Waiting on the model".to_string()
        } else {
            match (&self.tool_label, self.activity == TOOL_ACTIVITY) {
                (Some(title), true) => format!("Running {title}"),
                (Some(_), false) | (None, true | false) => self.activity.to_string(),
            }
        };
        let mut parts = vec![head];
        parts.push(format_working_elapsed(self.elapsed_secs));
        if stalled {
            parts.push(format!("quiet {}", format_working_elapsed(self.quiet_secs)));
        }
        if self.tokens > 0 {
            parts.push(format!(
                "{} {} tokens",
                if self.download {
                    "\u{2193}"
                } else {
                    "\u{2191}"
                },
                crate::chrome::format_token_count(self.tokens)
            ));
        }
        parts.join(" \u{00b7} ")
    }
}

/// Spinner frames.
pub(crate) const LOADER_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

/// The loader's spinner per activity, so the animation says what is happening: an orbit
/// while waiting, a filling column while writing, a dense spin for code, a pulse while
/// executing; thinking keeps the classic frames.
fn activity_frames(activity: &str) -> &'static [&'static str] {
    match activity {
        "Waiting" => &[
            "\u{2808}", "\u{2810}", "\u{2820}", "\u{2880}", "\u{2840}", "\u{2804}", "\u{2802}",
            "\u{2801}",
        ],
        "Writing" => &[
            "\u{2840}", "\u{2844}", "\u{2846}", "\u{2847}", "\u{28c7}", "\u{28e7}", "\u{28f7}",
            "\u{28ff}", "\u{28fe}", "\u{28fc}", "\u{28f8}", "\u{28b8}", "\u{2838}", "\u{2818}",
            "\u{2808}", "\u{2800}",
        ],
        "Writing code" => &[
            "\u{28fe}", "\u{28fd}", "\u{28fb}", "\u{28bf}", "\u{287f}", "\u{28df}", "\u{28ef}",
            "\u{28f7}",
        ],
        "Executing" => &[
            "\u{2802}", "\u{2806}", "\u{2816}", "\u{2836}", "\u{2837}", "\u{283f}", "\u{2837}",
            "\u{2836}", "\u{2816}", "\u{2806}",
        ],
        _ => &LOADER_FRAMES,
    }
}

/// The working pulse icon frames (250ms interval): the shared "still working"
/// marker across the agents view, the subagent tray, and tool markers.
pub const WORKING_ICON_FRAMES: [&str; 4] = ["\u{25c7}", "\u{25c8}", "\u{25c6}", "\u{25c8}"];

#[must_use]
pub fn working_icon_frame(frame: usize) -> &'static str {
    WORKING_ICON_FRAMES[frame % WORKING_ICON_FRAMES.len()]
}

fn spacer() -> Line {
    Vec::new()
}

pub(crate) fn pad_to(line: Line, width: usize, base: Style) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    let mut out = line;
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), base));
    }
    out
}

/// Render a status text (TS `Text` with paddingX=1, paddingY=0): wrapped at
/// `width - 2`, one leading margin column, padded to the full width.
#[must_use]
pub fn render_text_rows(text: &str, style: Style, width: usize) -> Vec<Line> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let content_width = width.saturating_sub(2).max(1);
    let wrapped = crate::width::wrap_text(text, content_width);
    let row_count = wrapped.len();
    let mut out = Vec::new();
    for (index, line) in wrapped.into_iter().enumerate() {
        // The margin keeps the default foreground. Wrapped rows keep the ANSI state open
        // through their trailing padding, so continuation rows pad with the row style.
        let mut row: Line = vec![Span::raw(" ")];
        let styled: Line = line
            .into_iter()
            .map(|span| Span::styled(span.content, style))
            .collect();
        row.extend(styled);
        let padding_style = if index + 1 < row_count {
            style
        } else {
            Style::default()
        };
        out.push(pad_to(row, width, padding_style));
    }
    if out.is_empty() {
        out.push(vec![Span::styled(" ".repeat(width), Style::default())]);
    }
    out
}

/// The user-message block (TS `UserMessageComponent`: Box(2,1) on `userMessageBg`,
/// markdown inside colored `userMessageText`). Prompt-highlight tokens render
/// in their own colors (TS masks them to same-width placeholders so markdown cannot eat them).
#[must_use]
pub fn render_user_block(
    text: &str,
    theme: &Theme,
    code_block_indent: &str,
    width: usize,
) -> Vec<Line> {
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
    md.code_block_indent = code_block_indent.to_string();
    render_user_block_with_style(text, theme, &md, width)
}

/// The view supplies its session cwd through Markdown style, keeping href resolution local.
pub(crate) fn render_user_block_with_style(
    text: &str,
    theme: &Theme,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let content_width = width.saturating_sub(4).max(1);
    let body = theme.fg_style(ThemeColor::UserMessageText);
    let mask = geometry::user_mask(text);
    let rendered = crate::markdown::render_markdown(&mask.text, content_width, md);
    let mut rows: Vec<Line> = Vec::new();
    let blank = vec![Span::styled(" ".repeat(width), bg)];
    rows.push(blank.clone());
    if rendered.is_empty() {
        let row = vec![
            Span::styled("  ".to_string(), bg),
            Span::styled(String::new(), body),
        ];
        rows.push(pad_to(row, width, bg));
    }
    for line in rendered {
        let mut row: Line = vec![Span::styled("  ".to_string(), bg)];
        // The user block colors everything `userMessageText` on the block background;
        // markdown structure (wrapping) is kept, its own colors are not, and the link
        // affordance survives the restyle.
        let restyled: Line = line
            .into_iter()
            .map(|span| {
                let mut style = bg.patch(body);
                if span.style.add_modifier.contains(Modifier::UNDERLINED) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if span.style.fg == md.link_url.fg {
                    style = style.patch(md.link_url);
                }
                Span::styled(span.content, style)
            })
            .collect();
        row.extend(mask.restore_line(theme, &restyled));
        rows.push(pad_to(row, width, bg));
    }
    rows.push(blank);
    // Zone markers: `A` on the first block row, `B`/`C` on the last (TS
    // `UserMessageComponent.render`).
    if let Some(first) = rows.first_mut() {
        crate::osc133::mark_start(first);
    }
    if let Some(last) = rows.last_mut() {
        crate::osc133::mark_end(last);
    }
    rows
}

/// One assistant message: a leading spacer when a visible body exists, markdown
/// blocks separated by spacers, and a trailing spacer before its tool calls.
pub fn render_assistant(
    message: &AssistantMessage,
    detail: Detail,
    theme: &Theme,
    code_block_indent: &str,
    width: usize,
    preceded_by_tool_activity: bool,
    cache: &mut crate::markdown::MarkdownBlockCache,
) -> Vec<Line> {
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme).for_replies(theme);
    md.code_block_indent = code_block_indent.to_string();
    render_assistant_with_style(
        message,
        detail,
        theme,
        &md,
        width,
        preceded_by_tool_activity,
        cache,
    )
}

/// Render with the owning view's session-local Markdown context.
pub(crate) fn render_assistant_with_style(
    message: &AssistantMessage,
    detail: Detail,
    theme: &Theme,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
    preceded_by_tool_activity: bool,
    cache: &mut crate::markdown::MarkdownBlockCache,
) -> Vec<Line> {
    let visible_blocks = geometry::visible_blocks(message);
    let has_visible_content = !visible_blocks.is_empty();
    let mut out: Vec<Line> = Vec::new();
    if has_visible_content {
        out.push(spacer());
    }
    for (index, block) in visible_blocks.iter().enumerate() {
        match block {
            MessageBlock::Text(text) => {
                out.extend(render_markdown_block(text, md, width, cache));
            }
            MessageBlock::Thinking(text) => {
                if detail.show_thinking() {
                    out.extend(render_thinking_block(text, theme, md, width, cache));
                } else {
                    out.push(geometry::folded_thinking_row(text, theme));
                }
                // Thinking adds spacing only when another visible block follows.
                if index + 1 < visible_blocks.len() {
                    out.push(spacer());
                }
            }
        }
    }
    if let Some(error) = &message.error {
        out.push(spacer());
        // TS `createErrorComponent`: an error whose text ends with the
        // login-recovery suffix renders as one merged inline line.
        let merged = crate::error_summary::format_inline_login_recovery_message(error);
        out.extend(crate::error_summary::render_collapsible_error(
            merged.as_deref().unwrap_or(error),
            None,
            detail.tool_output_expanded(),
            ThemeColor::Error,
            theme,
            width,
        ));
    }
    if geometry::trailing_space(message, has_visible_content, preceded_by_tool_activity) {
        out.push(spacer());
    }
    // Zone markers on message bodies without tool calls (TS `AssistantMessageComponent.render`:
    // tool-call messages return unmarked).
    if !message.has_tool_calls {
        if let Some(first) = out.first_mut() {
            crate::osc133::mark_start(first);
        }
        if let Some(last) = out.last_mut() {
            crate::osc133::mark_end(last);
        }
    }
    out
}

/// Markdown rows with TS margins: rendered at `width - 2`, one leading margin
/// column, padded to the full width.
pub(crate) fn render_markdown_block(
    text: &str,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
    cache: &mut crate::markdown::MarkdownBlockCache,
) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let rendered =
        crate::markdown::render_markdown_tagged(text.trim(), content_width, md, "", cache);
    let mut out = Vec::new();
    for line in rendered {
        let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
        row.extend(line);
        // Padding never carries the content style: a dangling SGR prefix survives tmux's
        // trim on rows whose content style differs from the body color.
        out.push(pad_to(row, width, Style::default()));
    }
    out
}

/// The thinking block: markdown with every style collapsed to `dim`.
fn render_thinking_block(
    text: &str,
    theme: &Theme,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
    cache: &mut crate::markdown::MarkdownBlockCache,
) -> Vec<Line> {
    let md = geometry::thinking_style(md, theme);
    let content_width = width.saturating_sub(2).max(1);
    let rendered = crate::markdown::render_markdown_tagged(
        text.trim(),
        content_width,
        &md,
        geometry::THINKING_CACHE_TAG,
        cache,
    );
    let mut out = Vec::new();
    for line in rendered {
        // The markdown margin sits outside the styled content (default fg).
        let mut row: Line = vec![Span::raw(" ")];
        row.extend(line);
        // TS pads with unstyled spaces after the row's closing reset (see
        // render_markdown_block); padding never carries the dim content color.
        out.push(pad_to(row, width, Style::default()));
    }
    out
}

/// The working loader rows (TS `Loader.render`: `["", spinner + message]`).
#[must_use]
pub fn render_loader(
    working: &WorkingState,
    frame: usize,
    theme: &Theme,
    detail: Detail,
    width: usize,
    stop_key: Option<&str>,
) -> Vec<Line> {
    let accent = theme.fg_style(ThemeColor::Accent);
    let muted = theme.fg_style(ThemeColor::Muted);
    // A stalled stream takes the waiting orbit back.
    let frames = activity_frames(if working.stalled() {
        "Waiting"
    } else {
        working.activity
    });
    let spinner = frames[frame % frames.len()];
    let message = working.label();
    let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
    row.push(Span::styled(spinner.to_string(), accent));
    if !message.is_empty() {
        // The gap between the spinner and the label is unstyled: the plain space sits
        // between the two colored runs, so the row resets to default fg there.
        row.push(Span::raw(" ".to_string()));
        row.push(Span::styled(message, muted));
    }
    // The stop key as a filled chip, flush right: ` Esc  stop `.
    if let Some(key) = stop_key {
        let chip = vec![
            Span::styled(
                format!(" {key} "),
                theme
                    .bg_style(ThemeBg::SelectedBg)
                    .patch(theme.fg_style(ThemeColor::Text)),
            ),
            Span::styled("  stop ".to_string(), muted),
        ];
        let used = crate::width::line_width(&row) + crate::width::line_width(&chip);
        if used < width {
            row.push(Span::raw(" ".repeat(width - used)));
            row.extend(chip);
        }
    }
    let mut rows = vec![spacer(), pad_to(row, width, Style::default())];
    // The live thought sits under the spinner, aligned with its label; an expanded transcript
    // already streams the whole thinking block, so it shows nothing more here.
    if let (Some(thought), false) = (&working.thought, detail.show_thinking()) {
        let thought = crate::width::truncate_to_width(thought, width.saturating_sub(4), "\u{2026}");
        let style = theme
            .fg_style(ThemeColor::Dim)
            .add_modifier(Modifier::ITALIC);
        rows.push(pad_to(
            vec![Span::raw("   ".to_string()), Span::styled(thought, style)],
            width,
            Style::default(),
        ));
    }
    rows
}

/// An in-flight provider auto-retry (TS `retryLoader` + `CountdownTimer`):
/// replaces the working loader until the retry loop settles.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryState {
    pub attempt: u32,
    pub max_attempts: u32,
    pub ends_at: std::time::Instant,
    /// The provider error that started this retry.
    pub error_message: String,
    /// Why the retry started: a quick retry counts down; a provider
    /// failover switch names the backup the turn re-routes to.
    pub reason: RetryStartReason,
    /// The session model's provider, named in the countdown line (`openai 429`).
    pub provider: Option<String>,
}

impl RetryState {
    /// Whole seconds left in the countdown (never negative).
    #[must_use]
    pub fn seconds_left(&self) -> u64 {
        self.ends_at
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
    }
}

/// The retry loader rows. A quick retry is the nc-style countdown line, the amber
/// `retry 2 of 4 in 3s` head, then the provider and status, with the provider's own words on
/// the row under it. SANCTIONED DIVERGENCE from the TS `auto_retry_start` rendering (operator
/// ruling 2026-09-23): the episode names the ERROR too, not just the attempt. A failover
/// switch keeps its one muted line naming the backup.
#[must_use]
pub fn render_retry(retry: &RetryState, frame: usize, theme: &Theme, width: usize) -> Vec<Line> {
    let muted = theme.fg_style(ThemeColor::Muted);
    let warning = theme.fg_style(ThemeColor::Warning);
    let spinner = LOADER_FRAMES[frame % LOADER_FRAMES.len()];
    let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
    match &retry.reason {
        RetryStartReason::Quick => {
            row.push(Span::styled(spinner.to_string(), warning));
            // The gap between the spinner and the label is unstyled (the TS
            // `Loader` pen reset — see `render_loader`).
            row.push(Span::raw(" ".to_string()));
            row.push(Span::styled(
                format!(
                    "retry {} of {} in {}s",
                    retry.attempt,
                    retry.max_attempts,
                    retry.seconds_left()
                ),
                warning,
            ));
            // The provider and HTTP status (`openai 429`): the status is the error's leading
            // three-digit code, the provider the session model's.
            let status = retry
                .error_message
                .split_whitespace()
                .next()
                .filter(|word| word.len() == 3 && word.bytes().all(|byte| byte.is_ascii_digit()));
            let source = match (retry.provider.as_deref(), status) {
                (Some(provider), Some(status)) => Some(format!("{provider} {status}")),
                (Some(only), None) | (None, Some(only)) => Some(only.to_string()),
                (None, None) => None,
            };
            if let Some(source) = source {
                row.push(Span::styled(" \u{00b7} ".to_string(), muted));
                row.push(Span::styled(source, theme.fg_style(ThemeColor::Text)));
            }
            let reason = crate::width::truncate_to_width(
                &retry.error_message,
                width.saturating_sub(4),
                "\u{2026}",
            );
            let detail: Line = vec![Span::raw("   ".to_string()), Span::styled(reason, muted)];
            vec![
                spacer(),
                pad_to(row, width, Style::default()),
                pad_to(detail, width, Style::default()),
            ]
        }
        RetryStartReason::Backup { backup_model } => {
            row.push(Span::styled(spinner.to_string(), muted));
            row.push(Span::raw(" ".to_string()));
            row.push(Span::styled(
                format!(
                    "Primary model unavailable ({}) — retrying on backup model {backup_model}...",
                    retry.error_message
                ),
                muted,
            ));
            vec![spacer(), pad_to(row, width, Style::default())]
        }
    }
}

#[cfg(test)]
mod tests;
