---
name: refine
description: Trigger continual harness refinement from the Scheme kernel. Use when you notice a repeated failure, reusable tactic, delegation role, or behavior policy that should be persisted as a harness entry. Returns immediately; refinement runs when the current turn ends.
---

# Refine

Refinement analyzes the conversation trajectory and applies small, evidence-backed
updates to the continual harness (prompts, memories, skills, subagent specs).
The implementation lives in the host (the same one behind the user's `/refine`
command); these kernel functions are the interface to it:

```scheme
(refine-status)
(refine-run)
(refine-run "create a memory about always checking git status before committing")
(refine-run "promote the error-handling pattern to a global skill" #:global #t)
```

## API

- `(refine-status)` — the current refine state as a hash: `pending` (whether a
  requested refine is already queued for this turn) and `in_flight` (whether a
  refine is currently planning or applying).
- `(refine-run [instructions] #:global)` — schedule refinement. Returns
  `{scheduled #t}` immediately, or `{scheduled #f, reason}` when refinement
  cannot start. Optional `instructions` focus the refinement on a specific
  observation. Pass `#:global #t` to target the global harness store
  (cross-session); omit it for local (session-scoped) refinement.

## Rules

- Refinement never runs mid-cell. A scheduled refinement runs when the current
  turn ends; the harness applies changes and rebuilds the system prompt, then
  resumes you automatically. Continue working normally after calling it.
- One request per turn is enough; calling `refine-run` again before the turn
  ends only updates the instructions.
- Use refinement after observing a repeated failure, a reusable tactic, a
  repeated delegation role, or a behavior policy worth persisting. Do not
  rewrite the whole harness when a focused memory, skill, prompt note, or
  subagent spec is enough.
- To write one entry yourself, use the `harness-*` functions
  (`(api-help "harness")`).
