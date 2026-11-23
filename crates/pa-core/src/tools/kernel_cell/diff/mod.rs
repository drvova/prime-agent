//! Numbered cell-edit display, using the existing jsdiff-compatible line algorithm.
//!
//! What a cell's diffs hold is bounded: the rows go into the cell's
//! `details.diffs`, which the session file keeps and every attached client
//! receives and renders. One edit keeps [`EDIT_BUDGET`] and one cell
//! [`CELL_BUDGET`]; a diff past either ends in a [`DiffCut`] row that counts
//! the changed lines it left out, so a client counting rows shows the true
//! `+A -R`. The limits keep whole a new file of 4,000 lines and the rewrite
//! of one at the workspace's size guidance (2,000 lines): in this
//! repository's history 99% of the files a commit changed changed fewer than
//! 1,700 lines, and a numbered row of its code averages under 50 bytes.

use std::fmt::Write as _;

use pa_types::ai::DiffCut;

mod lines;

pub(crate) use lines::diff_lines;
use lines::DiffPart;

/// The rows, and the bytes of those rows (with their newlines), a diff may
/// still keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DiffBudget {
    pub(super) rows: usize,
    pub(super) bytes: usize,
}

/// What one edit's diff keeps at most.
const EDIT_BUDGET: DiffBudget = DiffBudget {
    rows: 4_000,
    bytes: 256 * 1024,
};

/// What all of one cell's diffs keep at most.
pub(super) const CELL_BUDGET: DiffBudget = DiffBudget {
    rows: 8_000,
    bytes: 512 * 1024,
};

/// What a diff row says of its line.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Added,
    Removed,
    Context,
}

/// A diff's rows as they are kept: each row while the budget holds, then only
/// the count of the changed rows that did not fit.
struct Rows {
    text: String,
    left: DiffBudget,
    width: usize,
    cut: Option<DiffCut>,
}

impl Rows {
    /// One row of `kind`: `number` and `text`, or the skip row (`...`) for
    /// `None`.
    fn push(&mut self, kind: Kind, number: Option<usize>, text: &str) {
        if self.cut.is_none() {
            let prefix = match kind {
                Kind::Added => '+',
                Kind::Removed => '-',
                Kind::Context => ' ',
            };
            let width = self.width;
            let separator = usize::from(!self.text.is_empty());
            let bytes = separator
                + match number {
                    Some(_) => 1 + width + 1 + text.len(),
                    None => 1 + width + 4,
                };
            if self.left.rows > 0 && bytes <= self.left.bytes {
                self.left.rows -= 1;
                self.left.bytes -= bytes;
                if separator == 1 {
                    self.text.push('\n');
                }
                // Writing to a string cannot fail.
                let _ = match number {
                    Some(number) => write!(self.text, "{prefix}{number:>width$} {text}"),
                    None => write!(self.text, "{prefix}{:>width$} ...", ""),
                };
                return;
            }
            self.cut = Some(DiffCut {
                added: 0,
                removed: 0,
            });
        }
        if let Some(cut) = &mut self.cut {
            match kind {
                Kind::Added => cut.added += 1,
                Kind::Removed => cut.removed += 1,
                Kind::Context => {}
            }
        }
    }
}

/// Generate the cell edit display with line numbers and bounded context,
/// within what `budget` (the cell's) still holds; what it keeps is taken off
/// `budget`.
///
/// Only the lines between the texts' common first and last lines are
/// diffed: of those common lines only the context shows. A common head is
/// what the line diff matches first anyway; a common tail can move a change
/// whose place is ambiguous (a repeated line) to the tail's other end.
pub(super) fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    start_line: usize,
    budget: &mut DiffBudget,
) -> String {
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = start_line.saturating_sub(1) + old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    // The last piece of a split is a line without its newline, or the empty
    // end after the last newline, so the head compares only whole lines.
    let shorter = old_lines.len().min(new_lines.len());
    let head = old_lines
        .iter()
        .zip(&new_lines)
        .take(shorter - 1)
        .take_while(|(old, new)| old == new)
        .count();
    let tail = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(shorter - head)
        .take_while(|(old, new)| old == new)
        .count();
    let head_bytes: usize = old_lines[..head].iter().map(|line| line.len() + 1).sum();
    let tail_lines = &old_lines[old_lines.len() - tail..];
    let tail_bytes =
        tail_lines.iter().map(|line| line.len()).sum::<usize>() + tail.saturating_sub(1);
    // One common line more than the context keeps the row that skips the rest.
    let head_kept = head.min(context_lines + 1);
    let mut parts = Vec::new();
    if head_kept > 0 {
        let kept_bytes: usize = old_lines[head - head_kept..head]
            .iter()
            .map(|line| line.len() + 1)
            .sum();
        parts.push(DiffPart {
            value: old_content[head_bytes - kept_bytes..head_bytes].to_string(),
            added: false,
            removed: false,
        });
    }
    parts.extend(diff_lines(
        &old_content[head_bytes..old_content.len() - tail_bytes],
        &new_content[head_bytes..new_content.len() - tail_bytes],
    ));
    let tail_kept = tail_lines[..tail.min(context_lines + 1)].join("\n");
    if !tail_kept.is_empty() {
        parts.push(DiffPart {
            value: tail_kept,
            added: false,
            removed: false,
        });
    }

    let allowed = DiffBudget {
        rows: EDIT_BUDGET.rows.min(budget.rows),
        bytes: EDIT_BUDGET.bytes.min(budget.bytes),
    };
    let mut output = Rows {
        text: String::new(),
        left: allowed,
        width: line_num_width,
        cut: None,
    };
    let mut old_line_num = start_line + head - head_kept;
    let mut new_line_num = old_line_num;
    let mut last_was_change = false;

    for (i, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            for line in &raw {
                if part.added {
                    output.push(Kind::Added, Some(new_line_num), line);
                    new_line_num += 1;
                } else {
                    output.push(Kind::Removed, Some(old_line_num), line);
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change =
                i + 1 < parts.len() && (parts[i + 1].added || parts[i + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in &raw {
                        output.push(Kind::Context, Some(old_line_num), line);
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading_lines = &raw[..context_lines];
                    let trailing_lines = &raw[raw.len() - context_lines..];
                    let skipped_lines = raw.len() - leading_lines.len() - trailing_lines.len();

                    for line in leading_lines {
                        output.push(Kind::Context, Some(old_line_num), line);
                        old_line_num += 1;
                        new_line_num += 1;
                    }

                    output.push(Kind::Context, None, "");
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;

                    for line in trailing_lines {
                        output.push(Kind::Context, Some(old_line_num), line);
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown_lines = &raw[..context_lines.min(raw.len())];
                let skipped_lines = raw.len() - shown_lines.len();

                for line in shown_lines {
                    output.push(Kind::Context, Some(old_line_num), line);
                    old_line_num += 1;
                    new_line_num += 1;
                }

                if skipped_lines > 0 {
                    output.push(Kind::Context, None, "");
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }
            } else if has_trailing_change {
                let skipped_lines = raw.len().saturating_sub(context_lines);
                if skipped_lines > 0 {
                    output.push(Kind::Context, None, "");
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }

                for line in raw.iter().skip(skipped_lines) {
                    output.push(Kind::Context, Some(old_line_num), line);
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    let Rows {
        mut text,
        left,
        cut,
        ..
    } = output;
    budget.rows -= allowed.rows - left.rows;
    budget.bytes -= allowed.bytes - left.bytes;
    if let Some(cut) = cut {
        if !text.is_empty() {
            text.push('\n');
        }
        // Writing to a string cannot fail.
        let _ = write!(text, "{cut}");
    }
    text
}
