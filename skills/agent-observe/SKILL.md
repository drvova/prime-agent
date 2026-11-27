---
name: agent-observe
description: Read-only roster and observation of an agent's parent, siblings, and direct children. Use to discover reachable agents and to inspect family status and bounded recent-message previews without mutating sessions.
---

# Agent Observe

Observe the current agent's nuclear family through the local daemon: parent,
siblings, direct children, and self. `(agent-roster)` is the one family
roster and covers every member `agent-send` can reach.
`(agent-roster target)` also returns an inactive child's roster row.
`(agent-roster target #:recent #t)` also reads a member no worker holds (a passivated or
finished child) from its session file, read-only, without waking it. They
cannot read a root sibling that has no live session in this worker.
These functions are read-only: they can list family sessions, inspect one
session, and fetch bounded recent message previews. They cannot prompt, steer,
clear, kill, rename, or otherwise mutate another session.

Call them from the kernel:

```scheme
(define children (agent-roster #:children #t))
(define live (filter (lambda (child) (hash-try-get child 'active_session_id)) children))
(unless (null? live)
  (let ([name (hash-ref (car live) 'session_name)])
    (displayln (agent-roster name))
    (displayln (agent-roster name #:recent #t #:limit 6))
    ;; Deletion is a parent-owned RLM operation, not an observe mutation:
    (rlm-delete (car live))))
```

## API

- `(agent-roster)` returns `current` (the calling agent's own row) and
  `agents` (the family: parent, siblings, and direct children, active or not;
  the caller is excluded from `agents`). Each row carries `sessionId`,
  optional `sessionName`, `relationship` (`parent`/`sibling`/`child`),
  `status` (`running`/`idle` for live sessions, `inactive` without one), the
  live `activity` (`tool`/`model`/`compacting`/`busy`/`user`/`idle`),
  `isSessionActive`, the counts known for it (`messageCount`, `queuedCount`,
  `attachedClients`), `cwd` when the roster row carries it, the parent edges
  (`parentActiveSessionId`, `parentSessionId`) and `rlmChildId` for subagents,
  and `firstMessage` (capped at 240 chars) for an inactive child. A member
  with no live session has no `activeSessionId` and no live detail; address it
  by passing its row as `agent-send`'s `#:to`, or its `sessionName`, or its
  `sessionId` when the member has no name. For direct children,
  `(agent-roster #:children #t)` also exposes parent-owned lifecycle rows.
- `(agent-roster target)` returns one agent summary (the same shape as
  the roster rows, including a passive child's row). `target` is resolved like
  other live-session selectors: active id, session id or name, or an
  unambiguous id suffix.
- `(agent-roster target #:recent #t #:limit #:max-chars)` returns `agent` (the
  target's summary), `messages` (up to `#:limit` recent bounded previews,
  default 8), the clamped `limit` and `maxChars`, and `truncated` (true when
  older messages exist beyond the returned window). `#:limit` must be 1-50,
  and `#:max-chars` must be 80-2000 (default 800). Each preview's `index` is
  the message's position in the full transcript.

## Safety

- These functions are read-only and expose no mutation.
- Targets outside the nuclear family are rejected; transcript reads follow the
  same family rule as messaging.
- Message access is bounded by count and per-message character limit.
- Prefer status and recent previews for orchestration. Ask the user before
  using observed context to steer or message another session.
