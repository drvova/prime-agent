# pa-runtime

Prime Agent's event-driven runtime: the bus every event goes through, the subscriptions that
turn events into work, and the per-session task lanes that run it. Being built; nothing uses it
yet. This is the storage-independent core: storage, the agent turn and the kernel come in later
phases behind the traits defined here.

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
- **Wire2 server** (`WireServer::serve`): one client connection over any `AsyncRead` +
  `AsyncWrite` pair (the daemon's local socket or named pipe; `tokio::io::duplex` in tests),
  framed by `pa_types::daemon::framing` with each frame's JSON as the header.
  - **Hello**: a first frame that is not `hello` at `WIRE2_VERSION` gets `closing` ("restart the
    client") and the connection ends; then `welcome` with the log's last sequence number.
  - **Subscribe**: replays `since` from the log (`now`: nothing; `after`: every matching event
    above `seq`; `tail`: the last `count` matching events, at most `MAX_TAIL`), then exactly one
    `backfilled{through_seq}`, then live events above it. No gap and no duplicate: the
    connection's listener is registered before `welcome`, the replay reads the log only up to its
    last event at that moment, and live events at or below `through_seq` are skipped. A live-only
    topic (none yet: the bus stores every event) has nothing to replay. A subscribe with an id in
    use restarts it.
  - **Lag**: frames wait in a bounded per-connection queue (`WireConfig::outbound_capacity`). A
    client that stops reading stalls only its own connection: the bus drops the oldest events for
    that listener alone, and the connection catches up from the log in order. Live-only events
    lost that way cannot be recovered.
  - **Publish**: only on `CLIENT_TOPICS` (`agent/<id>/input`), as `Trust::Owner`, with the
    client's `hello` name as `source` and `<id>` as the session; the reply is `{"seq": n}`.
  - **Query / call**: the wire keeps the operation's name a string (an unknown name costs one
    error reply, not the connection); the server parses it into `Query` or `Call`, refuses an
    unknown one, and runs `Handlers` on its own task, at most `IN_FLIGHT` per connection.
  - A malformed frame, a second `hello`, or an outbound frame over the frame limits ends the
    connection with `closing`.
- **In-memory implementations** (`MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`,
  `MemoryTaskStore`) for tests and headless runs.

## Non-goals
No storage engine (the SQLite log is `pa-log`'s, behind `EventStore`, `StateStore` and
`TaskStore`). No agent turn, model call, kernel or session actor (behind `TaskRunner`,
`ModelCaller` and `TaskSink`). No event sources (`pa-sources`, publishing through `Intake`). No
socket listener (the daemon accepts connections and hands each to `WireServer::serve`), and no
query or call backend (behind `Handlers`). No subscription persistence, live-only topics, or
trust gate yet. Nothing bounds how wide a cascade grows within the hop limit, or how many
events an `Intake` buffers.

## Public API
- `Bus`, `BusParts`, `Published`, `Listener`, `Heard`, `Intake`, `BusClosed`,
  `DuplicateSubscription`.
- `WireServer`, `WireConfig`, `Handlers`, `Query`, `Call`, `UnknownOperation`,
  `CLIENT_TOPICS`, `IN_FLIGHT`, `MAX_TAIL`.
- `EventStore` (`append`, `since`, `before`), `StateStore`, `StateKey`, `StateWrite`,
  `SessionTree`, `ModelCaller`, `ModelCall`, `TaskSink`, `TaskRequest`.
- `TaskQueue`, `TaskRunner`, `TaskOutcome`, `Steered`, `TaskStore`, `INTERRUPTED_BY_RESTART`.
- `MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`, `MemoryTaskStore`.
