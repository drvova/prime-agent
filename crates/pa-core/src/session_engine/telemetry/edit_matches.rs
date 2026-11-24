//! `agent edit matches` (v2): how the kernel's `edit` found the text it
//! replaced, or why it found none (counts only: never a path, a string, or
//! file content). The `fs` capability's rule file names the event; session
//! end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::edit_matches::EditMatch;

impl SessionCounters {
    /// One `edit` call, by its outcome.
    pub(crate) fn note_edit_match(&self, outcome: EditMatch) {
        self.note_capability(
            match outcome {
                EditMatch::Exact => "edit_exact_count",
                EditMatch::TrailingWhitespace => "edit_trailing_whitespace_count",
                EditMatch::Indentation => "edit_indentation_count",
                EditMatch::UnicodePunctuation => "edit_unicode_punctuation_count",
                EditMatch::Unescaped => "edit_unescaped_count",
                EditMatch::AnchorPrefix => "edit_anchor_prefix_refused_count",
                EditMatch::Ambiguous => "edit_ambiguous_count",
                EditMatch::NotFound => "edit_not_found_count",
                EditMatch::AlreadyApplied => "edit_already_applied_count",
            },
            1,
        );
    }
}
