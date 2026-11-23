//! The transcript's click gestures: a left press starts a drag selection, a second press on the
//! same cell inside the multi-click window selects the word under it, a third the whole row. The
//! word and row spans are read from the frame's plain text; the copy on release is the drag's.

use std::time::{Duration, Instant};

/// How long after a press the next press on the same cell still counts as the same gesture
/// (the common desktop double-click interval).
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// What a left press selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionKind {
    /// A single press: the drag from here selects.
    Drag,
    /// A double press: the word under the pointer.
    Word,
    /// A triple press: the whole row under the pointer.
    Line,
}

impl SelectionKind {
    /// The `tui mouse selection` event's `kind`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            SelectionKind::Drag => "drag",
            SelectionKind::Word => "word",
            SelectionKind::Line => "line",
        }
    }
}

/// Counts consecutive left presses on one cell: each press inside the window on the cell (or a
/// neighbouring column, for a hand that moves a little) steps drag -> word -> line and back to a
/// drag, any other press starts over.
#[derive(Debug, Default)]
pub(crate) struct ClickTracker {
    /// The last press: when, where, and what it selected.
    last: Option<(Instant, usize, usize, SelectionKind)>,
}

impl ClickTracker {
    /// Record one left press at `(row, col)` and say what it selects.
    pub(crate) fn press(&mut self, row: usize, col: usize, now: Instant) -> SelectionKind {
        let previous = self
            .last
            .filter(|(at, last_row, last_col, _)| {
                *last_row == row
                    && last_col.abs_diff(col) <= 1
                    && now.saturating_duration_since(*at) <= MULTI_CLICK_WINDOW
            })
            .map(|(.., kind)| kind);
        let kind = match previous {
            None | Some(SelectionKind::Line) => SelectionKind::Drag,
            Some(SelectionKind::Drag) => SelectionKind::Word,
            Some(SelectionKind::Word) => SelectionKind::Line,
        };
        self.last = Some((now, row, col, kind));
        kind
    }
}

/// Whether a character belongs to a word a double click selects: letters, digits, and the
/// punctuation paths, identifiers and URLs are made of, so `src/main.rs` or `a_b-c` select whole.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || "_-./~:@%+#=?&".contains(c)
}

/// The column span `[from, to)` of the word covering column `col` of a plain row, without the
/// sentence punctuation that ends it (`file.rs.` selects `file.rs`); `None` over blank cells.
pub(crate) fn word_span(text: &str, col: usize) -> Option<(usize, usize)> {
    let cells: Vec<(usize, usize, char)> = text
        .chars()
        .scan(0usize, |at, c| {
            let start = *at;
            *at += crate::width::char_width(c);
            Some((start, *at, c))
        })
        .collect();
    let hit = cells
        .iter()
        .position(|(start, end, _)| (*start..(*end).max(start + 1)).contains(&col))?;
    if !word_char(cells[hit].2) {
        return None;
    }
    let first = cells[..hit]
        .iter()
        .rposition(|(_, _, c)| !word_char(*c))
        .map_or(0, |index| index + 1);
    let mut last = cells[hit..]
        .iter()
        .position(|(_, _, c)| !word_char(*c))
        .map_or(cells.len(), |index| hit + index);
    while last > hit + 1 && ".:?".contains(cells[last - 1].2) {
        last -= 1;
    }
    Some((cells[first].0, cells[last - 1].1))
}

/// The column span of a row's visible text: from its first to its last non-blank cell.
pub(crate) fn line_span(text: &str) -> Option<(usize, usize)> {
    let mut from = None;
    let mut to = 0usize;
    let mut at = 0usize;
    for c in text.chars() {
        let start = at;
        at += crate::width::char_width(c);
        if !c.is_whitespace() && at > start {
            from.get_or_insert(start);
            to = at;
        }
    }
    from.map(|from| (from, to))
}

/// Hand a selection copy's kind to the telemetry seam and return at once: the mouse path never
/// waits on the delivery.
pub(crate) fn report_selection(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    kind: SelectionKind,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.mouse_selection_copied(kind.wire_name()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Presses on one cell inside the window step drag, word, line, then start over; a press
    /// elsewhere or after the window is a fresh drag.
    #[test]
    fn presses_step_drag_word_line_and_start_over() {
        let mut clicks = ClickTracker::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(
            [
                clicks.press(3, 10, t0),
                clicks.press(3, 11, t0 + ms(150)),
                clicks.press(3, 10, t0 + ms(300)),
                clicks.press(3, 10, t0 + ms(450)),
                clicks.press(4, 10, t0 + ms(500)),
                clicks.press(4, 10, t0 + ms(1_000)),
            ],
            [
                SelectionKind::Drag,
                SelectionKind::Word,
                SelectionKind::Line,
                SelectionKind::Drag,
                SelectionKind::Drag,
                SelectionKind::Drag,
            ]
        );
    }

    /// A double click takes the path or identifier under it whole, leaves the sentence's
    /// closing punctuation out, and selects nothing over a blank cell.
    #[test]
    fn a_word_is_the_path_or_identifier_under_the_pointer() {
        let row = "  see src/main.rs. then run_all(now)";
        assert_eq!(
            [
                word_span(row, 10),
                word_span(row, 27),
                word_span(row, 1),
                line_span(row),
            ],
            [Some((6, 17)), Some((24, 31)), None, Some((2, 36))]
        );
    }
}
