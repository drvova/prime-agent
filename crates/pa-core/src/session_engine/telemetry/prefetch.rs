//! `agent prefetch` (v2): the neighbour handles `fs.read` paged out, and
//! whether the model went on to use them (counts only: never a path).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::prefetch::PrefetchFact;

/// Reads that prefetched, handles issued, used, and left unused, and reads
/// the learned share left without handles.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PrefetchCounts {
    reads: u64,
    issued: u64,
    used: u64,
    unused: u64,
    skipped: u64,
}

impl SessionCounters {
    /// One read's prefetch (and the settled use of the previous one's handles).
    pub(crate) fn note_prefetch(&self, fact: PrefetchFact) {
        self.with(|values| {
            let counts = &mut values.prefetch;
            counts.reads += 1;
            counts.issued += fact.issued as u64;
            counts.used += fact.used as u64;
            counts.unused += fact.unused as u64;
            counts.skipped += u64::from(fact.skipped);
        });
    }
}

impl SessionTelemetry {
    /// Track `agent prefetch`, unless no read prefetched.
    pub(super) fn track_prefetch(&self) {
        let mut counts = PrefetchCounts::default();
        self.counters.with(|values| counts = values.prefetch);
        if counts == PrefetchCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("prefetch_read_count", counts.reads),
            ("prefetch_issued_count", counts.issued),
            ("prefetch_used_count", counts.used),
            ("prefetch_unused_count", counts.unused),
            ("prefetch_skipped_count", counts.skipped),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent prefetch", properties);
    }
}
