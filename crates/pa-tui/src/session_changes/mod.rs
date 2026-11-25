//! The session changes panel (`/diff`, `app.changes.open`): the files the agent changed in this
//! turn or the whole session, each with its added and removed rows, its number of edits, and
//! whether a test or build ran after its last edit and how it went. Enter shows a file's
//! combined diff through the tool cards' own diff renderer; `app.changes.attach` puts the diff
//! in the composer for the next message (a long one as the editor's `[paste #N]` marker).
//!
//! Everything comes from the transcript the view holds (the cell cards' `details.diffs`, the
//! checks the [`ledger`] reads), so the session scope is the loaded history: while the
//! transcript window has not loaded the session's start, the panel says so.

mod ledger;

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) use ledger::ChangeLedger;
use ledger::{ChangeRow, ChangeSummary, Check, CheckKind, Evidence, Scope};

use crate::chat::ChatEntry;
use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{empty_state_row, title_row, EscVerb, Footer, HintRank, ELLIPSIS};
use crate::menu_panel::finish_menu_row;
use crate::theme::{Theme, ThemeColor};
use crate::width::{grapheme_width, pad_cell, str_width, truncate_line};
use crate::{Line, Span};

/// The bindings that open the panel, switch its scope, and attach the selected diff.
pub(crate) const OPEN_BINDING: &str = "app.changes.open";
pub(crate) const SCOPE_BINDING: &str = "app.changes.scope";
pub(crate) const ATTACH_BINDING: &str = "app.changes.attach";

/// Rows the list pane spends on chrome: title, subtitle, the blank under them, the blank above
/// the footer, the footer, and the blank under it.
const CHROME_ROWS: usize = 6;
/// The narrowest path cell before the row drops its edits column, then shortens its evidence.
const MIN_PATH_WIDTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    List,
    Diff { path: String },
}

/// What a key did that the session acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChangesKey {
    /// Consumed by the panel.
    Handled,
    Close,
    /// The scope toggled.
    Scoped,
    /// A file's diff opened.
    OpenedDiff,
    /// Attach this file's diff to the next message.
    Attach(String),
}

/// How much of the session the transcript window holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadedHistory {
    /// The session's start is loaded.
    Whole,
    /// Older pages are not loaded (or are evicted): the session scope is what is loaded.
    Partial,
}

/// The open panel.
#[derive(Debug)]
pub(crate) struct ChangesPanel {
    scope: Scope,
    mode: Mode,
    /// The selected file, kept by path across updates.
    selected: Option<String>,
    history: LoadedHistory,
    diff_scroll: usize,
    /// The diff rows the last paint laid out and the region it showed them in.
    diff_rows: (usize, usize),
    /// The diff rows of `(revision, width, path)`.
    diff_cache: Option<(u64, usize, String, Vec<Line>)>,
}

impl ChangesPanel {
    /// The panel on this turn's changes, over a transcript window that holds `history`.
    pub(crate) fn new(history: LoadedHistory) -> Self {
        Self {
            scope: Scope::Turn,
            mode: Mode::List,
            selected: None,
            history,
            diff_scroll: 0,
            diff_rows: (0, 0),
            diff_cache: None,
        }
    }

    /// The scope's rows and the ones without a check after their last edit (the telemetry's
    /// counts).
    pub(crate) fn counts(&self, ledger: &mut ChangeLedger, chat: &[ChatEntry]) -> (u64, u64) {
        let rows = &ledger.summary(chat, self.scope).rows;
        let untested = rows
            .iter()
            .filter(|row| row.evidence == Evidence::None)
            .count();
        (rows.len() as u64, untested as u64)
    }

    /// One key id.
    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        kb: &KeybindingsManager,
        ledger: &mut ChangeLedger,
        chat: &[ChatEntry],
    ) -> ChangesKey {
        if kb.matches(key, "tui.select.cancel") {
            return ChangesKey::Close;
        }
        let summary = ledger.summary(chat, self.scope);
        let selected = selected_index(summary, self.selected.as_deref());
        match self.mode.clone() {
            Mode::List => {
                if kb.matches(key, "app.modal.back") {
                    return ChangesKey::Close;
                }
                if kb.matches(key, SCOPE_BINDING) {
                    self.scope = match self.scope {
                        Scope::Turn => Scope::Session,
                        Scope::Session => Scope::Turn,
                    };
                    return ChangesKey::Scoped;
                }
                let Some(index) = selected else {
                    return ChangesKey::Handled;
                };
                let rows = &summary.rows;
                if kb.matches(key, "tui.select.up") || kb.matches(key, "tui.select.down") {
                    let next = if kb.matches(key, "tui.select.up") {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(rows.len() - 1)
                    };
                    self.selected = Some(rows[next].path.clone());
                    return ChangesKey::Handled;
                }
                if kb.matches(key, "tui.select.confirm") || kb.matches(key, "app.modal.open") {
                    self.mode = Mode::Diff {
                        path: rows[index].path.clone(),
                    };
                    self.diff_scroll = 0;
                    return ChangesKey::OpenedDiff;
                }
                if kb.matches(key, ATTACH_BINDING) {
                    return ChangesKey::Attach(rows[index].path.clone());
                }
                ChangesKey::Handled
            }
            Mode::Diff { path } => {
                if kb.matches(key, "app.modal.back") {
                    self.mode = Mode::List;
                    return ChangesKey::Handled;
                }
                if kb.matches(key, ATTACH_BINDING) {
                    return ChangesKey::Attach(path);
                }
                let (total, region) = self.diff_rows;
                let last = total.saturating_sub(region);
                let page = region.max(1);
                if kb.matches(key, "tui.select.up") {
                    self.diff_scroll = self.diff_scroll.min(last).saturating_sub(1);
                } else if kb.matches(key, "tui.select.down") {
                    self.diff_scroll = (self.diff_scroll + 1).min(last);
                } else if kb.matches(key, "tui.select.pageUp") {
                    self.diff_scroll = self.diff_scroll.min(last).saturating_sub(page);
                } else if kb.matches(key, "tui.select.pageDown") {
                    self.diff_scroll = (self.diff_scroll + page).min(last);
                }
                ChangesKey::Handled
            }
        }
    }

    /// The pane, at most `viewport_rows` tall. `cwd` is the session's directory: paths inside
    /// it show relative to it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the view's disjoint borrows, passed as they are"
    )]
    pub(crate) fn render(
        &mut self,
        ledger: &mut ChangeLedger,
        chat: &[ChatEntry],
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        cwd: &str,
        viewport_rows: usize,
    ) -> Vec<Line> {
        let revision = ledger.revision();
        let summary = ledger.summary(chat, self.scope);
        let open = match &self.mode {
            Mode::Diff { path } => summary.rows.iter().find(|row| row.path == *path).cloned(),
            Mode::List => None,
        };
        let mut lines = if let Some(row) = open {
            self.render_diff(&row, chat, revision, theme, width, kb, cwd, viewport_rows)
        } else {
            // A diff whose file left the scope (a rebuilt transcript) goes back to the list.
            self.mode = Mode::List;
            self.render_list(summary, theme, width, kb, cwd, viewport_rows)
        };
        lines.truncate(viewport_rows.max(1));
        lines
    }

    /// The text `app.changes.attach` puts in the composer: the file, its counts in the panel's
    /// scope, and those edits' rows as the cards show them, in a `diff` fence.
    pub(crate) fn attachment(
        &self,
        ledger: &mut ChangeLedger,
        chat: &[ChatEntry],
        path: &str,
        cwd: &str,
    ) -> Option<String> {
        let summary = ledger.summary(chat, self.scope);
        let row = summary.rows.iter().find(|row| row.path == path)?;
        let diffs: Vec<String> = row_diffs(chat, row)
            .iter()
            .filter_map(|entry| entry.get("diff").and_then(Value::as_str))
            .map(|diff| diff.trim_end_matches('\n').to_string())
            .collect();
        Some(format!(
            "{}: {}, +{} -{}\n```diff\n{}\n```\n",
            display_path(&row.path, cwd),
            plural(row.edits, "edit"),
            row.added,
            row.removed,
            diffs.join("\n...\n")
        ))
    }

    fn render_list(
        &self,
        summary: &ChangeSummary,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        cwd: &str,
        viewport_rows: usize,
    ) -> Vec<Line> {
        let rows = &summary.rows;
        // The scope reaches back past the loaded rows: the session always, the turn when its
        // prompt is not loaded.
        let partial = self.history == LoadedHistory::Partial
            && match self.scope {
                Scope::Session => true,
                Scope::Turn => !summary.start_loaded,
            };
        let title = match self.scope {
            Scope::Turn => "Changes \u{b7} this turn",
            Scope::Session => "Changes \u{b7} whole session",
        };
        let mut lines = vec![title_row(theme, width, title)];
        let mut subtitle: Vec<String> = Vec::new();
        if !rows.is_empty() {
            let (added, removed) = rows.iter().fold((0, 0), |(added, removed), row| {
                (added + row.added, removed + row.removed)
            });
            subtitle.push(plural(rows.len(), "file"));
            subtitle.push(format!("+{added} -{removed}"));
        }
        if partial {
            subtitle.push("from loaded history".to_string());
        }
        lines.push(truncate_line(
            &vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Muted, subtitle.join(" \u{b7} ")),
            ],
            width,
            ELLIPSIS,
        ));
        lines.push(Vec::new());
        let other_scope = match self.scope {
            Scope::Turn => "session",
            Scope::Session => "turn",
        };
        let footer = if rows.is_empty() {
            let empty = match (self.scope, partial) {
                (Scope::Turn, _) => "No file edits in this turn",
                (Scope::Session, true) => "No file edits in the loaded history",
                (Scope::Session, false) => "No file edits in this session",
            };
            lines.push(empty_state_row(theme, width, kb, "", empty));
            Footer::new()
                .bound(kb, &[SCOPE_BINDING], other_scope, HintRank::Useful)
                .bound(
                    kb,
                    &["app.modal.back", "tui.select.cancel"],
                    "close",
                    HintRank::Core,
                )
        } else {
            let selected = selected_index(summary, self.selected.as_deref()).unwrap_or(0);
            let visible = viewport_rows.saturating_sub(CHROME_ROWS).max(1);
            // A window that cannot show every row spends one on its position.
            let visible = if rows.len() > visible {
                visible.saturating_sub(1).max(1)
            } else {
                visible
            };
            let start = selected
                .saturating_sub(visible / 2)
                .min(rows.len().saturating_sub(visible));
            let end = (start + visible).min(rows.len());
            let columns = Columns::fit(rows, width, cwd);
            for (index, row) in rows[start..end].iter().enumerate() {
                lines.push(columns.row(theme, width, row, cwd, start + index == selected));
            }
            if end - start < rows.len() {
                lines.push(vec![
                    Span::raw("  "),
                    theme.fg_span(
                        ThemeColor::Muted,
                        format!("({}/{})", selected + 1, rows.len()),
                    ),
                ]);
            }
            Footer::new()
                .navigate(kb)
                .bound(kb, &[ATTACH_BINDING], "attach", HintRank::Useful)
                .bound(kb, &[SCOPE_BINDING], other_scope, HintRank::Extra)
                .bound(
                    kb,
                    &["tui.select.confirm", "app.modal.open"],
                    "diff",
                    HintRank::Core,
                )
                .bound(
                    kb,
                    &["app.modal.back", "tui.select.cancel"],
                    "close",
                    HintRank::Core,
                )
        };
        lines.push(Vec::new());
        lines.push(footer.line(theme, width));
        lines.push(Vec::new());
        lines
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the list pane's inputs plus the diff's cache key"
    )]
    fn render_diff(
        &mut self,
        row: &ChangeRow,
        chat: &[ChatEntry],
        revision: u64,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        cwd: &str,
        viewport_rows: usize,
    ) -> Vec<Line> {
        let prefix = "Changes \u{203a} ";
        let path = cut_left(
            display_path(&row.path, cwd),
            width.saturating_sub(2 + str_width(prefix)),
        );
        let mut lines = vec![title_row(theme, width, &format!("{prefix}{path}"))];
        let mut subtitle = vec![
            Span::raw("  "),
            theme.fg_span(ThemeColor::ToolDiffAdded, format!("+{}", row.added)),
            Span::raw(" "),
            theme.fg_span(ThemeColor::ToolDiffRemoved, format!("-{}", row.removed)),
            theme.fg_span(
                ThemeColor::Muted,
                format!(" \u{b7} {} \u{b7} ", plural(row.edits, "edit")),
            ),
        ];
        let (label, color) = evidence_label(row.evidence, Labels::Long);
        subtitle.push(theme.fg_span(color, label));
        lines.push(truncate_line(&subtitle, width, ELLIPSIS));
        lines.push(Vec::new());
        let cached = self
            .diff_cache
            .as_ref()
            .is_some_and(|(at, at_width, at_path, _)| {
                *at == revision && *at_width == width && *at_path == row.path
            });
        if !cached {
            let mut rows = crate::tool_card::layout::RowOutput::paint();
            crate::tool_card::ipython::diffs::render_diffs(
                &mut rows,
                &row_diffs(chat, row),
                /*has_code*/ false,
                /*expanded*/ true,
                theme,
                width,
            );
            self.diff_cache = Some((revision, width, row.path.clone(), rows.into_lines()));
        }
        let diff: &[Line] = self
            .diff_cache
            .as_ref()
            .map_or(&[], |(_, _, _, rows)| rows.as_slice());
        let region = viewport_rows.saturating_sub(CHROME_ROWS).max(1);
        // A diff taller than its region spends one row on its position.
        let region = if diff.len() > region {
            region.saturating_sub(1).max(1)
        } else {
            region
        };
        let start = self.diff_scroll.min(diff.len().saturating_sub(region));
        let end = (start + region).min(diff.len());
        lines.extend(diff[start..end].iter().cloned());
        if end - start < diff.len() {
            lines.push(vec![
                Span::raw("  "),
                theme.fg_span(
                    ThemeColor::Muted,
                    format!("rows {}\u{2013}{end} of {}", start + 1, diff.len()),
                ),
            ]);
        }
        self.diff_rows = (diff.len(), region);
        let footer = Footer::new()
            .bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "scroll",
                HintRank::Useful,
            )
            .bound(kb, &[ATTACH_BINDING], "attach", HintRank::Useful)
            .bound(kb, &["app.modal.back"], "back", HintRank::Useful)
            .esc(kb, EscVerb::Close);
        lines.push(Vec::new());
        lines.push(footer.line(theme, width));
        lines.push(Vec::new());
        lines
    }
}

/// The selected row's index: the kept path, else the first row; `None` with no rows.
fn selected_index(summary: &ChangeSummary, selected: Option<&str>) -> Option<usize> {
    if summary.rows.is_empty() {
        return None;
    }
    Some(
        selected
            .and_then(|path| summary.rows.iter().position(|row| row.path == path))
            .unwrap_or(0),
    )
}

/// `path` relative to `cwd` when it lies inside it.
fn display_path<'a>(path: &'a str, cwd: &str) -> &'a str {
    let cwd = cwd.trim_end_matches(['/', '\\']);
    if cwd.is_empty() {
        return path;
    }
    path.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix(['/', '\\']))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
}

/// `text` cut from the left to `width` columns behind the shared mark, so a path keeps its
/// file name.
fn cut_left(text: &str, width: usize) -> String {
    if str_width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(str_width(ELLIPSIS));
    if budget == 0 {
        return ELLIPSIS.chars().take(width).collect();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        let cells = grapheme_width(grapheme);
        if used + cells > budget {
            break;
        }
        used += cells;
        kept.push(grapheme);
    }
    kept.reverse();
    format!("{ELLIPSIS}{}", kept.concat())
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Labels {
    Long,
    Short,
}

/// The evidence cell's words and colour.
fn evidence_label(evidence: Evidence, labels: Labels) -> (&'static str, ThemeColor) {
    let (long, short, color) = match evidence {
        Evidence::After(Check {
            kind: CheckKind::Test,
            passed: true,
        }) => (
            "tests ran after \u{2713}",
            "tests \u{2713}",
            ThemeColor::Success,
        ),
        Evidence::After(Check {
            kind: CheckKind::Test,
            passed: false,
        }) => (
            "tests failed after \u{2717}",
            "tests \u{2717}",
            ThemeColor::Error,
        ),
        Evidence::After(Check {
            kind: CheckKind::Build,
            passed: true,
        }) => (
            "built after \u{2713}",
            "built \u{2713}",
            ThemeColor::Success,
        ),
        Evidence::After(Check {
            kind: CheckKind::Build,
            passed: false,
        }) => (
            "build failed after \u{2717}",
            "build \u{2717}",
            ThemeColor::Error,
        ),
        Evidence::None => ("no test after", "untested", ThemeColor::Warning),
    };
    match labels {
        Labels::Long => (long, color),
        Labels::Short => (short, color),
    }
}

/// The list's columns, fitted once per paint over every row so the window keeps them aligned:
/// the path takes what the counts, the edits and the evidence leave; on a narrow pane the edits
/// column drops first, then the evidence shortens.
struct Columns {
    path: usize,
    added: usize,
    removed: usize,
    /// `None` when the column dropped.
    edits: Option<usize>,
    evidence: usize,
    labels: Labels,
}

impl Columns {
    fn fit(rows: &[ChangeRow], width: usize, cwd: &str) -> Self {
        let widest = |cell: &dyn Fn(&ChangeRow) -> usize| rows.iter().map(cell).max().unwrap_or(0);
        let added = widest(&|row| str_width(&format!("+{}", row.added)));
        let removed = widest(&|row| str_width(&format!("-{}", row.removed)));
        let edits = widest(&|row| str_width(&plural(row.edits, "edit")));
        let evidence = |labels| widest(&|row| str_width(evidence_label(row.evidence, labels).0));
        let path = widest(&|row| str_width(display_path(&row.path, cwd)));
        // Marker, then two-cell gaps around the counts, one between them.
        let counts = 2 + added + 1 + removed;
        let room = |edits: Option<usize>, evidence: usize| {
            width.saturating_sub(2 + counts + edits.map_or(0, |edits| 2 + edits) + 2 + evidence)
        };
        let fits = |edits, evidence| room(edits, evidence) >= MIN_PATH_WIDTH.min(path);
        let (edits, labels) = if fits(Some(edits), evidence(Labels::Long)) {
            (Some(edits), Labels::Long)
        } else if fits(None, evidence(Labels::Long)) {
            (None, Labels::Long)
        } else {
            (None, Labels::Short)
        };
        let evidence = evidence(labels);
        Self {
            path: room(edits, evidence).min(path).max(1),
            added,
            removed,
            edits,
            evidence,
            labels,
        }
    }

    fn row(&self, theme: &Theme, width: usize, row: &ChangeRow, cwd: &str, selected: bool) -> Line {
        let path = cut_left(display_path(&row.path, cwd), self.path);
        let mut path = Span::raw(pad_cell(&path, self.path));
        if selected {
            path = theme.bold(path);
        }
        let added = format!("+{}", row.added);
        let removed = format!("-{}", row.removed);
        let mut line: Line = vec![
            Span::raw(if selected { "\u{203a} " } else { "  " }),
            path,
            Span::raw(" ".repeat(2 + self.added - str_width(&added))),
            theme.fg_span(ThemeColor::ToolDiffAdded, added),
            Span::raw(" ".repeat(1 + self.removed - str_width(&removed))),
            theme.fg_span(ThemeColor::ToolDiffRemoved, removed),
        ];
        if let Some(edits) = self.edits {
            line.push(Span::raw("  "));
            line.push(theme.fg_span(
                ThemeColor::Muted,
                pad_cell(&plural(row.edits, "edit"), edits),
            ));
        }
        let (label, color) = evidence_label(row.evidence, self.labels);
        line.push(Span::raw("  "));
        line.push(theme.fg_span(color, pad_cell(label, self.evidence)));
        finish_menu_row(theme, &line, width, selected)
    }
}

/// The file's diffs in the order its cells made them, as the cards carry them.
fn row_diffs(chat: &[ChatEntry], row: &ChangeRow) -> Vec<Value> {
    row.entries
        .iter()
        .filter_map(|index| match chat.get(*index) {
            Some(ChatEntry::Tool(card)) => card.result.as_ref(),
            Some(_) | None => None,
        })
        .filter_map(|result| result.details.get("diffs").and_then(Value::as_array))
        .flatten()
        .filter(|entry| entry.get("path").and_then(Value::as_str) == Some(row.path.as_str()))
        .filter(|entry| entry.get("diff").and_then(Value::as_str).is_some())
        .cloned()
        .collect()
}

/// Hand one panel action's report to the telemetry seam and return at once: the key path never
/// waits on the delivery. `files` and `untested` are the scope's rows and the ones with no check
/// after their last edit.
pub(crate) fn report_used(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    action: &'static str,
    files: u64,
    untested: u64,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry
                .session_changes_used(action, files, untested)
                .await;
        });
    }
}

#[cfg(test)]
mod tests;
