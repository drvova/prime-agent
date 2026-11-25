//! The transcript store's events: the background import of JSONL session files into the SQLite
//! store, and the `prime-agent export-jsonl` command that writes them back out.

use super::{count, duration, enum_rule, required, EventRule};

/// The size buckets of the bytes an import or export moved.
const BYTES_BUCKETS: &[&str] = &["lt_1mb", "lt_100mb", "lt_1gb", "lt_10gb", "ge_10gb"];

/// The daemon's background import of the agent dir's JSONL session files into the SQLite store
/// (v2), emitted once a pass that ran settles: files imported, files the database already held,
/// files that failed (skipped whole, retried on the next boot), the size of what was imported,
/// and how long it took. Counts and a size bucket only: never a path, a session id, or a file
/// name.
pub(super) const AGENT_TRANSCRIPTS_IMPORTED: EventRule = EventRule {
    name: "agent transcripts imported",
    since: 2,
    properties: &[
        ("imported_count", required(count())),
        ("already_stored_count", required(count())),
        ("failed_count", required(count())),
        ("bytes_bucket", required(enum_rule(BYTES_BUCKETS, "lt_1mb"))),
        ("duration_ms", required(duration())),
    ],
};

/// `prime-agent export-jsonl` ran (v2): whether it exported one session or all of them, the
/// files written, those already there with the same bytes, and the transcripts that failed.
/// Counts only: never a path, a session id, or content.
pub(super) const AGENT_SESSIONS_EXPORTED: EventRule = EventRule {
    name: "agent sessions exported",
    since: 2,
    properties: &[
        ("scope", required(enum_rule(&["session", "all"], "all"))),
        ("written_count", required(count())),
        ("unchanged_count", required(count())),
        ("failed_count", required(count())),
    ],
};
