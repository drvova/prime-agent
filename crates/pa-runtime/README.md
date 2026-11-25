# pa-runtime

Prime Agent's event-driven runtime: the bus every event goes through, the subscriptions that
turn events into work, and the per-session task lanes that run it. Being built; nothing uses it
yet. This is the storage-independent core: storage, the agent turn, the kernel and the wire
come in later phases behind the traits defined here.

## Scope
- **Bus** (`Bus::publish`): stores the event through an `EventStore` (which assigns id,
  sequence number and time), fans it out to `Listener`s, matches it against the subscriptions
  with `TopicTrie`, and runs the routes it sets off, breadth first.
  - **Fan-out** is a bounded broadcast. Publishing never waits for a listener; one that falls
    more than `listener_capacity` behind loses the oldest events and hears `Heard::Lagged`,
    then catches up from the log with `EventStore::since`. Listeners see sequence order.
  - **Loops**: an event a route causes is built with `Event::cause`, so it carries at most its
    cause's and the subscription's trust, and a chain of causes ends at `MAX_HOPS`. The event
    that would pass it is replaced by one `system/loop_dropped`, itself at the end of the
    chain, so nothing it would cause is published or reported again.
  - **Route failures** (a state write, model call or task hand-off that errs) publish
    `system/route_failed`, caused by the matching event.
  - **Intake**: a synchronous, non-blocking, unbounded way onto the bus for code that cannot
    await (an event source's callbacks); its task publishes in order. `pa_sources::Publisher`
    is implemented on it.
- **Subscriptions**: scope (`Local`: the subscription's session; `Agent`: it and its
  subagents, via `SessionTree`; `Global`: every event), then `when`, then throttle (at most one
  match per window; the rest dropped, not queued), then debounce (the route runs once, with the
  last match, after the quiet period), on Tokio's clock. Routes: `Set` writes a `StateStore`
  key in the subscription's session, `Publish` publishes a caused event in the matching event's
  session, `Call` hands a `ModelCall` to a `ModelCaller`, `Agent` hands a `TaskRequest` to a
  `TaskSink`. A route's trust is the lower of the subscription's and the event's.
- **Task lanes** (`TaskQueue`): one serial lane per session, highest priority first (lowest
  `Priority`), then oldest; sessions in parallel under a global `max_concurrent_runs`. A lane
  picks its next task only once it holds a run permit. Tasks go Queued → Running → Done, Failed
  or Cancelled, each state saved to a `TaskStore` first. A `TaskRunner` runs each task on its own
  Tokio task, so a panic fails only that task. A `Steer` task queued while its session runs one
  is offered to the running task (`TaskRunner::steer`): taken, it is Done; declined, it runs
  next. `cancel_queued` cancels a session's waiting tasks. `TaskQueue::restore` requeues the
  store's queued tasks in order and fails each running one with `"interrupted by restart"`.
- **In-memory implementations** (`MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`,
  `MemoryTaskStore`) for tests and headless runs.

## Non-goals
No storage engine (the SQLite log is `pa-log`'s, behind `EventStore`, `StateStore` and
`TaskStore`). No agent turn, model call, kernel or session actor (behind `TaskRunner`,
`ModelCaller` and `TaskSink`). No event sources (`pa-sources`, publishing through `Intake`). No
wire server: `Listener` is what one reads. No subscription persistence, live-only topics, or
trust gate yet. Nothing bounds how wide a cascade grows within the hop limit, or how many
events an `Intake` buffers.

## Public API
- `Bus`, `BusParts`, `Published`, `Listener`, `Heard`, `Intake`, `BusClosed`,
  `DuplicateSubscription`.
- `EventStore`, `StateStore`, `StateKey`, `StateWrite`, `SessionTree`, `ModelCaller`,
  `ModelCall`, `TaskSink`, `TaskRequest`.
- `TaskQueue`, `TaskRunner`, `TaskOutcome`, `Steered`, `TaskStore`, `INTERRUPTED_BY_RESTART`.
- `MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`, `MemoryTaskStore`.
