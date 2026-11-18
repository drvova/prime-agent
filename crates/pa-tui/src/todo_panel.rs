//! The todo tray: the session todo list (`todo_update` events, the attach snapshot's
//! `state.todos`) rendered as a compact block above the composer — a `Todo done/total` header and
//! one row per item with its status glyph.
//!
//! Shrink order, applied as the width or the row budget runs out:
//! 1. notes drop (below [`NOTE_MIN_WIDTH`] columns, or when they would not fit);
//! 2. item text truncates with an ellipsis;
//! 3. done items fold into the header (below [`DONE_ROWS_MIN_WIDTH`] columns, or when more than
//!    [`MAX_ITEM_ROWS`] items would show), then the rest beyond the budget fold into `+N more`;
//! 4. below [`ITEM_ROWS_MIN_WIDTH`] columns only the header row remains.
//!
//! A finished list (every item done) collapses to its header row.

use pa_types::todo::{TodoItem, TodoList, TodoStatus};

use crate::menu_chrome::CHROME_INDENT;
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line};
use crate::{Line, Span};

/// The most item rows the tray shows.
pub(crate) const MAX_ITEM_ROWS: usize = 6;
/// Narrower than this, notes drop.
pub(crate) const NOTE_MIN_WIDTH: usize = 60;
/// Narrower than this, done items fold into the header.
pub(crate) const DONE_ROWS_MIN_WIDTH: usize = 40;
/// Narrower than this, only the header row shows.
pub(crate) const ITEM_ROWS_MIN_WIDTH: usize = 24;

const ELLIPSIS: &str = "\u{2026}";

/// The tray rows for `list` at `width` (none for an empty list).
#[must_use]
pub(crate) fn render_todo_panel(list: &TodoList, theme: &Theme, width: usize) -> Vec<Line> {
    if list.items.is_empty() || width == 0 {
        return Vec::new();
    }
    let done = list.done_count();
    let total = list.items.len();
    let finished = done == total;
    let show_items = !finished && width >= ITEM_ROWS_MIN_WIDTH;
    let hide_done = show_items && (width < DONE_ROWS_MIN_WIDTH || total > MAX_ITEM_ROWS);
    let shown: Vec<&TodoItem> = if show_items {
        list.items
            .iter()
            .filter(|item| !(hide_done && item.status == TodoStatus::Done))
            .collect()
    } else {
        Vec::new()
    };
    let (shown, more) = if shown.len() > MAX_ITEM_ROWS {
        // The budget keeps one row for the `+N more` line.
        (
            &shown[..MAX_ITEM_ROWS - 1],
            shown.len() - (MAX_ITEM_ROWS - 1),
        )
    } else {
        (&shown[..], 0)
    };

    let mut header = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(
            if finished {
                ThemeColor::Success
            } else {
                ThemeColor::Accent
            },
            format!("Todo {done}/{total}"),
        ),
    ];
    if finished {
        header.push(theme.fg_span(ThemeColor::Dim, " \u{b7} all done"));
    } else if hide_done && done > 0 {
        header.push(theme.fg_span(ThemeColor::Dim, format!(" \u{b7} {done} done hidden")));
    } else if !show_items {
        if let Some(active) = list.active() {
            header.push(theme.fg_span(ThemeColor::Dim, " \u{b7} "));
            header.push(Span::raw(active.text.clone()));
        }
    }
    let mut lines = vec![truncate_line(&header, width, ELLIPSIS)];

    let show_notes = width >= NOTE_MIN_WIDTH;
    for item in shown {
        // (glyph, glyph colour, text colour)
        let (mark, color, text_color) = match item.status {
            TodoStatus::Done => ("\u{2713}", ThemeColor::Success, ThemeColor::Dim),
            TodoStatus::InProgress => ("\u{25b8}", ThemeColor::Accent, ThemeColor::Text),
            TodoStatus::Pending => ("\u{25cb}", ThemeColor::Dim, ThemeColor::Muted),
        };
        let mut row = vec![
            Span::raw(CHROME_INDENT),
            theme.fg_span(color, mark),
            Span::raw(" "),
            theme.fg_span(text_color, item.text.clone()),
        ];
        if let Some(note) = item.note.as_deref().filter(|_| show_notes) {
            let used: usize = row.iter().map(|span| str_width(&span.content)).sum();
            // A note shows only whole enough to read: never at the item text's expense.
            if used + 3 + str_width(note).min(12) <= width {
                row.push(theme.fg_span(ThemeColor::Dim, format!(" \u{b7} {note}")));
            }
        }
        lines.push(truncate_line(&row, width, ELLIPSIS));
    }
    if more > 0 {
        lines.push(truncate_line(
            &vec![
                Span::raw(CHROME_INDENT),
                theme.fg_span(ThemeColor::Dim, format!("  +{more} more")),
            ],
            width,
            ELLIPSIS,
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn list() -> TodoList {
        let item = |id: &str, text: &str, status, note: Option<&str>| TodoItem {
            id: id.to_string(),
            text: text.to_string(),
            status,
            note: note.map(str::to_string),
        };
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

    fn rows(list: &TodoList, width: usize) -> Vec<String> {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        render_todo_panel(list, &theme, width)
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect()
    }

    #[test]
    fn a_wide_tray_shows_every_item_with_its_glyph_and_note() {
        assert_eq!(
            rows(&list(), 80),
            vec![
                "  Todo 2/5",
                "  \u{2713} Read the session engine",
                "  \u{2713} Add the todo host requests \u{b7} pa-core",
                "  \u{25b8} Wire the daemon surface \u{b7} attach caps",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e test",
            ]
        );
    }

    #[test]
    fn a_narrow_tray_drops_notes_folds_done_items_and_truncates() {
        assert_eq!(
            rows(&list(), 28),
            vec![
                "  Todo 2/5 \u{b7} 2 done hidden",
                "  \u{25b8} Wire the daemon surface",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e te\u{2026}",
            ]
        );
        assert_eq!(rows(&list(), 20), vec!["  Todo 2/5 \u{b7} Wire t\u{2026}"]);
    }

    #[test]
    fn long_and_finished_lists_fold() {
        let mut long = list();
        for index in 6..=12 {
            long.items.push(TodoItem {
                id: index.to_string(),
                text: format!("step {index}"),
                status: TodoStatus::Pending,
                note: None,
            });
        }
        assert_eq!(
            rows(&long, 80),
            vec![
                "  Todo 2/12 \u{b7} 2 done hidden",
                "  \u{25b8} Wire the daemon surface \u{b7} attach caps",
                "  \u{25cb} Render the tray",
                "  \u{25cb} Write the kernel e2e test",
                "  \u{25cb} step 6",
                "  \u{25cb} step 7",
                "    +5 more",
            ]
        );
        let mut finished = list();
        for item in &mut finished.items {
            item.status = TodoStatus::Done;
        }
        assert_eq!(rows(&finished, 80), vec!["  Todo 5/5 \u{b7} all done"]);
        assert_eq!(rows(&TodoList::default(), 80), Vec::<String>::new());
    }
}
