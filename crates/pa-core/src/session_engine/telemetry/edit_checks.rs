//! `agent edit checks` (v2): the checks the kernel ran after edits, by
//! outcome (counts only: never a path, a command, or output). The `fs` capability's rule file
//! names the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::edit_checks::EditCheckOutcome;

impl SessionCounters {
    /// One check after an edit, by outcome: a clean, finding, or failed check also counts as a
    /// run; a late or skipped one does not.
    pub(crate) fn note_edit_check(&self, outcome: EditCheckOutcome) {
        let (ran, outcome_property) = match outcome {
            EditCheckOutcome::Clean => (true, None),
            EditCheckOutcome::Found => (true, Some("edit_check_found_count")),
            EditCheckOutcome::Failed => (true, Some("edit_check_failed_count")),
            EditCheckOutcome::Late => (false, Some("edit_check_late_count")),
            EditCheckOutcome::Skipped => (false, Some("edit_check_skipped_count")),
        };
        if ran {
            self.note_capability("edit_check_run_count", 1);
        }
        if let Some(property) = outcome_property {
            self.note_capability(property, 1);
        }
    }
}
