# Kernel capability manifests

One TOML file per kernel capability: a module (or a slice of the `rlm` package) of
`src/rlm/` that the persistent Python REPL exposes to the model. Each manifest records
everything the host side keeps about that capability: what the runtime-ready probe asserts,
whether the probe's import closure starts at it, the name the kernel bootstrap binds it
under, the host requests it issues and the prompt name each one is documented as, and its
standalone adoption telemetry event.

Scope: facts about the capabilities. Non-goals: behavior. The files live outside `src/` so
they are neither installed into the kernel venv nor part of the runtime identity hash (which
covers `pyproject.toml`, `src/rlm/**/*.py`, and `src/rlm/machines/`).

pa-core embeds every file with `include_str!` (`crates/pa-core/src/kernel/capabilities/`), and
its consistency test fails when a manifest and the host's hand-maintained lists disagree, or
when a file here is not embedded there.

## Adding a capability

Add `<name>.toml` here and its `include_str!` line in pa-core's `MANIFESTS`; do not grow a
shared list in another file. The file name is the capability name.

## Schema

| Key | Type | Meaning |
| --- | --- | --- |
| `module` | string | The Python module the capability lives in (`rlm.schedule`; `rlm` for a slice of the package root). |
| `closure_seed` | bool | The module seeds the readiness probe's import-closure freeze. |
| `closure_transitive` | string array | Modules the closure must reach through this one without seeding them. Optional. |
| `bound_module` | string | The name the kernel bootstrap binds the module under (`import rlm.schedule as schedule`). Omitted when the module is not bound by name. |
| `prompt_builtins` | string array | This capability's entries in the core prompt's list of kernel-implemented tools (`schedule.*`, `ask`). Optional. |
| `documented` | string array | The core prompt's bullet tokens this capability owns (`schedule.add`). Optional. |
| `internal_host_requests` | string array | Host requests the module issues that are host plumbing, never documented. Optional. |
| `probe` | string array | The runtime-ready probe's Python statements for this capability, verbatim. A statement may use `inspect` and `rlm` (imported by `core.toml`) and names its own earlier statements bind. |
| `[host_requests]` | table | Each host request the module issues, mapped to the documented prompt token it surfaces as. Optional. |
| `[[telemetry]]` | array of tables | `event`: a standalone session-end adoption event in the pa-telemetry catalog; `properties`: the count properties of that event this capability owns (the event's `session_id` is implied). Several capabilities may share one event. Optional. |
