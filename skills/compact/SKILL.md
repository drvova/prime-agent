---
name: compact
description: Check context usage and compact the conversation from the Scheme kernel. Use when context is filling up and substantial work remains, so the session is summarized and you keep working instead of stopping early.
---

# Compact

Compaction replaces older conversation history with a dense summary, freeing
context so long-running work can continue. The implementation lives in the
host (the same one behind the user's `/compact` command); these kernel
functions are the interface to it:

```scheme
(compact #:status #t)
(compact)
(compact "keep the failing test names and the migration checklist")
```

## API

- `(compact #:status #t)` — current context usage as a hash: `tokens`,
  `context_window`, and `percent` (unset right after a compaction until the
  next model response), plus `scheduled` (whether a requested compaction is
  already pending).
- `(compact [instructions])` — schedule compaction. Returns
  `{scheduled #t}`, or `{scheduled #f, reason}` when there is nothing to
  compact yet. Optional `instructions` focus the summary on what matters for
  the remaining work.

## Rules

- Compaction never runs mid-cell. A scheduled compaction runs when the
  current turn ends; the harness then resumes you automatically with the
  summary plus recent messages, and you continue the task.
- The Scheme kernel persists through compaction — everything you defined
  remains available.
- Compact at a natural boundary when context usage is high and substantial
  work remains, instead of becoming terse or returning to the user early.
  Check `(compact #:status #t)` when unsure.
- One request per turn is enough; calling `compact` again before the turn
  ends only updates the instructions.
