# Kernel capability manifests

One TOML file per kernel capability: a module (or a slice of the `rlm` package) of
`src/rlm/` that the persistent Python REPL exposes to the model. Each manifest records
everything the host side keeps about that capability: what the runtime-ready probe asserts,
whether the probe's import closure starts at it, the name the kernel bootstrap binds it
under, the host requests it issues and the prompt name each one is documented as, and its
standalone adoption telemetry event. Beside it, the capability's doc files (`<name>.md`, and
`<name>.<part>.md` for each further part) hold its section of the core system prompt verbatim.

Scope: facts about the capabilities and their prompt documentation. Non-goals: behavior. The files live outside `src/` so
they are neither installed into the kernel venv nor part of the runtime identity hash (which
covers `pyproject.toml`, `src/rlm/**/*.py`, and `src/rlm/machines/`).

pa-core embeds every file with `include_str!` (`crates/pa-core/src/kernel/capabilities/`) and
derives from them the runtime-ready probe (`core.toml`'s statements first, then every other
manifest's by file name, each in file order), the closure freeze's seed, and the core prompt
layer. Its tests run the derived probe against `src/`, and fail when a manifest disagrees with
the hand-written surfaces (the bootstrap bindings, the core prompt, the telemetry catalog, the
registered host-request handlers), or when a file here is not embedded there. `crates/pa-core/tests/prompt_guards.rs`
maps host requests to prompt tokens from the `[host_requests]` tables.

## Doc files and the core prompt

`crates/pa-core/src/prompts/layers/core.md` holds the core prompt's hand-written text: headings
and prose shared by several capabilities, and the tools that are not kernel capabilities. Where
a capability's documentation goes it has one slot line, `{{capability:<stem>}}`, which the
layer replaces with the doc file `<stem>.md` minus its final newline. A doc file is the
capability's whole `##` section when it has one of its own (`schedule.md`), otherwise its
bullets within a shared section (`ask.md` under "Todos and questions"). A capability whose
documentation is split by another's text has one file per contiguous part (`core.md`,
`core.end_turn.md`, `core.progress_note.md`). The kernel-implemented tool list is the slot
`{{prompt_builtins:<name>,...}}`: the named capabilities' `prompt_builtins`, in slot order.

The layer is spliced once, by the first system-prompt assembly. pa-core's tests fail when a slot
names no doc file, a doc file fills no slot or two, the builtin list misses a capability with
`prompt_builtins` or names one twice, a doc file is not named for the manifest that embeds it,
or a capability's doc files bullet other names than its `documented` list.

## Adding a capability

A new capability touches:

- `<name>.toml` here (the file name is the capability name);
- `<name>.md` here, when it documents names in the core prompt;
- one `{{capability:<name>}}` line in `prompts/layers/core.md`, and its name in the
  `{{prompt_builtins:...}}` slot when it has `prompt_builtins`;
- one `manifest!("<name>", docs = ["<name>"])` line in pa-core's `MANIFESTS`, which embeds
  both files (`include_str!` needs a literal path, so this list cannot be read from the
  directory without a build script).
- for a `[[telemetry]]` block, its rule file in pa-telemetry (`src/catalog/capabilities/<name>.rs`,
  listed in that module's emission order); its seam counts through pa-core's generic
  `SessionCounters::note_capability` under the rule's property names.
  No shared counter struct changes.

Do not grow a shared list in another file. Its `probe` statements may use only `inspect`, `rlm`,
and the names its own earlier statements bind, since the probe runs the manifests in name order.

## Schema

| Key | Type | Meaning |
| --- | --- | --- |
| `module` | string | The Python module the capability lives in (`rlm.schedule`; `rlm` for a slice of the package root). |
| `closure_seed` | bool | The module seeds the readiness probe's import-closure freeze. |
| `closure_transitive` | string array | Modules the closure must reach through this one without seeding them. Optional. |
| `bound_module` | string | The name the kernel bootstrap binds the module under (`import rlm.schedule as schedule`). Omitted when the module is not bound by name. |
| `prompt_builtins` | string array | This capability's entries in the core prompt's list of kernel-implemented tools (`schedule.*`, `ask`), spliced into that list's slot. Optional. |
| `documented` | string array | The core prompt's bullet tokens this capability owns (`schedule.add`), each bulleted in its doc files. Optional. |
| `internal_host_requests` | string array | Host requests the module issues that are host plumbing, never documented. Optional. |
| `probe` | string array | The runtime-ready probe's Python statements for this capability, verbatim. A statement may use `inspect` and `rlm` (imported by `core.toml`) and names its own earlier statements bind. |
| `[host_requests]` | table | Each host request the module issues, mapped to the documented prompt token it surfaces as. Optional. |
| `[[telemetry]]` | array of tables | `event`: a standalone session-end adoption event in the pa-telemetry catalog; `properties`: the count properties of that event this capability owns (the event's `session_id` is implied). Several capabilities may share one event. Optional. |
