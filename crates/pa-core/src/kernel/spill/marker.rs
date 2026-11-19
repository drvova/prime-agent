//! The transcript line a spill leaves where its output was cut.

use std::fmt::Write as _;

use super::{SpillHandle, SpillOutcome, HANDLE_PREFIX};

/// `n` with comma thousands separators (`48,211`).
pub(crate) fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

impl SpillHandle {
    /// The handle as the model reads it, in place of a truncation marker:
    /// `[out:<id> · N chars · L lines · M chars not shown]`, naming a partial
    /// store or a store that did not take it.
    pub(crate) fn marker(&self, not_shown: usize) -> String {
        let mut marker = format!(
            "[{HANDLE_PREFIX}{} · {} chars · {} lines · {} chars not shown",
            self.id,
            group_digits(self.total_chars),
            group_digits(self.lines),
            group_digits(not_shown),
        );
        match &self.outcome {
            SpillOutcome::Queued if self.stored_chars < self.total_chars => {
                let _ = write!(
                    marker,
                    " · first {} chars stored",
                    group_digits(self.stored_chars)
                );
            }
            SpillOutcome::Queued => {}
            SpillOutcome::NotStored(reason) => {
                let _ = write!(marker, " · not stored: {reason}");
            }
        }
        marker.push(']');
        marker
    }

    /// The handle a thinned request-view result carries in place of its
    /// middle: `[out:<id> · N chars · L lines · thinned from this request; ...]`.
    /// It names no store state, so the view stays a pure function of the
    /// history.
    pub(crate) fn thinned_marker(&self) -> String {
        format!(
            "[{HANDLE_PREFIX}{} · {} chars · {} lines · thinned from this request; ctx.page/ctx.grep read it whole]",
            self.id,
            group_digits(self.total_chars),
            group_digits(self.lines),
        )
    }
}
