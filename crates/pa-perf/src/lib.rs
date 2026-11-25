//! pa-perf: the prime-agent performance corpus generator and TUI bench
//! harness. A dev/CI tooling crate at the top of the workspace dependency
//! graph — nothing depends on it, and it never ships in a release.
//!
//! The corpus generator writes the product's real on-disk layout under an
//! isolated agent dir, using the product's own serializers and writers
//! (pa-types payloads, pa-daemon `SessionEntry`/`session_header_line`/
//! `RlmSpawnLedger`, pa-core's `WindowedSessionStore` for the warm
//! sidecars). The bench harness drives the real release binary under a pty.
//! The trials aggregation reads saved session files through pa-core's run
//! metrics and aggregates them the `FrontierHarness` Eval way.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "values are bounded by the corpus shapes and the bench's own measurements; the statistics are floats"
)]
#![expect(
    clippy::too_many_lines,
    reason = "the corpus writers and the bench's scenario table are flat sequences"
)]

pub mod bench;
pub mod corpus;
pub mod report;
pub mod trials;

mod rng;
