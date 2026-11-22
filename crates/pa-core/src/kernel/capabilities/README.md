# Kernel capability manifests

One TOML file per capability of the kernel API (the Scheme functions a session's `eval` cells
call). Each manifest records what the host side keeps about that capability: the host requests
it issues as plumbing, and its standalone adoption telemetry events.

Scope: facts about the capabilities. Non-goals: behavior, and what the model is told. The
kernel API reference the system prompt is spliced from lives in `src/prompts/capabilities/`
(see `src/prompts/layers.rs`).

`mod.rs` embeds every file with `include_str!` for the tests, which fail when a manifest
disagrees with the telemetry catalog or with pa-telemetry's capability rule files, or when a
file here is not embedded.
`crates/pa-core/tests/prompt_guards.rs` reads `internal_host_requests` to tell host plumbing
from the requests the model reaches.

## Adding a capability

- `<name>.toml` here (the file name is the capability name);
- one `manifest!("<name>")` line in `MANIFESTS` (`include_str!` needs a literal path, so the
  list cannot be read from the directory without a build script).
- for a `[[telemetry]]` block, its rule file in pa-telemetry
  (`src/catalog/capabilities/<name>.rs`, listed in that module's emission order); its seam counts
  through `SessionCounters::note_capability` (`src/session_engine/telemetry.rs`) under the rule's
  property names. No shared counter struct changes.

## Schema

| Key | Type | Meaning |
| --- | --- | --- |
| `internal_host_requests` | string array | Host requests the capability issues that are host plumbing, never a model-facing function. Optional. |
| `[[telemetry]]` | array of tables | `event`: a standalone session-end adoption event in the pa-telemetry catalog; `properties`: the count properties of that event this capability owns (the event's `session_id` is implied). Several capabilities may share one event. Optional. |
