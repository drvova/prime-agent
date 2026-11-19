//! The context window: what fills the model's context and what it costs, opened from the tray's
//! context gauge (a click, after oh-my-pi's clickable context meter), `/context`, or its key. It
//! mounts over the editor dock like the `/usage` page and paints at once from the state the tray
//! already holds (tokens, window, model, thinking level, autocompact threshold, session cost);
//! the one `get_context_breakdown` request fills in the category estimate, the cache and
//! compaction history, and the subagent rows when it lands. Every row fits its width: a stat row
//! drops its detail, then its share column, before the label truncates.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use pa_types::context_breakdown::{
    CategoryUsage, ContextBreakdown, ContextCategories, RequestCache,
};

use crate::chrome::{format_token_count, ChromeState, ContextReading};
use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{title_row, EscVerb, Footer, HintRank, CHROME_INDENT};
use crate::menu_panel::{menu_list_layout, scroll_row};
use crate::theme::{Theme, ThemeColor};
use crate::usage_page::{countdown, Section};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

/// The frame's rows outside the content window: the rule, the title, the blank under it, the
/// footer, and the trailing blank.
const FIXED_FRAME_ROWS: usize = 5;
const SCROLL_INDICATOR_ROWS: usize = 1;
const ROW_INDENT: &str = "    ";
/// The usage bar's widest run.
const MAX_BAR: usize = 48;
/// Subagent rows shown before the rest fold into one `/context all` row.
const AGENT_ROW_BUDGET: usize = 8;

/// Where the window was opened from (the adoption event's `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextViewSource {
    /// A click on the tray's gauge or context label.
    Click,
    /// `/context`.
    Command,
    /// The `app.context.open` key.
    Shortcut,
}

impl ContextViewSource {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ContextViewSource::Click => "click",
            ContextViewSource::Command => "command",
            ContextViewSource::Shortcut => "shortcut",
        }
    }
}

/// How the breakdown request ended up when the window closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextBreakdownOutcome {
    /// The reply landed with the category estimate.
    Loaded,
    /// The reply landed without one (the engine cannot measure it).
    Unavailable,
    /// The request failed.
    Failed,
    /// The window closed before the reply.
    Loading,
}

impl ContextBreakdownOutcome {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ContextBreakdownOutcome::Loaded => "loaded",
            ContextBreakdownOutcome::Unavailable => "unavailable",
            ContextBreakdownOutcome::Failed => "failed",
            ContextBreakdownOutcome::Loading => "loading",
        }
    }
}

/// The adoption event's primitives (`tui context window closed`): counts and enums only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextViewSummary {
    pub source: ContextViewSource,
    pub breakdown: ContextBreakdownOutcome,
    /// Category rows the window showed.
    pub category_count: u64,
    pub subagent_count: u64,
}

/// The outcome of one key press in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextViewAction {
    /// Consumed (scrolling or an inert key).
    None,
    /// Close the window.
    Close,
}

/// The context window.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    source: ContextViewSource,
    scroll: usize,
    /// The content window's height at the last paint (the scroll keys walk the painted window).
    visible_rows: usize,
    breakdown: Section<ContextBreakdown>,
}

/// One category's row: its label, its usage, and the detail word for its item count.
struct CategoryRow {
    label: &'static str,
    usage: CategoryUsage,
    /// The item noun, singular and plural (empty: no item detail).
    items: (&'static str, &'static str),
}

/// The category rows the reply can fill, in display order. A row with nothing in it (no images,
/// no thinking) stays out; the prompt, tool, and message rows always show.
fn category_rows(categories: &ContextCategories) -> Vec<CategoryRow> {
    let row = |label, usage, items| CategoryRow {
        label,
        usage,
        items,
    };
    let rows = [
        (
            true,
            row("System prompt", categories.system_static, ("", "")),
        ),
        (
            true,
            row("Dynamic tail", categories.system_dynamic, ("", "")),
        ),
        (
            true,
            row("Tool schemas", categories.tools, ("tool", "tools")),
        ),
        (
            true,
            row("Messages", categories.messages, ("message", "messages")),
        ),
        (
            false,
            row("Tool calls", categories.tool_calls, ("call", "calls")),
        ),
        (
            false,
            row(
                "Tool results",
                categories.tool_results,
                ("result", "results"),
            ),
        ),
        (false, row("Images", categories.images, ("image", "images"))),
        (
            false,
            row("Thinking", categories.thinking, ("block", "blocks")),
        ),
        (
            false,
            row("Harness digest", categories.harness_digest, ("", "")),
        ),
    ];
    rows.into_iter()
        .filter(|(always, row)| *always || row.usage.items > 0)
        .map(|(_, row)| row)
        .collect()
}

/// The prompt-cache hit rate in percent: the share of the prompt read from the cache,
/// `cache_read / (input + cache_read + cache_write)`. `None` for an empty prompt. The context
/// window's cache section and the run summary row's `N% cached` share this one definition.
pub(crate) fn cache_hit_percent(prompt: RequestCache) -> Option<f64> {
    let total = prompt.input + prompt.cache_read + prompt.cache_write;
    (total > 0).then(|| prompt.cache_read as f64 / total as f64 * 100.0)
}

fn money(cost: f64) -> String {
    format!("${cost:.2}")
}

fn share(tokens: u64, window: u64) -> String {
    if window == 0 {
        return String::new();
    }
    let percent = tokens as f64 / window as f64 * 100.0;
    if percent > 0.0 && percent < 0.1 {
        "<0.1%".to_string()
    } else {
        format!("{percent:.1}%")
    }
}

/// A section heading at the chrome indent.
fn heading(theme: &Theme, width: usize, text: &str) -> Line {
    truncate_line(
        &vec![
            Span::raw(CHROME_INDENT),
            theme.fg_span(ThemeColor::Muted, text.to_string()),
        ],
        width,
        "",
    )
}

/// A prose row at the row indent. Too narrow for one row, it wraps at its ` · ` separators
/// (greedily, whole segments per row) and only truncates a segment wider than the row itself.
fn note(theme: &Theme, width: usize, color: ThemeColor, text: &str) -> Vec<Line> {
    let budget = width.saturating_sub(str_width(ROW_INDENT));
    let separator = " \u{b7} ";
    let mut rows: Vec<String> = Vec::new();
    for segment in text.split(separator) {
        match rows.last_mut() {
            Some(row) if str_width(row) + str_width(separator) + str_width(segment) <= budget => {
                row.push_str(separator);
                row.push_str(segment);
            }
            Some(_) | None => rows.push(segment.to_string()),
        }
    }
    rows.iter()
        .map(|row| {
            truncate_line(
                &vec![Span::raw(ROW_INDENT), theme.fg_span(color, row.clone())],
                width,
                "\u{2026}",
            )
        })
        .collect()
}

/// The stat-row columns: the labels' shared width, then the right-aligned value, share, and
/// detail columns.
struct StatColumns {
    label: usize,
    value: usize,
    share: usize,
}

/// One aligned stat row. Too narrow for everything, it drops the detail, then the share, and
/// only then truncates the label.
fn stat_row(
    theme: &Theme,
    width: usize,
    columns: &StatColumns,
    (label, value, share, detail): (&str, &str, &str, &str),
    value_color: ThemeColor,
) -> Line {
    let fixed = str_width(ROW_INDENT) + columns.label + 2 + columns.value;
    let with_share = fixed + 2 + columns.share;
    // An empty share still holds its column, so the details line up.
    let show_share = columns.share > 0 && with_share <= width;
    let show_detail =
        !detail.is_empty() && show_share && with_share + 2 + str_width(detail) <= width;
    let label_width = columns
        .label
        .min(width.saturating_sub(str_width(ROW_INDENT) + 2 + columns.value));
    let label = truncate_to_width(label, label_width, "…");
    let mut line = vec![
        Span::raw(ROW_INDENT),
        theme.fg_span(ThemeColor::Text, label.clone()),
        Span::raw(" ".repeat(label_width.saturating_sub(str_width(&label)) + 2)),
        theme.fg_span(
            value_color,
            format!("{value:>width$}", width = columns.value),
        ),
    ];
    if show_share {
        line.push(theme.fg_span(
            ThemeColor::Dim,
            format!("  {share:>width$}", width = columns.share),
        ));
    }
    if show_detail {
        line.push(theme.fg_span(ThemeColor::Dim, format!("  {detail}")));
    }
    truncate_line(&line, width, "…")
}

/// The usage bar: `━` for the used share in the gauge's tone, `─` for the rest, a `┃` tick at
/// the autocompact threshold.
fn usage_bar(theme: &Theme, width: usize, percent: f64, threshold: Option<f64>) -> Line {
    let cells = width.saturating_sub(str_width(ROW_INDENT) + 2).min(MAX_BAR);
    if cells < 6 {
        return Vec::new();
    }
    let used = ((cells as f64 * percent / 100.0).round() as usize).min(cells);
    let tone = if percent >= 90.0 {
        ThemeColor::Error
    } else if percent >= 70.0 {
        ThemeColor::Warning
    } else {
        ThemeColor::Accent
    };
    let tick = threshold.map(|at| ((cells as f64 * at / 100.0).round() as usize).min(cells - 1));
    let mut line = vec![Span::raw(ROW_INDENT)];
    for cell in 0..cells {
        let span = if Some(cell) == tick {
            theme.fg_span(ThemeColor::Muted, "\u{2503}")
        } else if cell < used {
            theme.fg_span(tone, "\u{2501}")
        } else {
            theme.fg_span(ThemeColor::BorderMuted, "\u{2500}")
        };
        line.push(span);
    }
    line
}

impl ContextView {
    /// Open over the last landed breakdown (stale-while-revalidate); the request starts loading.
    #[must_use]
    pub fn new(source: ContextViewSource, last: Option<ContextBreakdown>) -> Self {
        Self {
            source,
            scroll: 0,
            visible_rows: 0,
            breakdown: Section::loading(last),
        }
    }

    /// A landed `get_context_breakdown` reply.
    pub fn apply_breakdown(&mut self, result: Result<ContextBreakdown, String>) {
        self.breakdown.land(result);
    }

    /// One key press: Esc (or Ctrl+C) closes, the selection keys scroll.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> ContextViewAction {
        if key == "ctrl+c"
            || kb.matches(key, "app.clear")
            || kb.matches(key, "tui.select.cancel")
            || kb.matches(key, "app.modal.back")
            || kb.matches(key, "app.context.open")
        {
            return ContextViewAction::Close;
        }
        let page = self.visible_rows.max(1) as isize;
        let delta = if kb.matches(key, "tui.select.up") {
            -1
        } else if kb.matches(key, "tui.select.down") {
            1
        } else if kb.matches(key, "tui.select.pageUp") {
            -page
        } else if kb.matches(key, "tui.select.pageDown") {
            page
        } else if kb.matches(key, "tui.select.top") {
            isize::MIN / 2
        } else if kb.matches(key, "tui.select.bottom") {
            isize::MAX / 2
        } else {
            0
        };
        // The bottom clamp runs at paint time, where the content height is known.
        self.scroll = (self.scroll as isize).saturating_add(delta).max(0) as usize;
        ContextViewAction::None
    }

    /// The adoption summary for the close.
    #[must_use]
    pub fn summary(&self) -> ContextViewSummary {
        let data = self.breakdown.data.as_ref();
        let breakdown = match (data, self.breakdown.loading, &self.breakdown.error) {
            (Some(reply), _, _) if reply.categories.is_some() => ContextBreakdownOutcome::Loaded,
            (Some(_), _, _) => ContextBreakdownOutcome::Unavailable,
            (None, true, _) => ContextBreakdownOutcome::Loading,
            (None, false, _) => ContextBreakdownOutcome::Failed,
        };
        ContextViewSummary {
            source: self.source,
            breakdown,
            category_count: data
                .and_then(|reply| reply.categories.as_ref())
                .map_or(0, |categories| category_rows(categories).len() as u64),
            subagent_count: data.map_or(0, |reply| reply.agents.len() as u64),
        }
    }

    /// The usage head: tokens over the window, the bar, and the autocompact distance.
    fn usage_rows(theme: &Theme, width: usize, chrome: &ChromeState) -> Vec<Line> {
        let Some(context) = chrome
            .context
            .as_ref()
            .filter(|context| context.reading != ContextReading::Unknown)
        else {
            return note(
                theme,
                width,
                ThemeColor::Dim,
                "Context usage is unknown until the next response.",
            );
        };
        let percent = context.percent();
        let headline = vec![
            Span::raw(ROW_INDENT),
            theme.fg_span(ThemeColor::Text, format_token_count(context.tokens)),
            theme.fg_span(
                ThemeColor::Dim,
                format!(
                    " / {} tokens \u{b7} {percent:.0}% used",
                    format_token_count(context.context_window)
                ),
            ),
        ];
        let mut rows = vec![truncate_line(&headline, width, "…")];
        if context.reading == ContextReading::Stale {
            rows.extend(note(
                theme,
                width,
                ThemeColor::Dim,
                "Last known value: the next reply measures the context again.",
            ));
        }
        rows.push(usage_bar(theme, width, percent, chrome.compaction_percent));
        rows.retain(|row| !row.is_empty());
        let threshold = match chrome.compaction_percent {
            Some(threshold) => {
                let at = (context.context_window as f64 * threshold / 100.0).round() as u64;
                match at.checked_sub(context.tokens).filter(|left| *left > 0) {
                    Some(left) => format!(
                        "Autocompact at {threshold:.0}% ({}) \u{b7} {} to go",
                        format_token_count(at),
                        format_token_count(left)
                    ),
                    None => format!("Past the {threshold:.0}% autocompact threshold"),
                }
            }
            None => "Autocompact is off".to_string(),
        };
        rows.extend(note(theme, width, ThemeColor::Dim, &threshold));
        rows
    }

    /// The category estimate, or the reason it is missing.
    fn breakdown_rows(&self, theme: &Theme, width: usize, window: u64) -> Vec<Line> {
        let mut rows = vec![heading(theme, width, "Breakdown (estimated)")];
        let categories = match (&self.breakdown.data, &self.breakdown.error) {
            (Some(reply), _) => reply.categories.as_ref(),
            (None, Some(error)) => {
                rows.extend(note(
                    theme,
                    width,
                    ThemeColor::Warning,
                    &format!("Unavailable: {error}"),
                ));
                return rows;
            }
            (None, None) => {
                rows.extend(note(theme, width, ThemeColor::Dim, "Measuring\u{2026}"));
                return rows;
            }
        };
        let Some(categories) = categories else {
            let reason = self
                .breakdown
                .data
                .as_ref()
                .and_then(|reply| reply.categories_error.as_deref());
            rows.extend(match reason {
                Some(error) => note(
                    theme,
                    width,
                    ThemeColor::Warning,
                    &format!("Unavailable: {error}"),
                ),
                None => note(
                    theme,
                    width,
                    ThemeColor::Dim,
                    "This session does not report a breakdown.",
                ),
            });
            return rows;
        };
        let category_rows = category_rows(categories);
        let total: u64 = category_rows.iter().map(|row| row.usage.tokens).sum();
        let mut cells: Vec<(String, String, String, String, ThemeColor)> = category_rows
            .iter()
            .map(|row| {
                let detail = match (row.items, row.usage.items) {
                    (("", _), _) => String::new(),
                    ((singular, _), 1) => format!("1 {singular}"),
                    ((_, plural), count) => format!("{count} {plural}"),
                };
                (
                    row.label.to_string(),
                    format_token_count(row.usage.tokens),
                    share(row.usage.tokens, window),
                    detail,
                    ThemeColor::Text,
                )
            })
            .collect();
        if let Some(thinning) = categories.thinning {
            cells.push((
                "Thinned away".to_string(),
                format_token_count(thinning.tokens_saved),
                String::new(),
                match thinning.thinned_results {
                    1 => "1 result thinned".to_string(),
                    count => format!("{count} results thinned"),
                },
                ThemeColor::Success,
            ));
        }
        cells.push((
            "Total".to_string(),
            format_token_count(total),
            share(total, window),
            String::new(),
            ThemeColor::Accent,
        ));
        let columns = StatColumns {
            label: cells
                .iter()
                .map(|cell| str_width(&cell.0))
                .max()
                .unwrap_or(0),
            value: cells
                .iter()
                .map(|cell| str_width(&cell.1))
                .max()
                .unwrap_or(0),
            share: cells
                .iter()
                .map(|cell| str_width(&cell.2))
                .max()
                .unwrap_or(0),
        };
        for (label, value, share, detail, color) in &cells {
            rows.push(stat_row(
                theme,
                width,
                &columns,
                (label, value, share, detail),
                *color,
            ));
        }
        rows
    }

    /// Cache, compaction, cost, subagents: the sections the reply fills (cost and the
    /// threshold already read from the tray's state).
    fn history_rows(
        &self,
        theme: &Theme,
        width: usize,
        chrome: &ChromeState,
        now_ms: i64,
    ) -> Vec<Line> {
        let reply = self.breakdown.data.as_ref();
        let mut rows = Vec::new();
        if let Some(reply) = reply {
            let cache = &reply.cache;
            rows.push(Vec::new());
            rows.push(heading(theme, width, "Prompt cache"));
            let totals = RequestCache {
                input: cache.input,
                cache_read: cache.cache_read,
                cache_write: cache.cache_write,
            };
            if let Some(hit) = cache_hit_percent(totals) {
                let mut text = format!(
                    "read {} \u{b7} write {} \u{b7} fresh {} \u{b7} hit {hit:.0}%",
                    format_token_count(cache.cache_read),
                    format_token_count(cache.cache_write),
                    format_token_count(cache.input),
                );
                if let Some(last) = cache.last_request.and_then(cache_hit_percent) {
                    let _ = write!(text, " (last {last:.0}%)");
                }
                rows.extend(note(theme, width, ThemeColor::Text, &text));
            } else {
                rows.extend(note(
                    theme,
                    width,
                    ThemeColor::Dim,
                    "No prompt tokens billed yet.",
                ));
            }
            rows.push(Vec::new());
            rows.push(heading(theme, width, "Compaction"));
            let text = match (reply.compaction.count, reply.compaction.last) {
                (0, _) | (_, None) => "No compactions yet.".to_string(),
                (count, Some(last)) => {
                    let ago = last.at_ms.map_or(String::new(), |at| {
                        format!(", {} ago", countdown(now_ms - at as i64))
                    });
                    format!(
                        "{count} so far \u{b7} the last replaced {}{ago}",
                        format_token_count(last.tokens_before)
                    )
                }
            };
            rows.extend(note(theme, width, ThemeColor::Text, &text));
        }
        rows.push(Vec::new());
        rows.push(heading(theme, width, "Cost"));
        // The headline is the spend the title bar already shows; the split rides the reply.
        let total = chrome.cost_usd.or(reply.map(|reply| reply.cost.total));
        let text = match (total, reply) {
            (Some(total), Some(reply)) if !reply.agents.is_empty() => format!(
                "{} session \u{b7} main {} \u{b7} subagents {}",
                money(total),
                money(reply.cost.own),
                money((reply.cost.total - reply.cost.own).max(0.0))
            ),
            (Some(total), _) => format!("{} session", money(total)),
            (None, _) => "No spend recorded yet.".to_string(),
        };
        rows.extend(note(theme, width, ThemeColor::Text, &text));
        if let Some(reply) = reply.filter(|reply| !reply.agents.is_empty()) {
            rows.push(Vec::new());
            rows.push(heading(theme, width, "Subagents"));
            let cells: Vec<(String, String, String, String)> = reply
                .agents
                .iter()
                .take(AGENT_ROW_BUDGET)
                .map(|agent| {
                    let indent = "  ".repeat(agent.depth.saturating_sub(1) as usize);
                    let context = match (agent.context_tokens, agent.context_window) {
                        (Some(tokens), Some(window)) if window > 0 => format!(
                            "{}/{} {:.0}%",
                            format_token_count(tokens),
                            format_token_count(window),
                            tokens as f64 / window as f64 * 100.0
                        ),
                        (Some(tokens), _) => format_token_count(tokens),
                        (None, _) => "\u{2014}".to_string(),
                    };
                    (
                        format!("{indent}{}", agent.label),
                        context,
                        money(agent.cost),
                        agent.status.clone(),
                    )
                })
                .collect();
            let columns = StatColumns {
                label: cells
                    .iter()
                    .map(|cell| str_width(&cell.0))
                    .max()
                    .unwrap_or(0)
                    .min(28),
                value: cells
                    .iter()
                    .map(|cell| str_width(&cell.1))
                    .max()
                    .unwrap_or(0),
                share: cells
                    .iter()
                    .map(|cell| str_width(&cell.2))
                    .max()
                    .unwrap_or(0),
            };
            for (label, context, cost, status) in &cells {
                rows.push(stat_row(
                    theme,
                    width,
                    &columns,
                    (label, context, cost, status),
                    ThemeColor::Text,
                ));
            }
            if reply.agents.len() > AGENT_ROW_BUDGET {
                rows.extend(note(
                    theme,
                    width,
                    ThemeColor::Dim,
                    &format!(
                        "{} more \u{b7} /context all lists every agent",
                        reply.agents.len() - AGENT_ROW_BUDGET
                    ),
                ));
            }
        }
        rows
    }

    /// The model the window describes.
    fn model_rows(theme: &Theme, width: usize, chrome: &ChromeState) -> Vec<Line> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(model) = &chrome.model_id {
            parts.push(model.clone());
        }
        if let Some(level) = &chrome.thinking_suffix {
            parts.push(format!("thinking {level}"));
        }
        if let Some(context) = &chrome.context {
            parts.push(format!(
                "{} window",
                format_token_count(context.context_window)
            ));
        }
        if parts.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![Vec::new(), heading(theme, width, "Model")];
        rows.extend(note(
            theme,
            width,
            ThemeColor::Text,
            &parts.join(" \u{b7} "),
        ));
        rows
    }

    /// Render the frame at the current row budget.
    pub fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        chrome: &ChromeState,
        viewport_rows: usize,
        now_ms: i64,
    ) -> Vec<Line> {
        let window = chrome.context.map_or(0, |context| context.context_window);
        let mut content = Self::usage_rows(theme, width, chrome);
        content.push(Vec::new());
        content.extend(self.breakdown_rows(theme, width, window));
        content.extend(self.history_rows(theme, width, chrome, now_ms));
        content.extend(Self::model_rows(theme, width, chrome));
        content.push(Vec::new());
        content.extend(note(
            theme,
            width,
            ThemeColor::Dim,
            "/compact compacts now \u{b7} /context all lists every agent",
        ));
        let total = content.len();
        let visible = menu_list_layout(
            Some(viewport_rows),
            viewport_rows,
            total,
            FIXED_FRAME_ROWS,
            SCROLL_INDICATOR_ROWS,
        );
        self.visible_rows = visible;
        self.scroll = self.scroll.min(total.saturating_sub(visible.max(1)));
        let end = (self.scroll + visible).min(total);
        let mut lines: Vec<Line> = Vec::with_capacity(FIXED_FRAME_ROWS + visible + 1);
        lines.push(vec![
            theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(width.max(1)))
        ]);
        lines.push(title_row(theme, width, "Context"));
        lines.push(Vec::new());
        lines.extend(content[self.scroll..end].iter().cloned());
        if end < total || self.scroll > 0 {
            lines.push(scroll_row(theme, width, self.scroll + 1, total));
        }
        let footer = if total > visible {
            Footer::new().bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "scroll",
                HintRank::Useful,
            )
        } else {
            Footer::new()
        };
        lines.push(footer.esc(kb, EscVerb::Close).line(theme, width));
        lines.push(Vec::new());
        lines.truncate(viewport_rows.max(1));
        lines
    }
}
