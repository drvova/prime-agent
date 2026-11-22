//! `agent prefetch` (v2): the neighbour handles `fs.read` paged out, and
//! whether the model went on to use them (counts only: never a path). The `fs` capability's rule
//! file names the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::prefetch::PrefetchFact;

impl SessionCounters {
    /// One read's prefetch (and the settled use of the previous one's handles): the read, the
    /// handles issued, used, and left unused, and whether the learned share left it without
    /// handles.
    pub(crate) fn note_prefetch(&self, fact: PrefetchFact) {
        for (property, count) in [
            ("prefetch_read_count", 1),
            ("prefetch_issued_count", fact.issued as u64),
            ("prefetch_used_count", fact.used as u64),
            ("prefetch_unused_count", fact.unused as u64),
            ("prefetch_skipped_count", u64::from(fact.skipped)),
        ] {
            self.note_capability(property, count);
        }
    }
}
