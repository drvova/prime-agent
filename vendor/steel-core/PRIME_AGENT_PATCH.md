# steel-core 0.8.3 with Prime Agent's patches

Pinned source of crates.io `steel-core` 0.8.3 (MIT OR Apache-2.0), the Scheme
interpreter behind the kernel (`crates/pa-core/src/kernel/scheme/`).

Each upstream source change is listed here; nothing else differs from the
published crate.

## `src/stdlib.rs`: one include path for every target

Upstream embeds its
Scheme prelude twice: with forward-slash paths for every target but Windows,
and with backslash paths (`include_str!(r#"scheme\stdlib.scm"#)`) for Windows
targets. `include_str!` resolves its path on the build host, so the backslash
form compiles only on a Windows host. Cross-checking a Windows target from
Linux, which the `windows cross-check + clippy (x86_64-pc-windows-gnu)` CI job
and `make windows-cross` do, fails with "couldn't read .../src/scheme\stdlib.scm".
The patch keeps the forward-slash form for every target; a Windows host
accepts forward slashes in `include_str!` paths. The unused `METHODS` constant
is now defined for Windows targets too, as a side effect.

Drop it with the steel-core release that uses one path form for all targets.

## `src/steel_vm/engine.rs`: `Engine::with_macro_engine`

Steel evaluates the body of a `defmacro`, of a `syntax-case` transformer and
of `begin-for-syntax` in a second engine its compiler owns
(`parser/kernel.rs`, `Kernel::engine`). Upstream gives an embedder no way to
reach it, so an engine whose builtins were restricted still ran macro bodies
with every builtin of `Engine::new_sandboxed`: processes, the environment,
`Engine::new`, the process's standard output, and a `ThreadStateController`
of its own that no interrupt of the outer engine reaches. The patch adds one
public method, `Engine::with_macro_engine`, that runs a closure on that
engine. `crates/pa-core/src/kernel/scheme/confinement.rs` closes both engines
with it and `cell.rs` interrupts both.

Drop it when upstream exposes the macro engine, or runs macro bodies with the
outer engine's modules and controller.

## Not vendored

`Cargo.toml` is the crate's published manifest without its `[[test]]` and
`[[bench]]` entries; `tests/`, `benches/`, `proptest-regressions/` and the
crate's own `Cargo.lock` are not vendored.

Drop this directory and the `[patch.crates-io]` entry once no patch above is
needed.
