//! `agent readonly cache` (v2): lookups in the kernel's read-only command
//! cache, by source and outcome (counts only: never a command or a path). The `bash` capability's
//! rule file names the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::command_cache::CacheLookup;

impl SessionCounters {
    /// One read-only cache lookup.
    pub(crate) fn note_cache_lookup(&self, lookup: CacheLookup) {
        self.note_capability(
            match (lookup.file_read, lookup.hit) {
                (false, true) => "readonly_cache_hit_count",
                (false, false) => "readonly_cache_miss_count",
                (true, true) => "readonly_cache_fs_read_hit_count",
                (true, false) => "readonly_cache_fs_read_miss_count",
            },
            1,
        );
    }
}
