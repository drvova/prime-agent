//! `agent error enrichment` (v2): failed cells and awaited commands that got
//! the source lines around their error locations (counts only: never a path,
//! a line, a command, or output).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::error_context::EnrichmentFact;

/// Enriched results, the locations attached and left out, reads that ran out
/// of time, and the failed commands seen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ErrorEnrichmentCounts {
    enriched: u64,
    locations: u64,
    skipped: u64,
    timed_out: u64,
    failed_commands: u64,
}

impl SessionCounters {
    /// One failed cell or awaited command looked at for error locations.
    pub(crate) fn note_error_enrichment(&self, fact: EnrichmentFact) {
        self.with(|values| {
            let counts = &mut values.error_enrichment;
            counts.enriched += u64::from(fact.locations > 0);
            counts.locations += fact.locations as u64;
            counts.skipped += fact.skipped as u64;
            counts.timed_out += u64::from(fact.timed_out);
            counts.failed_commands += fact.failed_commands as u64;
        });
    }
}

impl SessionTelemetry {
    /// Track `agent error enrichment`, unless the session never failed a cell
    /// or command with a location.
    pub(super) fn track_error_enrichment(&self) {
        let mut counts = ErrorEnrichmentCounts::default();
        self.counters
            .with(|values| counts = values.error_enrichment);
        if counts == ErrorEnrichmentCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("error_enrichment_count", counts.enriched),
            ("error_location_count", counts.locations),
            ("error_location_skipped_count", counts.skipped),
            ("error_enrichment_timeout_count", counts.timed_out),
            ("error_failed_command_count", counts.failed_commands),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent error enrichment", properties);
    }
}
