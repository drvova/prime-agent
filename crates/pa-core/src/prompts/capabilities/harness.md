# harness: persistent memories, notes, skill entries, subagent specs, and refinement

The continual harness store outlives the turn and compaction. Entries go to this session's store unless `#:global #t` selects the cross-session one; an id written `global:<id>` or `local:<id>`, as the overview shows it, is accepted as is. An unsaved session has no local store: pass `#:global #t`. An entry is `{id, kind, title, content, path, scope, reference, arguments, metadata, source, created_at, updated_at, version}`: `kind` is `"prompt"`, `"memory"`, `"skill"`, `"subagent"`, or `"factory"`; `scope` is `"local"` or `"global"`; `source` is `"agent"` or `"refine"`; `version` starts at 1 and grows with every update. An update keeps every optional field it omits. Everything raises while the user has harness refinements turned off in `/settings`.

- `(harness-create-memory title content #:id #:path #:metadata #:global)`: create a memory; the entry. Without `#:id` it is named after its title. `#:path` groups it (default `"general"`)
- `(harness-update-memory id title content #:path #:metadata #:global)`: replace a memory's title and content; the entry
- `(harness-delete-memory id #:global)`: delete a memory; whether it existed
- `(harness-create-prompt-note title content #:id #:path #:metadata #:global)`: create a prompt note (standing guidance); the entry. `#:path` defaults to `"policy"`
- `(harness-update-prompt-note id title content #:path #:metadata #:global)`: replace a prompt note's title and content; the entry
- `(harness-delete-prompt-note id #:global)`: delete a prompt note; whether it existed
- `(harness-create-skill title content #:id #:path #:reference #:arguments #:metadata #:global)`: create a skill entry; the entry. A skill entry describes a Python REPL skill: `#:reference` is required, a hash with `'type "python"`, a Python import (`'import` or `'python_import`), and a `'callable` or `'call_pattern`; `#:arguments` describes its parameters. This kernel stores and lists these entries but cannot call them
- `(harness-update-skill id title content #:path #:reference #:arguments #:metadata #:global)`: replace a skill entry's title and content; the entry
- `(harness-delete-skill id #:global)`: delete a skill entry; whether it existed
- `(harness-create-subagent title content #:id #:path #:metadata #:global)`: create a subagent spec, a reusable brief for `rlm-spawn`; the entry
- `(harness-update-subagent id title content #:path #:metadata #:global)`: replace a subagent spec's title and content; the entry
- `(harness-delete-subagent id #:global)`: delete a subagent spec; whether it existed
- `(harness-create-factory title content #:id #:path #:dag #:machine #:metadata #:global)`: create a factory entry, a state machine `factory-run` runs by its id; the entry. The spec is `#:machine` or `#:dag`, exactly one, validated first: an invalid spec is never stored. Refused while the factory is disabled
- `(harness-update-factory id title content #:path #:dag #:machine #:metadata #:global)`: replace a factory entry's title and content, and its spec when one is given; the entry
- `(harness-delete-factory id #:global)`: delete a factory entry; whether it existed. Allowed while the factory is disabled
- `(harness-record-refinement trigger changes #:evidence #:outcome #:id #:global)`: record one refinement pass: `{id, trigger, changes, evidence, outcome, created_at}`. `trigger` says what caused it; `changes` is one string or a list of them
- `(harness-plan-refinement observation #:failing-component #:next-step)`: a suggested diagnose, update, validate plan for one observation, as a list of steps. Nothing is stored
- `(harness-overview #:max-entries-per-kind #:global)`: a readable summary of one store as a string: its file, each kind's count and first entries (default 20 per kind), and the last refinements
- `(harness-search query #:kind #:limit #:global)`: entries ranked by how well they match the words of `query`, best first, at most `#:limit` (default 10); `#:kind` narrows to one kind
- `(harness-state #:state-dir #:global)`: one store in full: `{scope, file_path, entries, refinements}`, `entries` keyed by kind and then id. `#:state-dir` reads another agent's store from its harness directory

Refinement has another model read the transcript and apply small, evidence-backed edits to the store (the user's `/refine`). It never runs mid-cell: a scheduled refinement runs when the turn ends, the harness rebuilds the system prompt, sends a message with the result, and resumes you.

- `(refine-run [instructions] #:global)`: schedule a refinement for when the turn ends: `{scheduled #t}`, or `{scheduled #f, reason}`. `instructions` focus it on one observation; `#:global #t` targets the cross-session store. Calling it again before the turn ends only updates the instructions
- `(refine-status)`: `{pending, in_flight}`: whether a refinement is queued for this turn, and whether one is planning or applying
