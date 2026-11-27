//! `agent previews` (v2): surprise-weighted previews of spilled and thinned
//! outputs, what they chose, and the `output-*` reads that followed them (fewer
//! follow-up reads means the previews chose well). Counts only: never a line,
//! a pattern, or output. The `ctx` capability's rule file names the event; session end emits it
//! through the generic capability counter.

use super::SessionCounters;
use crate::kernel::spill::PreviewFact;

impl SessionCounters {
    /// One built preview: the preview, its lines kept, dropped, folded as repeats, and folded as
    /// noise, and the noise patterns it learned.
    pub(crate) fn note_preview(&self, fact: PreviewFact) {
        for (property, count) in [
            ("preview_count", 1),
            ("preview_kept_line_count", fact.kept_lines as u64),
            ("preview_dropped_line_count", fact.dropped_lines as u64),
            (
                "preview_repeat_collapsed_count",
                fact.repeats_collapsed as u64,
            ),
            ("preview_noise_line_count", fact.noise_lines as u64),
            (
                "preview_noise_pattern_learned_count",
                fact.patterns_learned as u64,
            ),
        ] {
            self.note_capability(property, count);
        }
    }

    /// One `output-*` read: a follow-up page once the session has shown a preview.
    pub(crate) fn note_preview_followup(&self) {
        self.with(|values| {
            if values.capabilities.count("preview_count") > 0 {
                values.capabilities.add("preview_followup_read_count", 1);
            }
        });
    }
}
