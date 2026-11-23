//! The row that ends a kernel cell's edit diff when the diff was cut.
//!
//! A cell's `details.diffs` entry is `{path, diff}`, where `diff` is the
//! edit's numbered rows (`+N text`, `-N text`, ` N text`). A diff longer than
//! the kernel keeps is cut, and its last row is a [`DiffCut`]: how many changed
//! lines the cut left out, added and removed, so a client counting the rows
//! still shows the edit's true totals. A numbered row starts with `+`, `-` or
//! a space, so no row of a diff reads as this one.

use std::fmt;

/// The changed lines a cut diff left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffCut {
    pub added: usize,
    pub removed: usize,
}

impl DiffCut {
    /// The cut a diff row records, or `None` for any other row.
    #[must_use]
    pub fn parse(row: &str) -> Option<Self> {
        let counts = row.strip_prefix(PREFIX)?;
        let (_, totals) = counts.split_once(" (+")?;
        let (added, removed) = totals.strip_suffix(')')?.split_once(" -")?;
        let cut = Self {
            added: added.parse().ok()?,
            removed: removed.parse().ok()?,
        };
        (cut.to_string() == row).then_some(cut)
    }
}

const PREFIX: &str = "\u{2026} diff cut: ";

impl fmt::Display for DiffCut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { added, removed } = self;
        let lines = added + removed;
        let noun = if lines == 1 { "line" } else { "lines" };
        write!(f, "{PREFIX}{lines} more {noun} (+{added} -{removed})")
    }
}
