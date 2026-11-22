# steel-core 0.8.3 include-path patch

Pinned source of crates.io `steel-core` 0.8.3 (MIT OR Apache-2.0), the Scheme
interpreter behind the kernel (`crates/pa-core/src/kernel/scheme/`).

The only upstream source change is `src/stdlib.rs`. Upstream embeds its
Scheme prelude twice: with forward-slash paths for every target but Windows,
and with backslash paths (`include_str!(r#"scheme\stdlib.scm"#)`) for Windows
targets. `include_str!` resolves its path on the build host, so the backslash
form compiles only on a Windows host. Cross-checking a Windows target from
Linux, which the `windows cross-check + clippy (x86_64-pc-windows-gnu)` CI job
and `make windows-cross` do, fails with "couldn't read .../src/scheme\stdlib.scm".
The patch keeps the forward-slash form for every target; a Windows host
accepts forward slashes in `include_str!` paths. The unused `METHODS` constant
is now defined for Windows targets too, as a side effect.

`Cargo.toml` is the crate's published manifest without its `[[test]]` and
`[[bench]]` entries; `tests/`, `benches/`, `proptest-regressions/` and the
crate's own `Cargo.lock` are not vendored.

Drop this directory and the `[patch.crates-io]` entry with the steel-core
release that uses one path form for all targets.
