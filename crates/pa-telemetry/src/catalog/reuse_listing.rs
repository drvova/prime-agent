//! The `prime-agent reuse` command's event: the user viewed the reuse listing of their stored
//! skills and memories, or pruned entries from it.

use super::{count, enum_rule, required, EventRule};

/// The reuse listing was shown (`list`) or the entries the user named were removed (`prune`)
/// (v2). `entries` is how many skills and global harness entries the listing covered, and
/// `reused`, `used_once` and `unused` how many of them stood so; `pruned` is how many a prune
/// removed (0 for a listing). Never a name, a key, a path, or an entry's content.
pub(super) const AGENT_REUSE_LISTING: EventRule = EventRule {
    name: "agent reuse listing",
    since: 2,
    properties: &[
        ("action", required(enum_rule(&["list", "prune"], "list"))),
        ("entries", required(count())),
        ("reused", required(count())),
        ("used_once", required(count())),
        ("unused", required(count())),
        ("pruned", required(count())),
    ],
};
