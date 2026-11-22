# factory: state-machine workflows of child agents

A machine declares states, each backed by a subagent spec, plus guarded transitions. Use it when a workflow needs shape: fan-out, bounded review/fix loops, joins, or one child per list item. A spec is plain data (hashes with symbol keys, lists, strings, numbers), stored as a harness entry of kind `factory` (`harness-create-factory`) or as a `MACHINE.md` in the machine library (seeds: `builder`, `pr-manager`, `review-sweep`). The factory ships disabled: until the user runs `/factory on`, the run functions and factory harness writes refuse with `the factory is disabled; run /factory on to enable it`, while `factory-help`, the validation functions, and the machine library still answer. Read `(displayln (factory-help))` before authoring a machine.

- `(factory-help)`: the full guide as a string: authoring rules, guards, joins, cycles, foreach, budgets, and this API with worked examples
- `(factory-run spec-id #:name)`: start a run of the stored factory entry `spec-id`, or of the library machine of that name. Returns at once, and the run continues after the cell and the turn end: `{run_id, spec_id, name, nodes, max_parallel, started, pending}`, plus `{machine, machine_path}` for a library machine. `#:name` labels the run
- `(factory-status run-id)`: `{run_id, spec_id, name, state, nodes, events, elapsed_ms, usage}`, plus `needs_verify`, and `pause_reason`, `last_error` and `remedy` while paused; `state` is `running`, `stopping`, `paused`, `done`, `failed`, or `stopped`
- `(factory-stop run-id)`: cancel every running child of the run: `{run_id, state, cancelled}`
- `(factory-resume run-id)`: continue a paused run: `{run_id, state, started, pending}`. Raises when the run is not paused
- `(factory-graph [reference])`: a machine's structure fused with live state for a run id; the static structure for a stored spec id; with no argument `{runs}`, every live run and the newest finished ones
- `(factory-watch run-id [seconds])`: the graph of a run plus `changed`, once its state or shape changes or `seconds` (at most 60) have passed
- `(factory-validate spec)`: the validation sentences of a spec in machine or dag form; an empty list means it is valid
- `(factory-canonicalize spec)`: the machine form the executor runs, every default applied: `{run, states, transitions}`. Raises the validation sentences
- `(factory-compile-dag dag)`: a dag-form spec compiled to machine form: `{machine, errors}`
- `(factory-topological-order nodes)`: node ids of a dag-form node list in dependency order. Raises on a duplicate id, an unknown dependency, or a cycle
- `(factory-list-machines)`: the machine library's usable machines, sorted by name: a list of `{name, description, version, author, source, path}`
- `(factory-import-machine path)`: validate a `MACHINE.md` file and store it in the personal machine library: `{name, path, created}`
- `(factory-export-machine target out #:overwrite)`: write a stored factory entry's spec, a run's machine, or a library machine as `MACHINE.md` at `out`: `{name, path, source}`. An existing file is refused unless `#:overwrite #t`
