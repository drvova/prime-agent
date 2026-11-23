//! The todo tray: the session todo list (`todo_update` events, the attach snapshot's
//! `state.todos`) docked above the composer while it has unfinished items — a `Todos done/total`
//! header and one row per item with its status glyph, the in-progress items lit.
//!
//! Folding (oh-my-pi's collapsed todo window, with Prime's toggle key):
//! - collapsed (the default), a list longer than [`COLLAPSED_ROWS`] shows a window around the
//!   current item — the item before it for context, the current one, the ones after — and one
//!   `+N more` row counting the rest; the header names the toggle key (`app.todos.toggle`);
//! - expanded, every item shows up to [`EXPANDED_ROWS`], then `+N more`;
//! - a finished list (every item done) is one summary row; the session UI drops it when the
//!   user starts the next turn.
//!
//! Shrink order, applied as the width runs out: notes drop (below [`NOTE_MIN_WIDTH`] columns, or
//! when they would not fit), item text truncates with an ellipsis, and below
//! [`ITEM_ROWS_MIN_WIDTH`] columns only the header row remains, naming the current item.

use pa_types::todo::{TodoItem, TodoList, TodoStatus};

use crate::menu_chrome::{CHROME_INDENT, ELLIPSIS};
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line};
use crate::{Line, Span};

/// The most item rows the collapsed tray shows.
pub(crate) const COLLAPSED_ROWS: usize = 5;
/// The most item rows the expanded tray shows.
pub(crate) const EXPANDED_ROWS: usize = 20;
/// Narrower than this, notes drop.
pub(crate) const NOTE_MIN_WIDTH: usize = 60;
/// Narrower than this, only the header row shows.
pub(crate) const ITEM_ROWS_MIN_WIDTH: usize = 24;

/// Whether the tray shows the window around the current item or the whole list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TodoFold {
    #[default]
    Collapsed,
    Expanded,
}

impl TodoFold {
    #[must_use]
    pub(crate) fn toggled(self) -> Self {
        match self {
            TodoFold::Collapsed => TodoFold::Expanded,
            TodoFold::Expanded => TodoFold::Collapsed,
        }
    }

    fn row_budget(self) -> usize {
        match self {
            TodoFold::Collapsed => COLLAPSED_ROWS,
            TodoFold::Expanded => EXPANDED_ROWS,
        }
    }
}

/// The tray rows for `list` at `width` (none for an empty list). `toggle_key` is the
/// `app.todos.toggle` key text the header offers when folding changes what shows.
#[must_use]
pub(crate) fn render_todo_panel(
    list: &TodoList,
    fold: TodoFold,
    toggle_key: &str,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    if list.items.is_empty() || width == 0 {
        return Vec::new();
    }
    let done = list.done_count();
    let total = list.items.len();
    if done == total {
        let row = vec![
            Span::raw(CHROME_INDENT),
            theme.fg_span(ThemeColor::Success, "\u{2713} "),
            theme.bold(theme.fg_span(ThemeColor::Success, "Todos")),
            theme.fg_span(ThemeColor::Success, format!(" {done}/{total}")),
            theme.fg_span(ThemeColor::Dim, " \u{b7} all done"),
        ];
        return vec![truncate_line(&row, width, ELLIPSIS)];
    }
    let show_items = width >= ITEM_ROWS_MIN_WIDTH;
    let (start, end) = window(list, fold.row_budget());
    let foldable = total > COLLAPSED_ROWS;

    let mut header = vec![
        Span::raw(CHROME_INDENT),
        theme.bold(theme.fg_span(ThemeColor::Accent, "Todos")),
        theme.fg_span(ThemeColor::Accent, format!(" {done}/{total}")),
    ];
    if !show_items {
        if let Some(active) = list.active() {
            header.push(theme.fg_span(ThemeColor::Dim, " \u{b7} "));
            header.push(Span::raw(active.text.clone()));
        }
    } else if foldable && !toggle_key.is_empty() {
        let action = match fold {
            TodoFold::Collapsed => "expand",
            TodoFold::Expanded => "collapse",
        };
        header.push(theme.fg_span(ThemeColor::Dim, format!(" \u{b7} {toggle_key} {action}")));
    }
    let mut lines = vec![truncate_line(&header, width, ELLIPSIS)];
    if !show_items {
        return lines;
    }

    let show_notes = width >= NOTE_MIN_WIDTH;
    for item in &list.items[start..end] {
        lines.push(item_row(item, show_notes, theme, width));
    }
    let hidden = total - (end - start);
    if hidden > 0 {
        lines.push(truncate_line(
            &vec![
                Span::raw(CHROME_INDENT),
                theme.fg_span(ThemeColor::Dim, format!("  +{hidden} more")),
            ],
            width,
            ELLIPSIS,
        ));
    }
    lines
}

/// The `[start, end)` item window: the whole list when it fits `budget`, else `budget` items
/// starting one before the current item (the first in-progress item, else the first pending one),
/// pulled back so the window ends at the list's end at most.
fn window(list: &TodoList, budget: usize) -> (usize, usize) {
    let total = list.items.len();
    if total <= budget {
        return (0, total);
    }
    let focus = list
        .items
        .iter()
        .position(|item| item.status == TodoStatus::InProgress)
        .or_else(|| {
            list.items
                .iter()
                .position(|item| item.status == TodoStatus::Pending)
        })
        .unwrap_or(0);
    let start = focus.saturating_sub(1).min(total - budget);
    (start, start + budget)
}

fn item_row(item: &TodoItem, show_notes: bool, theme: &Theme, width: usize) -> Line {
    let text = match item.status {
        TodoStatus::Done => theme.fg_span(ThemeColor::Dim, item.text.clone()),
        TodoStatus::InProgress => theme.bold(theme.fg_span(ThemeColor::Text, item.text.clone())),
        TodoStatus::Pending => theme.fg_span(ThemeColor::Muted, item.text.clone()),
    };
    let (mark, color) = match item.status {
        TodoStatus::Done => ("\u{2713}", ThemeColor::Success),
        TodoStatus::InProgress => ("\u{25b8}", ThemeColor::Accent),
        TodoStatus::Pending => ("\u{25cb}", ThemeColor::Dim),
    };
    let mut row = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(color, mark),
        Span::raw(" "),
        text,
    ];
    if let Some(note) = item.note.as_deref().filter(|_| show_notes) {
        let used: usize = row.iter().map(|span| str_width(&span.content)).sum();
        // A note shows only whole enough to read: never at the item text's expense.
        if used + 3 + str_width(note).min(12) <= width {
            row.push(theme.fg_span(ThemeColor::Dim, format!(" \u{b7} {note}")));
        }
    }
    truncate_line(&row, width, ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn item(id: &str, text: &str, status: TodoStatus, note: Option<&str>) -> TodoItem {
        TodoItem {
            id: id.to_string(),
            text: text.to_string(),
            status,
            note: note.map(str::to_string),
        }
    }

    fn list() -> TodoList {
        TodoList {
            items: vec![
                item("1", "Read the session engine", TodoStatus::Done, None),
                item(
                    "2",
                    "Add the todo host requests",
                    TodoStatus::Done,
                    Some("pa-core"),
                ),
                item(
                    "3",
                    "Wire the daemon surface",
                    TodoStatus::InProgress,
                    Some("attach caps"),
                ),
                item("4", "Render the tray", TodoStatus::Pending, None),
                item("5", "Write the kernel e2e test", TodoStatus::Pending, None),
            ],
        }
    }

    fn long() -> TodoList {
        let mut long = list();
        for index in 6..=12 {
            long.items.push(item(
                &index.to_string(),
                &format!("step {index}"),
                TodoStatus::Pending,
                None,
            ));
        }
        long
    }

    fn rows(list: &TodoList, fold: TodoFold, width: usize) -> Vec<String> {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        render_todo_panel(list, fold, "Alt+T", &theme, width)
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect()
    }

    #[test]
    fn a_short_list_shows_every_item_with_its_glyph_and_note() {
        assert_eq!(
            rows(&list(), TodoFold::Collapsed, 80),
            vec![
                "  Todos 2/5",
                "  \u{2713} Read the session engine",
                "  \u{2713} Add the todo host requests \u{b7} pa-core",
                "  \u{25b8} Wire the daemon surface \u{b7} attach caps",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e test",
            ]
        );
    }

    /// The in-progress item is lit: accent glyph, bold text; done items dim.
    #[test]
    fn the_current_item_is_highlighted() {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let tray = render_todo_panel(&list(), TodoFold::Collapsed, "Alt+T", &theme, 80);
        let current = &tray[3];
        assert_eq!(current[1].style, theme.fg_style(ThemeColor::Accent));
        assert!(current[3]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert!(!tray[4][3]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn a_narrow_tray_drops_notes_and_truncates_then_keeps_only_the_header() {
        assert_eq!(
            rows(&list(), TodoFold::Collapsed, 28),
            vec![
                "  Todos 2/5",
                "  \u{2713} Read the session engine",
                "  \u{2713} Add the todo host reque\u{2026}",
                "  \u{25b8} Wire the daemon surface",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e te\u{2026}",
            ]
        );
        assert_eq!(
            rows(&list(), TodoFold::Collapsed, 20),
            vec!["  Todos 2/5 \u{b7} Wire \u{2026}"]
        );
    }

    /// A long list folds to the window around the current item (one done item before it for
    /// context) plus `+N more`; expanding shows every item.
    #[test]
    fn a_long_list_folds_around_the_current_item_and_expands() {
        assert_eq!(
            rows(&long(), TodoFold::Collapsed, 80),
            vec![
                "  Todos 2/12 \u{b7} Alt+T expand",
                "  \u{2713} Add the todo host requests \u{b7} pa-core",
                "  \u{25b8} Wire the daemon surface \u{b7} attach caps",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e test",
                "  \u{25cb} step 6",
                "    +7 more",
            ]
        );
        let expanded = rows(&long(), TodoFold::Expanded, 80);
        assert_eq!(expanded.len(), 13);
        assert_eq!(expanded[0], "  Todos 2/12 \u{b7} Alt+T collapse");
        assert_eq!(expanded[12], "  \u{25cb} step 12");
        // Near the end the window stays full rather than running past the list.
        let mut late = long();
        for item in &mut late.items[..11] {
            item.status = TodoStatus::Done;
        }
        late.items[11].status = TodoStatus::InProgress;
        assert_eq!(
            rows(&late, TodoFold::Collapsed, 40)[1..],
            [
                "  \u{2713} step 8",
                "  \u{2713} step 9",
                "  \u{2713} step 10",
                "  \u{2713} step 11",
                "  \u{25b8} step 12",
                "    +7 more",
            ]
        );
    }

    #[test]
    fn a_finished_list_is_one_summary_row_and_an_empty_one_nothing() {
        let mut finished = list();
        for item in &mut finished.items {
            item.status = TodoStatus::Done;
        }
        assert_eq!(
            rows(&finished, TodoFold::Collapsed, 80),
            vec!["  \u{2713} Todos 5/5 \u{b7} all done"]
        );
        assert_eq!(
            rows(&TodoList::default(), TodoFold::Collapsed, 80),
            Vec::<String>::new()
        );
    }
}
