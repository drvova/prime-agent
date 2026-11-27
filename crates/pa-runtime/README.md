# pa-runtime

Prime Agent's event-driven runtime: the bus every event goes through, the subscriptions that
turn events into work, the per-session task lanes that run it, and the sessions that run each
task as an agent turn. The print and json modes run on it; the RPC mode moves onto it next. Storage is behind the traits defined here, with
SQLite implementations over `pa-log`.

## Scope
- **Bus** (`Bus::publish`): stores the event through an `EventStore` (which assigns id,
  sequence number and time), fans it out to `Listener`s, matches it against the subscriptions
  with `TopicTrie`, and runs the routes it sets off, breadth first.
  - **Fan-out** is a bounded broadcast. Publishing never waits for a listener; one that falls
    more than `listener_capacity` behind loses the oldest events and hears `Heard::Lagged`,
    then catches up from the log with `EventStore::since`. Listeners see stored events in sequence order; live-only ones lost to lag are gone.
  - **Loops**: an event a route causes is built with `Event::cause`, so it carries at most its
    cause's and the subscription's trust, and a chain of causes ends at `MAX_HOPS`. The event
    that would pass it is replaced by one `system/loop_dropped`, itself at the end of the
    chain, so nothing it would cause is published or reported again.
  - **Route failures** (a state write, model call or task hand-off that errs) publish
    `system/route_failed`, caused by the matching event.
  - **Live-only topics** (`Topic::is_live_only`: `agent/<id>/stream`, `agent/<id>/tool`) are
    fanned out to listeners and never appended to the `EventStore`: `seq` 0, id `live-<n>`, no
    route runs. They do not wait behind an append in progress.
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
  next. `cancel_queued` cancels a session's waiting tasks. `TaskQueue::restore(session)` requeues
  that session's stored queued tasks in order and fails each running one with
  `"interrupted by restart"`; it never reads or touches another session's tasks.
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
    event has nothing to replay: it goes to every subscription whose filter matches as it is
    fanned out, outside the `through_seq` check, and never moves the connection's cursor. A subscribe with an id in
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
- **SQLite stores** (`log_stores`, over one shared `pa_log::Log`): `LogEventStore`
  (`append_event`, `events_since`, `events_before`), `LogTaskStore` (`save_task`,
  `unfinished_tasks(session)`, in first-save order, which `TaskQueue::restore` replays), and
  `LogStateStore` on `pa-log`'s `kv` table: partition `state/<session>` for a session's keys and
  `state` for global ones, the key itself as the `kv` key, the value stored as
  `{"value", "trust", "cause"}` JSON. No other `kv` partition starts with `state`. Writes await
  the writer's `Receipt`; reads run on Tokio's blocking pool, never on the runtime's thread.
- **Runtime** (`Runtime::open(agent_dir, RuntimeOptions)`): the bus over the agent dir's
  database (the transcript store's `agent.db`, `pa_core::session::store::database_path`; opened
  on the blocking pool, no network), the task queue over it, `RuntimeOptions::spare_kernels`
  Scheme kernels kept built by pa-core's spare pool, `RuntimeOptions::sessions` opened while the
  database opens (their engines build meanwhile, so the open costs a first turn nothing), and one
  subscription installed at open: `agent/+/input` (an `AgentInput`) queues a `Prompt` task in
  that session (`AgentTarget::Origin`; the input's own `priority`, such as `steer`, when it names
  one; its images ride beside the task in memory, which the task store does not keep).
  `create_session(SessionSpec)` opens a session and returns its id (minted when the spec has
  none). A session's stored tasks are restored (`TaskQueue::restore`) when it is opened, at open
  for `RuntimeOptions::sessions` or by `create_session`; only the sessions a runtime opens are
  restored, under the caller's hold on them (print mode's `SessionLease`), so two processes on
  one `agent.db` never fail or run each other's tasks. `publish`, `listen`, `cancel(session)`
  (its queued tasks, and the running one's turn is aborted: it ends `Cancelled`),
  `idle(session)`, `settle(session, SettleAfter)`, `finish(session)` and `shutdown`. `Call`
  routes are refused (`system/route_failed`): the runtime
  makes no model calls yet.
- **Sessions**: each on its own OS thread and multi-threaded Tokio runtime (as the daemon's
  session host), entered with its `pa_types::session_scope` (id, cwd, environment). The
  embedder's `SessionBuilder` builds the engine there, so the composition root picks provider,
  settings and session manager, what the turn driver reports (`DriverEvents`: its json events
  on the stream, or quiet as the text print mode), the autonomous run it starts with, and its
  `InlineSubagents` if it hosts any; each build is a fresh engine whose kernel comes from the
  spare pool, never a copy. At build the stream's listener, the goal usage accounting, the
  autonomous accounting and the natural-turn-end continuation hook are wired on, in that
  order. A task runs a session command (`/compact`, `/refine`, `/goal`, `/autonomous`,
  `/better`) at the boundary with the continuation it schedules, or a turn: the pre-turn
  boundary, `prompt_with_auto_retry`, a steer the run took but did not read, the settled-turn
  boundary, then the goal's queued turns and, when the goal does not own the wake-up, the
  autonomous run's held ones. Every agent event is published live on `agent/<id>/stream` as
  `session_events::agent_event_json` gives it, with the driver's events and the auto-retry
  events in the same json-stream shapes; then `agent/<id>/done` (stored, an `AgentDone`: task,
  outcome, error, and the `TaskFailure`: a failed reply, command or turn), queued behind the
  stream, with the task's trust. A turn whose last reply is a failed model request fails its
  task as a failed reply.
- **Settle and finish**: `settle` drives the run to rest on the session's thread (its subagents
  to quiescence with the turns their rows ask for, the goal's owed continuation, the autonomous
  continuation held behind them, and the boundary drains of what those turns arm) and answers
  a `Settled` (the headless terminal result, the goal stopped at its no-progress cap during the
  run, the autonomous exit failure); `SettleAfter::Task(input)` arms it behind that input's task,
  so nothing of the session runs between them, and answers `None` when that task stopped the
  run. `finish` drains the refinements the turns started and ends the subagents, answering
  whether a `/refine` command's refinement failed. Both answer behind the events they streamed. A `Steer` task offered while
  a turn runs is handed to the agent's steering queue and read at its next step. A panic in a
  turn is caught at the session thread's work loop: that task fails with the panic's text, the
  lifetime ends (its scope ended, its runtime shut down), and the next task runs on a fresh
  engine. Events reach the bus through one ordered outbox on the runtime's own task, so nothing
  a route sets off runs on a session's runtime.
- **Agent routes** (`RuntimeTaskSink`): the target session is named, the event's own
  (`Origin`), or opened for the event (`Spawn`, in its `cwd` or the runtime's, recorded as a
  subagent of the event's session); an empty route prompt takes the event's `AgentInput`.
- **Turn driver** (`turn/`, moved from pa-cli's print runtime, run by the sessions above):
  `TurnBoundary` (the overflow compact-and-retry, the requested and threshold compaction arms,
  the auto-refine reviews and the disposal drain; public, so an embedder's `InlineSubagents`
  drive their turns through it), the goal surface (goal updates, queued goal turns, budget
  steers, the backoff wake), the autonomous run (gate commands after each settled turn, its
  continuations and exit contract), the natural-turn-end hook composing the goal and
  autonomous arms (both defer behind unsettled subagent work), the session slash commands,
  and the auto-retry events. Their json events go to the session's stream; a
  `TurnBoundary::new` prints its own on stdout.
- **In-memory implementations** (`MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`,
  `MemoryTaskStore`) for tests and headless runs.

## Non-goals
No storage engine (the SQLite log is `pa-log`'s, behind `EventStore`, `StateStore` and
`TaskStore`). No engine assembly (the embedder's `SessionBuilder`). No model calls for `Call`
routes yet (behind `ModelCaller`). No event sources (`pa-sources`, publishing through `Intake`).
No socket listener (the daemon accepts connections and hands each to `WireServer::serve`), and
no query or call backend (behind `Handlers`). No inline-subagent host (the embedder's
`InlineSubagents`; the print modes' lives in pa-cli). No subscription persistence or trust gate yet; a
task's `done` carries no cause chain yet. Nothing bounds how wide a cascade grows within the hop
limit, or how many events an `Intake` or the sessions' outbox buffers.

## Public API
- `Bus`, `BusParts`, `Published`, `Listener`, `Heard`, `Intake`, `BusClosed`,
  `DuplicateSubscription`.
- `WireServer`, `WireConfig`, `Handlers`, `Query`, `Call`, `UnknownOperation`,
  `CLIENT_TOPICS`, `IN_FLIGHT`, `MAX_TAIL`.
- `EventStore` (`append`, `since`, `before`), `StateStore`, `StateKey`, `StateWrite`,
  `SessionTree`, `ModelCaller`, `ModelCall`, `TaskSink`, `TaskRequest`.
- `TaskQueue`, `TaskRunner`, `TaskOutcome`, `Steered`, `TaskStore`, `INTERRUPTED_BY_RESTART`.
- `MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`, `MemoryTaskStore`.
- `LogEventStore`, `LogTaskStore`, `LogStateStore` (each `new(Arc<pa_log::Log>)`;
  `LogStateStore::get`).
- `Runtime` (`open`, `publish`, `listen`, `create_session`, `cancel`, `idle`, `settle`, `finish`,
  `shutdown`), `RuntimeOptions`, `SessionBuilder`, `SessionSpec`, `BuiltSession`, `DriverEvents`,
  `InlineSubagents`, `SettleAfter`, `Settled`, `Finished`; `TaskQueue::session_idle`.
- The turn driver's `TurnBoundary`.

Dependencies: `pa-types`, `pa-log` (the SQLite stores), `pa-core` (the session engine, the
kernel's spare pool, the turn driver's compaction, refinement, goal and autonomous seams) and
`pa-agent` (the agent loop's steering, abort and events), among workspace crates.
