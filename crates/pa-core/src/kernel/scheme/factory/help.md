# Factory

The factory runs state-machine workflows of spawned child agents. A machine
declares states, each backed by a subagent spec, plus guarded transitions
between them. `(factory-run "<spec_id>")` spawns each state's subagent as an
ordinary child, feeds captured outputs into the successors' prompts, and
drives the run to quiescence in the background; the call returns immediately
and the run continues after the cell and the model turn end. Use it when a
workflow needs shape: fan-out, bounded loops (review/fix until a verdict
approves), joins, or one child per list item.

The factory is opt-in: it ships disabled, and the user turns it on with
`/factory on` (`/factory off` disables it again, `/factory status` reports
it; the persisted setting is `factory.enabled` in the agent dir's
settings.json). While it is disabled, `factory-run`, `factory-status`,
`factory-stop`, `factory-resume`, `factory-graph` and `factory-watch`, plus
every factory harness write, refuse with one clean error:
"the factory is disabled; run /factory on to enable it". `(factory-help)`,
the dry run (`factory-validate` and friends) and the machine library answer
while disabled, so a machine can be authored before opting in.

## Where a spec lives

A spec is JSON data: in Scheme, hashes with symbol keys, lists, strings,
numbers and booleans. `(factory-run id)` looks for it in two places:

- A stored factory entry: a continual-harness entry of kind `factory`,
  written with `(harness-put "factory" "<spec_id>" content #:machine spec)`
  (the native form) or `#:dag spec` (sugar that compiles to machine form),
  exactly one of them. `harness-put` again replaces it and
  `(harness-delete "factory" "<spec_id>")` removes it. The write validates first; an
  invalid spec is never stored.
- The machine library: a `MACHINE.md` file (see Discovering machines).
  `(factory-import-machine "path/to/MACHINE.md")` validates and stores one,
  and `(factory-run "<its name>")` runs it without creating a harness entry.

Check a spec before storing it: `(factory-validate spec)` returns the list
of problems (empty when valid), and `(factory-canonicalize spec)` returns
the machine form the executor runs, every default applied.

This review/fix loop is the shipped pr-manager shape:

```scheme
(define pr-manager
  (hash
    'run (hash 'budget_ms 1800000 'max_parallel 8 'max_transitions 24)
    'states
    (list
      (hash 'id "entry" 'entry #t
            'subagent (hash 'prompt
              (string-append
                "Identify the pull request for the current branch with "
                "`gh pr view --json url`. Return a fenced json block of the form "
                "{\"pr_url\": \"https://github.com/owner/repo/pull/N\"}. "
                "Output only the json block."))
            'outputs (list (hash 'name "pr_url" 'type "json")))
      (hash 'id "reviewing"
            'subagent (hash 'prompt
              (string-append
                "Review the pull request at {pr_url} for merge-blocking "
                "defects with `gh pr diff`. When a fix report is bound below, "
                "verify the described fixes landed. Return a fenced json block "
                "of the form {\"verdict\": {\"approved\": <true|false>, "
                "\"findings\": [\"at most three one-line findings\"]}}. "
                "Output only the json block."))
            'inputs (list (hash 'name "pr_url" 'type "json" 'from "entry.pr_url")
                          (hash 'name "fix_report" 'type "json"
                                'from "fixing.fix_report" 'optional #t))
            'outputs (list (hash 'name "verdict" 'type "json"))
            'max_entries 4)
      (hash 'id "fixing"
            'subagent (hash 'prompt
              (string-append
                "Address the review findings in the verdict below. First "
                "reconcile any changes an earlier attempt left in the working "
                "tree. Make the smallest targeted fixes, run the relevant "
                "tests, and return a fenced json block of the form "
                "{\"fix_report\": {\"fixed\": [\"finding that was addressed\"], "
                "\"skipped\": [\"finding left alone and why\"]}}. "
                "Output only the json block.\n\n{verdict}"))
            'inputs (list (hash 'name "verdict" 'type "json" 'from "reviewing.verdict"))
            'outputs (list (hash 'name "fix_report" 'type "json"))
            'max_entries 3)
      (hash 'id "monitoring"
            'subagent (hash 'prompt
              (string-append
                "Stay resident as the watcher for {pr_url}: report the "
                "`gh pr checks` state once, then remain available for "
                "follow-up questions."))
            'inputs (list (hash 'name "pr_url" 'type "json" 'from "entry.pr_url"))
            'lifecycle "resident"))
    'transitions
    (list
      (hash 'from "entry" 'to "reviewing")
      (hash 'from "reviewing" 'to "fixing"
            'when (hash 'output "verdict" 'path "approved" 'op "eq" 'value #f))
      (hash 'from "reviewing" 'to "monitoring"
            'when (hash 'output "verdict" 'path "approved" 'op "eq" 'value #t))
      (hash 'from "fixing" 'to "reviewing"))))

(factory-validate pr-manager)   ; => '()
(harness-put "factory" "pr-manager"
  "Drive a PR through review/fix cycles, then keep a resident watcher on it."
  #:machine pr-manager)
```

The example exercises the core forms: `entry` is an entry state; the two
guards select the next state from the reviewer's `verdict`; `fix_report` is
optional, so the reviewer's first entry binds a null sentinel before the
fixer ever runs and its re-entry re-binds the real report; `max_entries`
bounds the loop; `monitoring` is a `resident` that stays alive under the
parent session after the run ends. Both worked examples bound their emitted
payloads in the prompt (a capped findings list here, a capped file list in
the review-sweep example) because the executor's capture cap still bounds
very large payloads: an unbounded payload truncates at the cap, fails to
capture, and fails the producing state through its `retries` and
`failure_policy` instead of settling valueless.

## Authoring reference

- **States**: 1 to 1024, unique ids matching `^[a-z0-9][a-z0-9-]{0,63}$`; at
  least one state carries `"entry": true`. An entry state's inputs must all
  be optional: its first entry binds the null sentinel, and a later entry
  through a transition (a verify loop back into it) binds the real settle.
  A state runs either a `python` callable (see Python states) or a
  subagent. A state's `subagent` is a harness subagent entry id or title
  (its content is the prompt template; `metadata.model`/`metadata.thinking`
  are spawn settings) or an inline `{"prompt": ...}` object with optional
  `name`/`model`/`thinking`. The optional `name` labels the spawned children
  run-scoped (at most 64 characters, unique across the machine's states; a
  name another state's name can suffix onto, `foo` vs `foo-i1`, is rejected
  at write time): the spawn label is `<run6>-name`, since the supervisor's
  sibling names are per parent session and a parent session outlives its
  runs. Read the exact label from the `spawned` event's `name` or the status
  node's instance rows. Re-entries, foreach fan-out and retries disambiguate
  with `-i<n>`/`-a<n>` suffixes; a suffixed label that would pass the host's
  64-character cap shrinks its base with a digest of the full name.
- **Ports**: inputs and outputs of type `text` or `json`. An input binds
  `"from": "<state_id>.<output_name>"`; types must match, duplicates are
  rejected, and nothing can read from a resident. Bound values render into
  `{input_name}` placeholders (one pass; inputs without a placeholder are
  appended in a trailing `## Inputs` section). A required input whose source
  has not settled yet keeps the entry pending; over a settled source that
  offers no value (an errored settle, or a settle that captured nothing for
  the port) it fails the dependent entry, while `"optional": true` binds a
  null sentinel in every no-value case. A state's own declared outputs
  capture from its settle's full answer: a declared port that binds no value
  (no parseable JSON for it in a fenced ```json block or a bare JSON object
  ending the answer, or a payload past the executor's capture cap) is
  retried once and then FAILS the producing entry as a task failure
  (`retries`, then `failure_policy`, apply, and the answer stays preserved
  under `needs_verify`), so a valueless settle never reaches a dependent's
  bind. A required self-input is rejected at validation, while an optional
  self-input is the designed self-loop form (first entry binds null,
  re-entries bind the previous settle).
- **Transitions**: `{"from": ..., "to": ..., "on": "settled", "when": ...}`.
  Each settle is evaluated exactly once and every guard that passes fires
  (fan-out is legal); a fire onto a state at `max_entries` is recorded as a
  blocked transition. `from` may be a list of states: a join that fires
  once every source settled, once per combination of source settles, and
  may not carry a guard. Guards are `{"output": ..., "path": ..., "op": ...,
  "value": ...}` over the from-state's latest settle: `op` is one of `eq`,
  `ne`, `gt`, `gte`, `lt`, `lte`, `exists`, `contains`; `path` drills a
  dotted path into a `json` output; `eq`/`ne` compare JSON-strictly (a
  boolean never equals a number), comparison ops need a numeric value,
  `contains` a non-empty list, `exists` no value, and a missing or
  unparseable port fails every op except `exists`. Under `continue` a failed
  settle still fires guard-less transitions, so dependents run; their
  required input over the failed source then fails the dependent entry,
  while an optional input over the failed source binds the null sentinel.
  Under the default `escalate` the failure never propagates that far.
- **Cycles are legal**: there is no acyclicity requirement; self-loops and
  back edges validate. The one rule is an entry state somewhere; a dag whose
  every node depends on another compiles to no entry states and is rejected.
- **foreach**: `{"over": "<input>", "max": 1..256}` expands one entry into
  one child per item of the named `json` input (clamped at `max`), each
  child rendered with its item bound as that input; an empty list settles
  the entry with no children.
- **Residents**: `"lifecycle": "resident"` states declare no outputs, no
  foreach, no retries, and no outgoing transitions, nothing reads from them,
  and their instance stays alive under the parent session after the run
  completes (stop the run to retire it).
- **Bounds and policies**: `run.max_parallel` (1..64, default 8) is the
  run's global budget of simultaneously running instances, not a per-node
  limit. `run.max_children` (default 10,000, capped at 1,000,000) is the
  run's budget of total admissions over its life, foreach expansions and
  retry re-spawns included; reaching it pauses the run once, and a resume
  continues past it as an explicit operator decision. `run.max_transitions`
  (default 10 per state, capped at 10,000) pauses the run once at the
  boundary, mid-settle; a resume continues after the transitions that
  already fired without re-firing them. `run.budget_ms` pauses the run once
  when exceeded (in-flight children keep running). Per state: `max_entries`
  (default 1), `retries` (0..10, same rendered prompt), `budget_ms`
  (admission to settlement; exceeding it fails the attempt without a retry),
  and `failure_policy`: `fail_fast` (cancel every child, run failed),
  `continue` (entry stays errored; the run finishes and reports failed if
  any state errored; its failed settle fires guard-less transitions so
  dependents run), or `escalate` (the default: pause the run BEFORE the
  failed settle propagates, so no transition fires and no downstream entry
  is spent, and a resume re-admits the failed entry as a fresh attempt: same
  rendered prompt, same entry slot, fresh `retries`; a binding failure over
  a broken upstream re-binds and pauses again, so stop and fix the upstream
  or author the dependent's state with `continue`). A child whose worker
  died or whose supervisor link was lost is infra, not the task's failure:
  the executor re-checks it after a short quiet period, then re-spawns the
  same prompt once without spending `retries` or `max_entries`; a second
  death fails through `retries`/`failure_policy` like any task failure.
- **Retry re-renders the same prompt into a changed world**: a retry runs
  the same prompt after an earlier attempt already changed the external
  state it works on, so a prompt that mutates external state (a fixing
  state's worktree, a tracker, a remote API) states its recovery posture:
  reconcile (start from what the earlier attempt left with `git status` and
  `git diff`, keep the changes that already serve the task, revert
  half-finished edits) or reset to a clean baseline, never fail on the old
  state or duplicate finished work.
- **Dead configurations fail loudly, never wedge**: a pending entry whose
  input source never settled, a `max_parallel` cap held entirely by
  never-settling residents with work queued, or nothing in flight and
  nothing pending each end the run as failed with the reason in the ledger.
  `wait` blocks on states are rejected at validation (not supported yet).

## Dag form

Sugar, not a second semantics: each node becomes a state entered once, a
node with no effective dependencies becomes an entry state, and the full
dependency set (`depends_on` plus every `inputs[].from` source) compiles to
ONE join transition, so a fan-in node waits for every parent.
`(factory-compile-dag dag)` shows the machine a dag compiles to. The shipped
review-sweep shape:

```scheme
(define review-sweep
  (hash
    'run (hash 'budget_ms 900000 'max_parallel 8)
    'nodes
    (list
      (hash 'id "files"
            'subagent (hash 'prompt
              (string-append
                "List the files the current branch changes relative to the "
                "base branch, capped at the eight most relevant. Return "
                "a fenced json block of the form {\"files\": [\"path/to/file\", ...]}. "
                "Output only the json block."))
            'outputs (list (hash 'name "files" 'type "json")))
      (hash 'id "review"
            'subagent (hash 'prompt
              (string-append
                "Review the changed file {files} for merge-blocking defects: "
                "correctness bugs, regressions, unhandled error paths, missing "
                "tests. Reply one short line: `<path>: <the most serious "
                "problem, or 'clean'>`."))
            'inputs (list (hash 'name "files" 'type "json" 'from "files.files"))
            'outputs (list (hash 'name "found" 'type "text"))
            'foreach (hash 'over "files" 'max 8))
      (hash 'id "report"
            'subagent (hash 'prompt
              (string-append
                "Merge the review lines below into one fenced json block of "
                "the form {\"issues\": [{\"file\": \"path\", \"finding\": \"...\"}]} "
                "listing every file that is not clean. Output only the json "
                "block.\n\n{found}"))
            'inputs (list (hash 'name "found" 'type "text" 'from "review.found"))))))
```

## Python states

A state can run a Python function instead of spawning a child: it carries
`"python": {"import": "<module>", "callable": "<function>"}` in place of
`subagent` (exactly one of the two). Use it for deterministic routing and
data shaping (listing work items, filtering, reshaping one state's output
for the next) where a child agent would only spend budget.

- **Contract**: the executor calls `fn(inputs)` with a dict of the state's
  declared inputs bound to raw values (a `text` input as a string, a `json`
  input as the parsed value, an optional input with no value as `None`;
  entry states with no declared inputs get `{}`). The function returns a
  dict whose keys are exactly the declared output names (a `text` output
  must be a string); that dict is the settle's outputs, so guards and
  downstream inputs read it like any other state's. An `async def` function
  is awaited.
- **Isolation**: every call runs in a fresh, short-lived Python subprocess
  (the `python3` on PATH, isolated mode) in the session's working directory,
  which is on the import path, so a module next to your work imports. It has
  no kernel, no harness, and cannot spawn agents. Prints go to stderr; only
  the returned dict goes to stdout. A machine without Python on PATH fails
  the state with "python state could not start".
- **Failures**: an exception (its stderr tail is the error), output that is
  not a dict, or keys that do not match the declared outputs fail the
  attempt: `retries` re-runs the call with the same inputs, then
  `failure_policy` applies. A bad import fails at run time the same way.
- **Time limit**: the state's `budget_ms`, else the run's `budget_ms`, else
  60 seconds. A call past its limit is killed and fails without a retry.
  A stop and `fail_fast` kill running calls.
- **Pure functions**: calls are re-run on retry and on resume after an
  escalate pause, so a python state should depend only on its inputs. A
  running call belongs to its run and is lost with it on a kernel restart.
- Python states cannot be `resident` and cannot use `foreach`. A call holds
  a `max_parallel` slot but is no child: it does not count toward
  `max_children`.

## Run and steer

```scheme
(define result (factory-run "pr-manager"))
;; {run_id, spec_id "pr-manager", nodes 4, max_parallel 8,
;;  started ("entry"), pending ()}  -- returns immediately.

(define status (factory-status (hash-ref result 'run_id)))
(hash-ref status 'state)   ; running | stopping | paused | done | failed | stopped
(hash-ref status 'nodes)   ; per state: status, entries_used/max_entries,
                           ; instances, latest answer_preview, error
(hash-ref status 'events)  ; trailing ledger: spawned, settled, answer_captured,
                           ; transition_fired, node_error, milestone, ...
(hash-ref status 'usage)   ; spawns, settled, tool_uses, max_parallel,
                           ; max_children, running, transitions_fired
;; needs_verify (when set): states whose entry keeps a provisional answer,
;;   a child exit with text or a settled answer whose declared output failed
;;   to capture; the entry row carries the provisional_answer preview.
;; pause_reason, last_error, remedy (while paused): why the run paused, the
;;   last admission or bind failure, and what to do next.

(define graph (factory-graph (hash-ref result 'run_id)))
;; {run_id, spec_id, state, machine {order, states, transitions, run},
;;  nodes, active_nodes, last_fired, events, usage,
;;  budget {limit_ms, consumed_ms}}  -- structure fused with live state.
(factory-graph)               ; every live run: {runs}
(factory-graph "pr-manager")  ; a stored spec's static graph

(define watched (factory-watch (hash-ref result 'run_id) 30))
;; the same snapshot plus `changed`: the call waits until the run's state
;; or instance shape changes or the timeout (seconds, at most 60) elapses,
;; so one call follows a run's progress without polling factory-status.
```

- `factory-run` validates the spec again and resolves every subagent
  reference first, reporting all failures in one error and starting nothing
  on any failure; `#:name` labels the run in status and the TUI.
- Pause and failure notices (escalate, budget, max_transitions,
  max_children, failed, finished) arrive as quiet notices in the
  conversation once per kind per run (an escalate pause notifies again
  after each resume), the pause notices with the resume call spelled out: a
  paused run does not need polling to be noticed.
- `(factory-stop run-id)` cancels every running child of the run
  (idempotent); `(factory-resume run-id)` continues a paused run and raises
  on a non-paused one; after an escalate pause it first re-admits the failed
  entry.
- The activity lane the daemon and TUI speak is camelCase on the wire
  (`runId`, `specId`, `timeoutMs`); this API is snake_case.

## Discovering machines

- The machine library: machines are `MACHINE.md` files (frontmatter with
  `name`, `description`, `version`, `author`, plus one fenced `machine-spec`
  block holding the JSON spec), one directory per machine, resolved from two
  levels: the machines shipped with the product (`PRIME_AGENT_MACHINES_DIR`
  redirects that level at a team directory) first, the personal `machines/`
  library under the agent dir second; the earlier level wins on name
  conflicts. `(factory-list-machines)` shows only what parses and validates.
  `(factory-import-machine path)` runs the same write-time validation as a
  stored spec, so an invalid machine never persists.
  `(factory-export-machine target out)` writes a stored entry's spec, a
  run's machine, or a library machine (copied verbatim) to a fresh path; an
  existing target is refused unless `#:overwrite #t`. `(factory-run
  "<name>")` runs a library machine directly without creating a harness
  entry; a machine that exists but is broken names its errors instead of
  pretending the name is unknown. The shipped machines are `builder`,
  `pr-manager`, and `review-sweep`; the worked examples above derive from
  their shapes.
- The TUI factory page: the activity dock's factory group opens one live
  diagram per run, newest run first.

## Safety

- Every subagent state spawns real children that spend budget. Bound loops
  with `max_entries`, `max_transitions`, and `run.max_children` (total
  admissions); the default `escalate` policy pauses instead of failing, so
  read `factory-status` (or the notice) before resuming.
- Outputs bind from the child's full final answer (the executor caps its
  own capture at 65,536 characters, the same bound the host puts on the
  answer text): a fenced json output block binds whole as long as it fits
  that cap (a bare JSON object that ends the answer binds too), so keep
  declared outputs compact and let the full answer live in the child's
  session.
- Runs live in kernel memory: a kernel restart loses `factory-status` for
  old runs, but the children keep running under the supervisor
  (`agent-roster #:children` sees them). Stop runs before restarting, or delete
  the children by hand afterwards.
- Residents outlive their run; stop the run (or tear down the session) to
  retire them. Prefer `(factory-stop run-id)` over deleting a factory child
  by hand: the executor claims and cancels children itself, and a hand
  deletion surfaces as a child failure through the state's policy.
