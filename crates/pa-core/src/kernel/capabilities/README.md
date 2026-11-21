# Kernel capability manifests

One TOML file per capability of the Python kernel: a module (or a slice of the `rlm` package) of
`prime-agent-runtime/src/rlm/`. Each manifest records what the host side keeps about that
capability: what the runtime-ready probe asserts, whether the probe's import closure starts at
it, the name the kernel bootstrap binds it under, the host requests it issues as plumbing, and
its standalone adoption telemetry event.

Scope: facts about the capabilities. Non-goals: behavior, and what the model is told. The
kernel API reference the system prompt is spliced from lives in `src/prompts/capabilities/`
(see `src/prompts/layers.rs`).

`mod.rs` embeds every file with `include_str!` and derives from them the runtime-ready probe
(`core.toml`'s statements first, then every other manifest's by file name, each in file order)
and the closure freeze's seed. Its tests run the derived probe against the runtime source, and
fail when a manifest disagrees with the hand-written surfaces (the bootstrap bindings, the
telemetry catalog), or when a file here is not embedded. `crates/pa-core/tests/prompt_guards.rs`
reads `internal_host_requests` to tell host plumbing from the requests the model reaches.

## Adding a capability

- `<name>.toml` here (the file name is the capability name);
- one `manifest!("<name>")` line in `MANIFESTS` (`include_str!` needs a literal path, so the
  list cannot be read from the directory without a build script).

Its `probe` statements may use only `inspect`, `rlm`, and the names its own earlier statements
bind, since the probe runs the manifests in name order.

## Schema

| Key | Type | Meaning |
| --- | --- | --- |
| `module` | string | The Python module the capability lives in (`rlm.schedule`; `rlm` for a slice of the package root). |
| `closure_seed` | bool | The module seeds the readiness probe's import-closure freeze. |
| `closure_transitive` | string array | Modules the closure must reach through this one without seeding them. Optional. |
| `bound_module` | string | The name the kernel bootstrap binds the module under (`import rlm.schedule as schedule`). Omitted when the module is not bound by name. |
| `internal_host_requests` | string array | Host requests the capability issues that are host plumbing, never a model-facing function. Optional. |
| `probe` | string array | The runtime-ready probe's Python statements for this capability, verbatim. A statement may use `inspect` and `rlm` (imported by `core.toml`) and names its own earlier statements bind. |
| `[[telemetry]]` | array of tables | `event`: a standalone session-end adoption event in the pa-telemetry catalog; `properties`: the count properties of that event this capability owns (the event's `session_id` is implied). Several capabilities may share one event. Optional. |
