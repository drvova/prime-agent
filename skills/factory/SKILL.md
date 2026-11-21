---
name: factory
description: Run state-machine workflows of spawned child agents: store a validated machine or dag spec as a continual-harness factory entry (harness-create-factory), then run, watch, stop, and resume it through the factory-* functions. For the full authoring reference and API guide, call (factory-help) in the kernel.
---

# Factory

The factory runs state-machine workflows of spawned child agents. A stored
factory entry — harness kind `factory`, written with
`harness-create-factory` — declares the machine: states, each backed by
a subagent spec, plus guarded transitions between them. The spec validates
at write time (`#:machine` form, or `#:dag` sugar that compiles to one; pass
exactly one), and an invalid spec is never stored.
`(factory-run "<spec_id>")` then spawns each state's subagent as an
ordinary child, feeds captured outputs into the successors' prompts, and
drives the run to quiescence in the background — the call returns
immediately and the run continues after the cell and the model turn end. Use
it when a workflow needs shape: fan-out, bounded loops (review/fix until a
verdict approves), joins, or one child per list item.

**For the full authoring reference and API guide — states, ports, guards,
joins, foreach, residents, budgets and policies, and the `factory-run`,
`factory-status`, `factory-stop`, `factory-resume`, `factory-graph`, and
`factory-watch` calls with worked examples — run
`(displayln (factory-help))` in the kernel.** `(api-help "factory")` lists
every factory function with its result.

## The opt-in gate

The factory ships disabled. `(factory-help)`, the dry run
(`factory-validate`, `factory-canonicalize`, `factory-compile-dag`,
`factory-topological-order`) and the machine library answer while it is off
(a machine can be authored before opting in), but the run functions and every
factory harness write refuse with one message:
`the factory is disabled; run /factory on to enable it`. The
user turns it on with `/factory on` in the client (`/factory off`
disables it again — refused while the session still has live runs, and
on a lane-advertising client while the live-run count cannot be read, so
an active factory never loses its stop path, `/factory status` reports
it), which persists the
`factory.enabled` setting in the agent dir's settings.json — the same
setting the daemon's `factory_activity` lane advertisement reads, so the
TUI's factory dock group and page surface only on a client started while
the factory is enabled. If a call refuses with that message, tell the
user to run `/factory on` and restart the client.

## Discovering machines

- The machine library: machines are `MACHINE.md` files, one directory
  per machine — the bundled seeds ship inside Prime Agent
  (`PRIME_AGENT_MACHINES_DIR` redirects that level at a team directory), and
  the personal library lives under the agent dir. `(factory-list-machines)`
  lists them, `(factory-import-machine path)` and
  `(factory-export-machine target out)` store and write one, and
  `prime-agent factory list | import | export` does the same from a shell. The
  seeds are `builder`, `pr-manager`, and `review-sweep`.
- The TUI factory page: the activity dock's `⚙ N factory` group (Enter or
  click) opens one live diagram per run, newest run first. The up/down
  arrows walk the run headers and the states that spawned a child;
  Enter or Right opens the selected state's latest child session, and
  Left or Esc closes the page. Ctrl+X (the agents view's stop key) twice
  stops the selected row: the whole run on its header, or the state's
  running child (that attempt fails under the state's `retries` and
  `failure_policy`). `r` resumes a paused run.
