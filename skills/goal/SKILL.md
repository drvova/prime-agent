---
name: goal
description: Manage the persistent thread goal from the Scheme kernel. Use to read goal status and budget usage, to start a goal when the user explicitly asks for one, or to mark the active goal complete once its objective is fully achieved.
---

# Goal

The thread goal is a persistent objective the harness keeps re-prompting you to
pursue across turns until it is complete. Goal state (status, token budget,
usage accounting) lives in the host; these kernel functions are the interface
to it:

```scheme
(goal-get)
(goal-create "ship the release notes")
;; Only pass #:token-budget when the user explicitly asks for one:
;; (goal-create "ship the release notes" #:token-budget 200000)
(goal-complete)
```

## API

- `(goal-get)` — the current goal as a hash: `goal` (unset when no goal is
  set), `remaining_tokens`, and `completion_budget_report`. The `goal` hash
  carries `objective`, `status`, `token_budget`, `tokens_used`,
  `time_used_seconds`, and timestamps.
- `(goal-create objective #:token-budget #:check #:verify #:max-cost-usd)` —
  start a new active goal. Fails while a goal is still pending (active, paused,
  or budget-limited); a completed or errored goal is replaced by the new one.
  Only create a goal when the user or system/developer instructions explicitly
  ask for a persistent long-running goal; do not infer goals from ordinary
  tasks. Set `#:token-budget` only when an explicit token budget is requested.
  `#:check "cargo test -q"` is a shell command that must exit 0 before
  completion, `#:verify "judge"` adds a read-only judge that must answer MET
  citing evidence already in the conversation, and `#:max-cost-usd` is a
  dollar cap on the goal's provider spend.
- `(goal-complete)` — mark the existing goal achieved. Use only when the
  objective has actually been achieved and no required work remains; do not
  call it merely because the budget is nearly exhausted or because you are
  stopping work. When the result includes a `completion_budget_report`, report
  that final usage to the user. On a goal with `#:check`/`#:verify` the claim
  is verified first: when `passed` in the result's `verification` is false the
  goal stays active; close its `gap` before claiming again.

## Rules

- Goal status transitions other than completion (pause, resume, clear,
  budget-limiting) are controlled by the user and the host; there is no API for
  them here.
- When an active goal is actually complete, call `(goal-complete)`; do not
  merely say it is done — the harness keeps continuing the goal until the
  completion call arrives.
