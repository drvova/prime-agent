//! `agent readonly cache` (v2): lookups in the kernel's read-only command
//! cache, by source and outcome (counts only: never a command or a path).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::command_cache::CacheLookup;

/// Hits and misses of allowlisted `bash()` commands and of `fs.read`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadonlyCacheCounts {
    hits: u64,
    misses: u64,
    file_hits: u64,
    file_misses: u64,
}

impl SessionCounters {
    /// One read-only cache lookup.
    pub(crate) fn note_cache_lookup(&self, lookup: CacheLookup) {
        self.with(|values| {
            let counts = &mut values.readonly_cache;
            *match (lookup.file_read, lookup.hit) {
                (false, true) => &mut counts.hits,
                (false, false) => &mut counts.misses,
                (true, true) => &mut counts.file_hits,
                (true, false) => &mut counts.file_misses,
            } += 1;
        });
    }
}

impl SessionTelemetry {
    /// Track `agent readonly cache`, unless no lookup ran.
    pub(super) fn track_readonly_cache(&self) {
        let mut counts = ReadonlyCacheCounts::default();
        self.counters.with(|values| counts = values.readonly_cache);
        if counts == ReadonlyCacheCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("readonly_cache_hit_count", counts.hits),
            ("readonly_cache_miss_count", counts.misses),
            ("readonly_cache_fs_read_hit_count", counts.file_hits),
            ("readonly_cache_fs_read_miss_count", counts.file_misses),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent readonly cache", properties);
    }
}
