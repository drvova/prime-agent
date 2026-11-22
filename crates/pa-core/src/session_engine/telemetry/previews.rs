//! `agent previews` (v2): surprise-weighted previews of spilled and thinned
//! outputs, what they chose, and the `ctx-*` reads that followed them (fewer
//! follow-up reads means the previews chose well). Counts only: never a line,
//! a pattern, or output.

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::spill::PreviewFact;

/// Previews built, their lines kept, dropped, folded as repeats, and folded
/// as noise, noise patterns learned, and `ctx-*` reads after the first preview.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PreviewCounts {
    previews: u64,
    kept: u64,
    dropped: u64,
    repeats: u64,
    noise_lines: u64,
    learned: u64,
    followup_reads: u64,
}

impl SessionCounters {
    /// One built preview.
    pub(crate) fn note_preview(&self, fact: PreviewFact) {
        self.with(|values| {
            let counts = &mut values.previews;
            counts.previews += 1;
            counts.kept += fact.kept_lines as u64;
            counts.dropped += fact.dropped_lines as u64;
            counts.repeats += fact.repeats_collapsed as u64;
            counts.noise_lines += fact.noise_lines as u64;
            counts.learned += fact.patterns_learned as u64;
        });
    }

    /// One `ctx-*` read: a follow-up page once the session has shown a preview.
    pub(crate) fn note_preview_followup(&self) {
        self.with(|values| {
            if values.previews.previews > 0 {
                values.previews.followup_reads += 1;
            }
        });
    }
}

impl SessionTelemetry {
    /// Track `agent previews`, unless the session built none.
    pub(super) fn track_previews(&self) {
        let mut counts = PreviewCounts::default();
        self.counters.with(|values| counts = values.previews);
        if counts == PreviewCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("preview_count", counts.previews),
            ("preview_kept_line_count", counts.kept),
            ("preview_dropped_line_count", counts.dropped),
            ("preview_repeat_collapsed_count", counts.repeats),
            ("preview_noise_line_count", counts.noise_lines),
            ("preview_noise_pattern_learned_count", counts.learned),
            ("preview_followup_read_count", counts.followup_reads),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent previews", properties);
    }
}
