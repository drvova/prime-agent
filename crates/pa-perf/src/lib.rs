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
// Pedantic-gate exceptions. Casts: the values are bounded by the corpus
// shapes and the bench's own measurements (row and byte counts, pty sizes,
// millisecond samples, the seeded generator's draws); the statistics are
// floats by nature. too_many_lines: the corpus writers and the bench's
// scenario table are flat sequences; splitting them adds indirection
// without changing the flow.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

pub mod bench;
pub mod corpus;
pub mod report;
pub mod trials;

mod rng;
