---
name: rlm-heartbeat
description: Manage agent-owned RLM heartbeats from the Scheme kernel. Use when the user asks the agent to start, create, schedule, or manage a heartbeat, unless they explicitly request the user's /heartbeat.
---

# RLM Heartbeat

RLM heartbeats are internal recurring prompts for the current agent session.
They are separate from the user's visible `/heartbeat`: these functions cannot
read, replace, pause, resume, or clear that user-level heartbeat.

Call them from the kernel:

```scheme
(event-define "tests" #f #:kind "heartbeat" #:prompt "check test progress" #:timer (hash 'every "5m"))
(event-define "build" #f #:kind "heartbeat" #:prompt "watch build" #:delivery-mode "follow_up")
(event-list #:kind "heartbeat")
(event-define "tests" #f #:kind "heartbeat" #:prompt "check test progress" #:status "pause")
(event-remove "tests" #:kind "heartbeat")
```

## API

- `(event-list #:kind "heartbeat" #:include-inactive)` — list this session's
  internal RLM heartbeats. By default this includes active and paused entries.
- `(event-define name #f #:kind "heartbeat" #:prompt instruction #:timer
  #:delivery-mode #:status)` — create the recurring heartbeat `name` for this
  session, or update the one of that name (by label or id). The default
  interval is every 5 minutes; `#:timer (hash 'every "10m")` sets another.
  Multiple RLM heartbeats may run at once; their names distinguish them.
  `#:delivery-mode` is `"steer"` (default) or `"follow_up"`; `#:status` may be
  `"pause"` or `"resume"`.
- `(event-remove name #:kind "heartbeat")` — cancel one RLM heartbeat by name
  or id.

## Delivery mode

Each heartbeat has a delivery mode controlling how the scheduled prompt reaches
the session when it is busy:

- `steer` (default): interrupt the current turn so the heartbeat runs promptly.
- `follow_up`: wait for the current turn to finish before running the heartbeat.

## Rules

- Use this when the user asks you to start, create, schedule, or manage your own
  heartbeat without explicitly referring to `/heartbeat`.
- Use this only for agent-internal recurring checks and long-running task
  coordination.
- Do not use these functions to satisfy a user's request to configure
  `/heartbeat`; that is a separate user-level surface.
- Keep heartbeat instructions specific and actionable so each recurring turn
  knows exactly what to inspect or continue.
