//! `agent edit checks` (v2): the checks the kernel ran after edits, by
//! outcome (counts only: never a path, a command, or output).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::edit_checks::EditCheckOutcome;

/// Checks by outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EditCheckCounts {
    clean: u64,
    found: u64,
    late: u64,
    skipped: u64,
    failed: u64,
}

impl SessionCounters {
    /// One check after an edit, by outcome.
    pub(crate) fn note_edit_check(&self, outcome: EditCheckOutcome) {
        self.with(|values| {
            let counts = &mut values.edit_checks;
            *match outcome {
                EditCheckOutcome::Clean => &mut counts.clean,
                EditCheckOutcome::Found => &mut counts.found,
                EditCheckOutcome::Late => &mut counts.late,
                EditCheckOutcome::Skipped => &mut counts.skipped,
                EditCheckOutcome::Failed => &mut counts.failed,
            } += 1;
        });
    }
}

impl SessionTelemetry {
    /// Track `agent edit checks`, unless no edit was checked.
    pub(super) fn track_edit_checks(&self) {
        let mut counts = EditCheckCounts::default();
        self.counters.with(|values| counts = values.edit_checks);
        if counts == EditCheckCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            (
                "edit_check_run_count",
                counts.clean + counts.found + counts.failed,
            ),
            ("edit_check_found_count", counts.found),
            ("edit_check_late_count", counts.late),
            ("edit_check_skipped_count", counts.skipped),
            ("edit_check_failed_count", counts.failed),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent edit checks", properties);
    }
}
