# pa-runtime

Prime Agent's event-driven runtime: the bus every event goes through, the subscriptions that
turn events into work, the per-session task lanes that run it, and the sessions that run each
task as an agent turn. The print and json modes run on it, and the RPC mode (`rpc`) and the ACP
mode (`acp`) live here beside them. Storage is behind the traits defined here, with
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
    chain, so nothing it would cause is published or reported again. A task an `Agent` route
    queues publishes what its event causes (its `done`, its kernel's publications, its agent
    messages, each one hop past the task's trigger), so an event on the end of the chain queues
    no task: one `system/loop_dropped` is published in its place.
  - **Route failures** (a state write, model call or task hand-off that errs) publish
    `system/route_failed`, caused by the matching event.
  - **Live-only topics** (`Topic::is_live_only`: `agent/<id>/stream`, `agent/<id>/tool`) are
    fanned out to listeners and never appended to the `EventStore`: `seq` 0, id `live-<n>`, no
    route runs. They do not wait behind an append in progress.
  - **Intake**: a synchronous, non-blocking, unbounded way onto the bus for code that cannot
    await (an event source's callbacks); its task publishes in order. `pa_sources::Publisher`
    is implemented on it (a gone bus is `PublisherClosed`).
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
  next. A `Steer` task that runs as its own is offered the `Steer` tasks queued behind it as it
  starts (the runner keeps them for that run), so steers that waited together are delivered
  together. `push` answers how the task was admitted (`pa_types::event_log::Admission`:
  `queued`, `steered`, or `parked` in a suspended lane). `TaskRunner::lane_changed` hears what
  a lane holds (`LaneState`) after each task joins it, leaves it to run, ends there or is
  cancelled out of it, under the queue's lock, so its reports arrive in order. A lane can be suspended: its queued tasks
  stay queued and none starts until it is resumed; the running one runs on. `cancel_queued`
  cancels a session's waiting tasks. `TaskQueue::restore(session)` requeues
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
    client's `hello` name as `source` and `<id>` as the session; the reply is `{"seq": n}`, with
    `task` and `admission` when its routes announced the task on `agent/<id>/accepted`.
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
  settings and session manager, what the turn driver reports and in which shapes the stream
  carries it (`DriverEvents`: the json stream's shapes, quiet as the text print mode, or the old
  daemon's session-event shapes for the runtime daemon's sessions) and the autonomous run it
  starts with, and it
  installs the `SessionSeams` the runtime hands it; each build is a fresh engine whose kernel comes from the
  spare pool, never a copy. At build the stream's listener, the goal usage accounting, the
  autonomous accounting and the natural-turn-end continuation hook are wired on, in that
  order. A task runs a session command (`/compact`, `/refine`, `/goal`, `/autonomous`,
  `/better`) at the boundary with the continuation it schedules, or a turn: the pre-turn
  boundary, `prompt_with_auto_retry`, a steer the run took but did not read, the settled-turn
  boundary, then the goal's queued turns and, when the goal does not own the wake-up, the
  autonomous run's held ones. Every agent event is published live on `agent/<id>/stream` by the
  session's `SessionStream` (`session/stream.rs`, every publish of the session's goes through
  it): in the json stream's shapes as `session_events::agent_event_json` gives it, with the
  driver's events and the auto-retry events verbatim; or, for `DriverEvents::SessionEvents`, as
  the old worker's `session_event.event` payloads built with `pa_core::session_engine::wire_events`
  (a reply's `message_start` with its `start`, the deltas coalesced per run with their live
  `contextUsage`, a tool result as a `message_start`/`message_end` pair, the billing stamp on
  settled assistant rows, `settled_context_usage` on every settled conversation row's
  `message_end`, `compaction_summary_delta` while a summary is written; a tool's update and end
  name no tool, as the old frames do not), each frame, driver events, `!` commands and child
  updates included, numbered with the session's `meta.sequence`, a parked update consuming its
  number as it parks. In those shapes the stream carries the old worker's queue view,
  `session_action_update` (`session/stream/actions.rs`), built from the lane's reports and the
  rows waiting on the agent: a task queued behind work (its lane running, holding a task or
  stopped) is listed in its lane (`Steer` under `steering`, the rest under `followUps`), a
  task that joined an idle lane never is, the waiting rows follow with `rlmChildStatus` marking
  a child's status notice; the task leaving the lane is the active `turn`, `preparing` with its
  row's text as label (a wake task's, its first waiting row's), `committing` once its first user
  or custom row settles, `running` from its reply's first frame, and gone before its `done`; an
  unchanged view, and the idle one a session starts from, is not sent. The turn driver's own
  projection (the goal surface's frames) is not sent there, and a session command's task is
  framed as a run (`agent_start`, `turn_start`, its rows, `turn_end` with its failure if it
  failed, `agent_end`), as the old worker framed it. Goal-minted turns (a budget steer, a held
  continuation) are not lane tasks and are not in the view yet. The update flush runs at the
  fixed 50 ms the old pacer starts from, without its backlog feedback: the backpressure point
  here is each connection's listener (16,384 events behind before it loses any and catches up
  stored events from the log), a lost live update is superseded by the next (each carries the
  whole message) and the snapshot covers the rest. Then `agent/<id>/done` (stored, an `AgentDone`: task,
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
  a turn runs is handed to the agent's steering queue and read at its next step, unless it
  carries less trust than the running task: then it runs as its own task after. A panic in a
  turn is caught at the session thread's work loop: that task fails with the panic's text, the
  lifetime ends (its scope ended, its runtime shut down), and the next task runs on a fresh
  engine. Events reach the bus through one ordered outbox on the runtime's own task, so nothing
  a route sets off runs on a session's runtime.
- **Agent routes** (`RuntimeTaskSink`): the target session is named, the event's own
  (`Origin`), or opened for the event (`Spawn`, in its `cwd` or the runtime's, recorded as a
  subagent of the event's session); an empty route prompt takes the event's `AgentInput`. The
  task's trigger is the cause of what its turn publishes for others: its `done`, its kernel's
  publications and its agent messages carry the task's trust and sit one hop past the trigger
  (a task restored after a restart starts its chain again). The runtime's own routes queue no
  task of their own (`RouteKind`: a subagent's `done`, the inbox, the kernel notices). An
  untrusted task's prompt is queued as `<untrusted-event source="<event source>">…</untrusted-event>`
  (the source escaped, a closing tag in the text neutralized); the system prompt is untouched.
  A task queued for a session's own input is announced on `agent/<id>/accepted` (an
  `AgentAccepted`: the task, its priority and its `Admission`), caused by the input: a `TaskSink`
  may answer an `Announced` event, which the bus publishes, so it is in the input's
  `Published::caused`. A steered task has no `done` of its own; a queued or parked one ends with
  its `agent/<id>/done`, so a prompt-and-wait waits for that.
- **Steering mode**: a session built with `QueueModes::Settings` (the runtime daemon's) reads
  the global settings' `steeringMode` and `followUpMode` as each turn starts (`all`, the default
  steering mode: the steers waiting at a step are read together; `one-at-a-time`: one per step);
  one built with `QueueModes::Built` (the print modes') keeps the engine's. A steer taken by a running turn is read at its next step; one
  offered before the turn reached its thread is kept for it and read as it starts. Cancelling the
  running task withdraws the steers its turn took and had not read and hands them back to be
  queued again.
- **Trust gate** (`trust/`): the session's host sets the running task's trust as the task
  starts and clears it as it ends (`Owner` between tasks: a kernel prewarm, a background
  command's report); the builder gets a `TrustGate` over it and installs it as the engine's
  `pa_core::kernel::CallPolicy`, so the kernel asks it before every host request and every
  side-effecting native. An owner's task is admitted everywhere. An untrusted task is admitted
  by the class `trust/classes.rs` gives the call: `fs.read`/`fs.grep`/`fs.glob`/`fs.find` when
  each path in the payload (its `path` or `root`, and any `cwd`) resolves under the session's
  directory, links followed, and a glob's pattern is relative with no `..`; the session's own
  `ctx.*`, `todo.*` and `rlm.end_turn`; the kernel's reports about its own work
  (`bash.completed`/`consumed`/`quiet`, `cell.completed`/`consumed`, `call.refused` and the
  `*.fact`, `tests.summary`, `rizin.used`, `factory.progress` counts); and of the natives only the
  file readers. Everything else (bash and its jobs, file writes, worktrees, hypotheses, the
  harness store, factories, rizin, MCP, every `web.*` request (a URL could carry out what the
  turn read), spawning, messaging, scheduling,
  settings, observing other sessions, questions) and any request the table does not list is
  refused: "refused for an untrusted turn: <call>". The table's guard fails while pa-core
  registers a request it does not list (the scan is pa-core's `tests/common/host_requests.rs`).
- **Kernel events** (`events.rs`, `BusEvents`): every session's `event.*` host requests
  (pa-core's `EventBusController`, registered through `register_event_bus_host_handlers` into
  the handlers the builder gets) reach one bus per runtime, under pa-core's rules
  (`EventBusState::apply`: the daemon's names, limits, schema checks, ownership and refusals),
  its state kept in the agent dir's database as the daemon's is (the `event-bus.json` partition
  of the agent dir; no file). A publication is a stored event on `source/<name>/published`
  (the checked payload, `source` the owner's name, `session` its id, the publishing task's trust,
  caused by its trigger; `Owner` and no cause between tasks); its answer
  `{eventId, matched}` names that event. A subscription is a bus subscription
  `event-bus/<id>` on `source/<name>/#`, or on the topic filter it names (any text with `/`),
  its filter the `when` (each key's value equal), routed to its session at `FollowUp` or
  `Steer` priority; the task starts from the message the daemon delivers
  (`[agent-message from event <source>]`, then pa-core's `event_delivery_message`) and counts as
  delivered. A session never gets its own events, and a session the runtime does not run gets
  none (a subscription restored from an earlier run waits for its session).
- **Subagents** (`subagents/`): every open session has a node in the runtime's family tree.
  `rlm-spawn` gates on pa-core's `rlm_depth` bound (a root's resolves when its engine is
  built; a child's is its parent's, fixed), checks the name, and opens the child as a runtime
  session (`SessionSpec::subagent`: its parent, handle, depth and requested model, which its
  builder resolves; a build error is the spawn's error), with the parent's cwd (or the one it
  asks for), environment and running task's trust; the child's session gets its own trust
  gate. The task is delivered as the parent's `agent_message` row and run by an empty
  `AgentInput` on `agent/<child>/input` (an empty input runs the rows delivered to a session
  as the quiescence drive runs them; a subagent's task ends at rest, so its `done` follows its
  subtree). The parent hears the child through the subscription `subagent/<child>` on
  `agent/<child>/done`, whose route queues no task: it settles the child (its answer and
  verdict) and steers the no-reply notice or the failure row onto the parent's agent. The
  family's `agent_message.*` publishes the same row on the receiver's `agent/<to>/inbox`, and the
  runtime's inbox subscription (`runtime/inbox`) delivers it as the daemon's receiving worker
  does: refused while the receiver holds 20 undelivered rows, queued without spending a wake to a
  receiver in a turn, and otherwise a wake of the idle receiver, at most 20 per sender per
  receiver in 10 minutes (pa-core's constants; past them the receipt is `wake_refused` with
  `retryAfterMs`, and nothing is delivered); the sending side keeps pa-core's message limit and
  rate bucket. A delivered row is steered onto the receiver, and a settled child is given its
  next task over it; in the runtime daemon a root is woken for it (below), and the delivery,
  once it landed, is the traffic pane's entry on `runtime/traffic` (live only: the old
  `agent_traffic` entry with the receiver's root, the sender's relationship to it, the outcome
  and a sanitized preview). The spawn's and the retask's empty inputs carry no cause: the depth bound
  and the wake budget bound them. The kernel's notices (a background command's end, loss or
  silence, a background cell's end, a factory milestone) are admitted by the session's notice
  sink as the daemon's worker admits them, at once: a `Steer` one steers its row onto the
  session's agent, a `WakeIdle` one (`[bash-quiet]`) too unless the session is in a turn,
  where it is dropped, and a `Record` one is only kept; each is then stored on
  `session/<id>/bash/quiet` (a `[bash-quiet]`) or `session/<id>/bash/done` (every other) as
  `{taken: steered | recorded | dropped_busy, row}`, and the runtime's notice subscription
  (`runtime/bash`) wakes the session for a steered one: its drive looks again and a settled
  child is given its next task. In the runtime daemon a root is woken for every row delivered
  to it (a settled child's notice, a delivered agent message, a steered kernel notice), as the
  old daemon's notice turn: a `Steer` empty input on `agent/<id>/input`, which a running turn
  that still reads steering takes with nothing to hand over (the row is already on its agent)
  and which otherwise runs as its own task; a root's empty-input task runs the rows on its own
  agent (`Rest::OwnRows`) and ends without waiting for its children, whose settles wake it
  again. A headless root queues no wake: its settle drives the rows. `rlm-collect` waits on the parent's activity (each settle fires
  it) to its deadline, and ends with its cell when the kernel stops it; `rlm-list-subagents`,
  `rlm-delete-subagent` (the child and its subtree cancelled, their kernels disposed and
  sessions closed) and `rlm-rename` read and change the roster. A session is at rest when no
  child is unsettled, no late receipt is pending, no row waits on its agent and no background
  command runs: the settle drives it there, the goal and autonomous continuations wait for it,
  and `finish` ends every subagent.
- **Turn driver** (`turn/`, moved from pa-cli's print runtime, run by the sessions above):
  `TurnBoundary` (the overflow compact-and-retry, the requested and threshold compaction arms,
  the auto-refine reviews and the disposal drain), the goal surface (goal updates, queued goal turns, budget
  steers, the backoff wake), the autonomous run (gate commands after each settled turn, its
  continuations and exit contract), the natural-turn-end hook composing the goal and
  autonomous arms (both defer behind unsettled subagent work), the session slash commands,
  and the auto-retry events. Their json events go to the session's stream (a
  `TurnBoundary::new`, the tests' own, prints its events on stdout).
- **Sources** (`sources.rs`, `Runtime::define_source(SourceDefinition)`): timer and
  folder-watch sources (`pa-sources`), by name. A definition (kind, options, the session that
  defined it) is a row of agent.db's `sources` table in the partition `runtime-sources`, which
  meets neither pa-core's scheduled jobs (`cron:<path>`) nor the kernel's bus sources (the
  `event-bus.json` partition). It is checked as it is defined (one topic level, the kind's
  options); defining a name again the same way changes nothing, another way is refused, and a
  webhook is refused ("webhook sources arrive with the daemon"). A source runs while a bus
  subscription's filter (`TopicFilter::matches`) matches a topic it publishes on: `timer/<name>`
  (Owner), or `source/<name>/changed` and `source/<name>/error` (the watch's `trust`, Owner by
  default). The registry tells the sources' task of every subscription added or removed, by any
  path (`Runtime::add_subscription`, the kernel's, the subagents'), without blocking; that task
  starts a source at the first such subscription and stops it, waiting for its work to end, at
  the last. Events reach the bus through an `Intake`. Each timer fire's `firedAt` is kept in the
  row's claim (`{"lastFiredMs"}`), written through the database's writer without waiting, so a
  timer started again, in the run or after a restart, catches up under pa-sources' rule (missed
  fires collapse into one; the next counts from it). `running_sources` names those running.
  `shutdown` stops every source and stores the last claims first; nothing else waits on a source,
  so a run's settle does not. A definition's `SourceLifetime` says whether it is stored: `Kept`
  is the row above, `Run` (the kernel's) is held in memory for the run and never written.
  `undefine` (the kernel's `event-source-remove`) stops a running source before it answers.
- **Kernel timer and watch sources** (`events.rs`): `event-source-define` with `#:timer` or
  `#:watch` is a bus source record under pa-core's rules (`SourceKinds::Accepted`: one namespace
  and the 32-source limit across kinds, owner-only removal, its kind's fixed payload schema, no
  `event-publish`), and also a `Run` definition of the sources above, owned by the defining
  session; one the sources refuse (a bad schedule, a name they hold another way) is taken off
  the bus again and the refusal answered. A watch's `path` resolves against the session's
  working directory and must exist under it, links followed (`trust::WorkingDir`, the trust
  gate's read rule). A subscription naming a timer hears `timer/<name>`; one naming a watch,
  `source/<name>/#`; its turn reads `[agent-message from event <name>]`. pa-core leaves these
  sources, and the subscriptions naming them, out of the bus's stored rows, so nothing of them
  outlives the run.
- **RPC mode** (`rpc`, `prime-agent --mode rpc`, moved from pa-daemon in phase 7f): JSON
  commands on stdin, responses and the session's events on stdout, in the TS `modes/rpc`
  shapes. `rpc::run_rpc_mode(RpcOptions)` serves one in-process session engine the composition
  root assembles (`RpcEngineHandle`, holding the session's lease) and replaces it whole through
  the root's `RpcEngineFactory` (`RpcEngineRequest`) for `new_session`, `switch_session` and
  `fork`, whose branched file `SessionManager::create_branched_file` writes. Events are buffered
  while a prompt's response is pending, so the response comes first; `prompt` answers at
  admission; `steer`/`follow_up` queue on the agent and a pump delivers the queue turn by turn,
  parked by `abort` until the next prompt-family command; stdin's end settles the turn and exits
  0, SIGTERM/SIGHUP exit 143/129. The daemon-only families answer the TS in-process errors. Its
  commands drive the engine directly, not the runtime's task lanes: the RPC semantics (a prompt
  refused while a turn streams, a follow-up folded into the running run, parked rows folded
  into the next prompt's run) are the agent's own queue's, not a lane's.
- **In-memory implementations** (`MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`,
  `MemoryTaskStore`) for tests and headless runs.
- **Runtime daemon** (`daemon`, opt-in until phase 8: `PRIME_AGENT_RUNTIME_DAEMON=1` or the global
  setting `"daemon": "runtime"` makes `prime-agent --mode daemon` start it instead of the
  supervisor). `daemon::run(listener, DaemonOptions, shutdown)` opens a `Runtime` over the agent
  dir and serves one `WireServer::serve` per connection on the listener the embedder bound at
  `daemon::socket_path` (`daemon2.sock` beside the old daemon's socket; on Windows the old pipe
  name with `2` appended), through `pa_types::platform::transport` as the old daemon's socket is.
  The embedder takes `pa_core::platform::RuntimeDaemonLock` (an OS file lock on
  `<agent dir>/runtime-daemon.lock`, its pid, socket and restart owner inside) with
  `daemon::take_lock` and holds it for the process's life: a second runtime daemon on the agent
  dir refuses to start, and the supervisor's boot reap spares the holder and its restart owner.
  A holder that does not answer a wire2 hello on the same socket within 3 s is a wedged
  predecessor: `take_lock` stops it (`RuntimeDaemonLock::reap_holder`) and then takes the lock. Its
  `Handlers`: `sessions.list` (the sessions dir's saved sessions and the open ones, or
  `{"open": true}` the open ones only; each row in the old daemon's session-summary shape,
  `daemon/handlers/summary.rs`: the session's id as `id`, `activeSessionId` and `sessionId`, its
  name, model, times and message count from its transcript, and for an open one its lane's
  activity, waiting prompts and `attachedClients`, the connections whose subscriptions match its
  `agent/<id>/done`, `wire::Attached`), `session.snapshot` (header, name, model, thinking level,
  entry count, leaf, open, and while it is open the stream's last `meta.sequence` as
  `lastEventSequence`) and
  `transcript.page` (`{session, before?, limit?}`: the stored rows before `before`, at most 500)
  read the transcript store; with a `shape` they answer the old worker's replies, which pa-tui's
  `wire2_compat` maps `attach`, `get_state` and `get_messages` to (`daemon/handlers/attach.rs`):
  `session.snapshot {shape: "attach", clientId?, capabilities?, resumeCursor?}` the attach
  result (summary, connection state, the display window of a `windowed_snapshot` client with
  `historyBeforeEntryId` and `lastUserPromptMs`, `lastEventCursor` from the stream's generation
  and counter, `replay`), `{shape: "state"}` the summary, and `transcript.page {shape:
  "messages", page?, historyLeafId?}` the old cursor page (`firstEntryId`, `lastEntryId`,
  `more`, `historyLeafId`; "History branch changed" for an anchor off the branch), the rows and
  cuts from `pa_core::session::display_transcript`. One row differs from the old daemon's by
  design: the harness digest (`[harness-digest]`, pa-core `session_engine::harness_digest`) is
  stored as the engine writes it, at a session's open when its context is not empty (a stale
  digest appended without events) and with the first turn's prompt otherwise, as the TS
  product, print mode and RPC mode on the same engine store it. The old worker kept its engine's
  session manager in memory and dropped the digest from what it saved, so on the runtime daemon
  a reopened session's transcript holds one more row, and an attach window over it starts one
  row later; `attach_paging.diff` pins that offset. The agents view's feeds, which
  `wire2_compat` maps `roster_subscribe` and `agent_traffic_subscribe` to
  (`daemon/handlers/feeds.rs`): `runtime.roster` answers `{roster}`, one old `AgentRosterEntry`
  per open session and per subagent below one (`agentId`, the attach's summary without
  `sessionActions` and with a subagent's `parentSessionPath`, `parentActiveSessionId`,
  `rlmChildId` and `runtimeKind: "subagent"`, `status`, `workerId`), and the live-only
  `runtime/roster` carries `{changed: [entry]}` when an entry composed again after its session's
  stream events differs from the one published last, and `{changed: [], removed}` when a session
  stops or is given up on, or a subagent is deleted, with the subagents below it;
  `runtime.traffic` answers `{entries}`, each root's newest 200 traffic entries, roots in order.
  Neither waits on a turn: one task hears the bus and marks entries due, another composes them;
  the agents view's saved-session catalog, which `wire2_compat` maps `list_saved_sessions` to
  (`daemon/handlers/saved.rs`, the old supervisor's handler over `pa_core::session::catalog`):
  `sessions.list {shape: "saved", stream?, activeSessionId? | cwd, sessionDir?, scope?, limit?,
  cursor?}` walks the sessions dir newest first and, with `stream`, publishes on the live-only
  `runtime/session_list/<stream>` one old `session_list_item` (without its `type`) per row as
  its file folds, each followed by its `session_list_progress`, then the walk's completion
  (`loaded == total`); the walk never waits for the topic (a full queue drops the progress,
  then the row), and the reply is the old response's `data` (`sessions`, each with its deleted
  subagents' spend, `remaining`, `nextCursor`), the catalog. A summary's `usage` is the
  catalog's own-usage fold of the transcript with its billing split, and its `modified` is the
  session's creation before its transcript has a row, as the old worker's were. Not yet: a
  saved row's live persistent shells and stored factory specs, and subagents only the spawn
  ledger keeps (the old daemon's passive walk);
  `session.create` (`{session?}` to open or create by id, or `{path}` to
  open a transcript by its file, and `cwd`) takes the session's `SessionLease` and holds it until
  `session.stop` or the daemon's end, so a session the old daemon or a print run holds is refused
  (`session_already_active`) and the old daemon is refused one this daemon holds; a session it
  holds open already (revived at boot, opened by another client) is answered as it runs
  (`"reused": true`), the old daemon's create-open reuse;
  `session.rename`, `session.label` and `session.set_effort` change an open session through its
  engine (on the session's runtime, once its first build is done: `Runtime::built_engine`, which
  the state queries wait on too) and a saved one under its lease; `session.delete` changes saved
  sessions only. `session.create` also takes `options` (`SessionOptions`: `provider`, `model`,
  `thinking`, `systemPrompt`, `appendSystemPrompt`, `skills`, `promptTemplates`, `autonomous`,
  `goal: {objective, tokenBudget?}`, `executionMode`; an unknown key is refused), the launch
  choices an ACP client forwards, handed to the builder on the `SessionSpec` and kept under `kv`
  `runtime-daemon`/`options:<id>`, so the session's revival and scheduled wake build it the same
  way and a create without options keeps them. `session.create {"ephemeral": true}`
  (`daemon/handlers/owned.rs`, `--no-session`) opens a new session kept in memory only (the
  builder gets `SessionSpec::ephemeral`): no transcript file, options row, lease or held-session
  record, owned by the creating connection's `hello` name and stopped on `session.stop` or 30 s
  after that client's last connection ended (the old daemon's owned-worker grace). The runtime's
  stores keep nothing of it or of the subagents it spawns (`log_stores.rs`, `Unstored`): its
  events run their routes and reach live listeners but are not appended (`seq` 0, an
  `unstored-` id, delivered by the wire feed as a live-only event is), its tasks are not saved
  and no state is written for it. The list's
  rows also carry the old summary's `name` and `firstMessage`. For the ACP client (`daemon/handlers/model.rs`,
  `mcp.rs`, `bus.rs`): the `models` query lists the signed-in models; `session.set_model`
  (`{session, provider, modelId}` or `{session, model}`) resolves through the model registry,
  enforces the settings' `allowedModels` through pa-core's gate, the old daemon's too
  (`pa_core::models::{load_model_allowlist, assert_model_allowed}`: unreadable global settings or a
  malformed `allowedModels` refuse every model), and on an
  open session switches the running agent, records the `model_change` row through its engine
  and makes it the settings' default, the old daemon's `set_model`, and retargets the session's
  next request: the slot its stream reads its provider target from (`BuiltSession::
  provider_target`, which the embedder's build hands over) gets the model, its request auth
  (through the session's credential pin; a model whose auth does not resolve is refused before
  anything moves) and its active tier, and the turn boundary's model facts follow;
  `session.set_mcp_servers`
  (`{session, owner, servers}`) replaces the MCP servers one client supplies an open session in
  its engine's MCP manager, fenced by the owner id (another owner's list is refused, its clear
  changes nothing), refused while a turn runs, a failed replacement rolled back, the old daemon's
  `replace_acp_mcp_servers`; the `sources` query lists the kept timer and watch sources (with
  whether each runs) and the run-defined ones running, `subscriptions` the bus's subscriptions,
  and `settings` (`{session?}`) the effective settings for a session's or the daemon's cwd with
  what could not be read. A scheduled job made, changed or cancelled publishes
  `agent/<id>/schedules` (`{"active": n}`) once its timer is in place, the old daemon's
  `heartbeats_changed`. The user's `!` commands (`daemon/handlers/bash.rs`, over pa-core's
  `pa_core::user_bash`, the old daemon's `execute_bash`/`abort_bash`): `session.bash`
  (`{session, command, excludeFromContext?, transient?, runId?}`) answers `{session, started}`
  as the command starts on the session's runtime, refused while another runs or the session
  stops; the run streams `bash_start`, a `bash_output` per sanitized chunk and `bash_end` on
  `agent/<id>/stream`, and unless `transient` its `bashExecution` row is saved and joins the
  agent's messages (with no turn running, under the agent's idle reservation; a running turn
  takes it as a steer). `session.abort_bash` (`{session}`) kills it (it ends `cancelled`), and
  `session.stop` and the daemon's shutdown kill it and wait (bounded) for it to settle before
  the session closes. The rest of what the ACP client asks (`daemon/handlers/input.rs`,
  `children.rs`, `model.rs`, `queue.rs`): `session.pause_input` (`{session, key}`) holds the open
  session's lane for the calling connection (`TaskQueue::hold`: nothing queued starts and input is
  admitted `parked`, apart from the lane's park, so a resume leaves a held lane held) and answers
  its `pauseId` (the same connection and key answer the same one); `session.release_input`
  (`{session, pauseId}`) releases it, and the connection's end (`Handlers::disconnected`) and the
  session's stop release all of theirs, the old daemon's `acquire_session_input_pause` and
  `release_session_input_pause`; `session.cancel_task` (`{session, task}`) takes a queued task out
  of the lane or aborts the running one's turn (the steers it took and had not read go back),
  answering `cancelled`, the old daemon's `cancel_prompt_admission`; `session.wait_idle` takes
  `"subagents": false` to wait for the session's own lane only and answers the autonomous run's
  accounting (`autonomous`) and the kernel's live background jobs
  (`outstandingBackgroundJobs`), the old daemon's `wait_for_headless_completion`;
  `session.model` (`{session}`) answers the model in its wire shape, the thinking level and the
  levels the model supports, and `session.set_effort` on an open session clamps the level to the
  model, records the `thinking_level_change` row only when the level changes and lets the
  settings' default follow (when the model thinks or the level is not `off`), the old daemon's
  `get_connection_state` and `set_thinking_level`; `session.children` (`{session}`) answers the
  direct subagents in the old daemon's `get_rlm_children` row shape and `session.cancel_child`
  (`{session, child}`) cancels one's run (`cancelled: false` for an unknown or settled child),
  its `cancel_rlm_child` (a cancelled child's row reads `cancelled`, as the old daemon's does);
  `session.system_prompt` (`{session}`) answers an open session's system prompt, the old
  `get_system_prompt`. A session's setting switches are the old worker's commands of the same
  names (`daemon/handlers/switches.rs`, their rules `pa_core::settings::switches`), each a call
  on an open session answering the old response's `data` (`null` for one that had none):
  `session.cycle_model` (`{session, direction?}`: the scoped list's available entries, else the
  available catalog; gated by `allowedModels`, switched with its pinned or clamped level, a
  `model_change` row and the settings' default model; `{model, thinkingLevel, serviceTier,
  isScoped}`, `null` with fewer than two), `session.set_scoped_models` (`{session,
  scopedModels}`, kept for the session's open life and shown as the snapshot's
  `scopedModels`), `session.cycle_effort` (`cycle_thinking_level`: the next supported level, as
  `session.set_effort` sets it; `{level}`, `null` for a model that does not think),
  `session.set_service_tier` (the requested tier as a `service_tier_change` row when the
  preference changes, the settings' default when the model supports it), and
  `session.set_transport`, `session.set_steering_mode`, `session.set_follow_up_mode`,
  `session.set_auto_compaction`, `session.set_auto_retry` (the settings the next turn reads; the
  queue modes and auto-compaction also switch the running session). A move of the active tier
  (the preference clamped to the model) is the old `service_tier_changed` on the session's
  stream; a cycled model or level reaches the roster feed. A cycled model retargets the next
  request as `session.set_model` does, and a set tier becomes the active tier the next request
  carries; a session's build starts it at its tier row's tier (else the settings' default)
  clamped to its model, as the old worker's create. A root session stopped and opened again waits for its new engine's
  build (`Subagents::forget_root`), and `schedule.add` right after a create waits for the
  build that arms its jobs;
  `session.fork` copies a transcript into a new session (`SessionManager::fork_from`);
  `session.export` writes it as JSONL (`export_jsonl`). An open session's turns and queue
  (`daemon/handlers/queue.rs`): `session.abort` aborts the running turn (its task ends
  `Cancelled`), puts the steers it had not read back at the front of the lane and parks the lane
  (input published meanwhile is admitted `parked`) until `session.resume_queue`;
  `session.abort_and_send_queued` keeps the lane going, so those steers run next, together;
  `session.withdraw` cancels the queued tasks and answers them in run order;
  `session.restore` (`{messages}`) publishes them back as steers; `session.wait_idle` answers
  once the session is at rest (`Runtime::at_rest`: nothing queued or running in its lane, no
  delivered row waiting on its agent for the wake it queued, and every subagent below it
  settled, `Subagents::subtree_settled`, so the notice turn a child's settle wakes it for has
  run; the old daemon's `wait_for_idle` with `waitForRlmQuiescence`); `session.compact`
  (`{instructions?}`) queues `/compact` in the lane, refused while it is parked, and answers its
  task's outcome with the `compaction_end` it streamed; the `tasks` query answers the lane
  (running, queued, parked). The state queries (`daemon/handlers/state.rs`) read the session
  manager (an open session's on its runtime, a saved one's opened for the read):
  `session.context` (`active_context`), `session.usage` (`usage_host::session_usage_json`, the
  branch's message counts, the context use against the open model's window), `session.tree`
  (`SessionTree` nodes and the leaf), `session.context_tree` (the root's label, model, spend and
  context use) and `session.breakdown` (`ContextBreakdown`: the open engine's
  `context_categories`, the prompt-cache totals, the compactions, the spend); an open
  session's tree has a child node and its breakdown an agent row for each subagent below it,
  read from that subagent's engine (`Subagents::family_of`). For the CLI's daemon commands:
  `session.send` (`daemon/handlers/messaging.rs`; `{session, message, delivery?, from?,
  client?}`) delivers the old daemon's `send_message`: today's `agent_message` row on the
  session's inbox (`Subagents::send_message`), from the client (`{clientId}`, spending no wake
  budget) or another open session, steered or (`follow_up`) queued behind a running turn, a
  saved session opened for it first, answering the old daemon's receipt; the `schedules` query
  (`{session?, all?}`) and `schedule.add` (`{session, schedule, prompt}`) and `schedule.cancel`
  (`{job}`) list, add and cancel the jobs of the sessions it holds on pa-core's job store
  (`daemon/handlers/jobs.rs`, `Schedules::{jobs, add, cancel}`), the old daemon's `cron_*`. **Families**
  (`subagents/family.rs`): each spawn, settle (done), rename and delete is recorded in the
  supervisor's RLM spawn ledger and the child's `rlm-subagent.json`
  (`pa_core::session_engine::rlm_ledger`), the spawn's edge before the child is given its task
  (a spawn it cannot record is refused); a root session opened (created, revived, woken)
  reopens, off the open, the live children the ledger records under its transcript as settled
  children on their own transcripts (`Subagents::reopen_children`). `agent_message.wait` parks
  on each node's `AgentMessageWaits` (the inbox delivery reports every message it delivers) and
  `wait_for_arrival` refuses a cycle from every open session's wait edge; `rlm-collect` reads
  `waiting_on_you` from it and ends when a child starts waiting on the caller.
  `agent_observe.list`, `get` and `recent` read the nuclear family the runtime runs
  (`session_messages` is not yet there). A subagent's RLM heartbeats are armed on its spawn's
  transcript and skipped once the runtime no longer runs it. The kernel's timer and watch
  sources are kept (`Runtime::keep_kernel_sources`), so they run again after a restart. `settings`, `models`, `sources` and `subscriptions` answer that
  they are not yet in the runtime daemon. At start it
  closes standard input, raises the open-file limit, keeps the model catalog's caches warm as
  the supervisor does and, once the startup refresh settles, sends the `daemon event`
  `catalog_refresh` with `daemon_kind: runtime`, all off the accept path.
  **Reliability** (`session/faults.rs`, `daemon/held.rs`): the daemon's sessions are supervised.
  It installs `pa_core::session::crash::install_panic_hook` and each lifetime's scope carries a
  `PanicSink` and a `StuckCellSink`: a panic nothing catches on one of the session's threads, one
  its work loop catches, or a cell still on the engine after its interrupt's grace ends that
  lifetime only. The work running fails with `SessionCrash::reason` and is not rerun; the crash
  is appended to `<agent dir>/runtime-daemon/session-crashes.jsonl` (`crashes`), stored as
  `agent/<id>/crashed` (a `SessionFaultEvent`), and the programs the session's commands left
  running are reaped from its orphan journal (`<agent dir>/runtime-daemon/<id>.orphans.jsonl`,
  named in each daemon session's environment). The next lifetime waits
  `restart_backoff_ms(failures)` (250 ms doubling to 30 s; a 30 s lifetime resets the count),
  keeps the fault in the transcript (display-only `session_fault` rows, which
  `session.snapshot` reads back as `lastCrash` with its state), answers each call the crash cut
  off once as of unknown outcome (`pa_core::session::revival::interrupted_call_results`), builds
  a fresh engine (which tells the model once that its kernel definitions are gone) and stores
  `agent/<id>/recovered`. Each fault is also mirrored onto the session's stream, unnumbered, as
  the supervisor sends it: `session_crashed`, then for a run the crash cut off (its
  `agent_start` went out, its `agent_end` never will) a `turn_end` with the crash's reason and an
  `agent_end`, then `session_recovered` or `session_failed`. After the sixth crash in a row it stores `agent/<id>/failed`, the work
  sent fails with the give-up's reason, and the daemon closes the session (its lease goes). The
  sessions the daemon holds are kept in the database (`kv` `runtime-daemon`/`sessions`: path,
  cwd, the daemon's pid); a daemon that starts after one that died reaps each one's orphans by
  the dead daemon's pid and revives those with unfinished tasks under their lease: the death is
  their `daemon_restart` crash, and the task that was running is classified by
  `pa_core::session::revival` (`transcript_evidence` from when its run started,
  `pa_log::Log::task_saved_at`; `host_death` by its run count): run again as queued, continued
  as the same task behind `TURN_INTERRUPTED_NOTICE`, stored done when its answer was saved, or,
  on its second death in a row, stored failed with the crash's reason. A kept launch is kept by
  pa-daemon's restart owner (`keeper::KeptDaemon::Runtime`), its records under `daemon2.sock`.
  **Scheduled jobs** (`daemon/schedules.rs`): a daemon session's kernel serves `schedule.*` and
  `rlm_heartbeat.*` on pa-core's job store (the session's `session-artifacts/<id>` partition,
  the old daemon's rows), bound through `SessionSeams::scheduled_jobs`. Each active job has a
  kept timer source `schedule-<job>` at its next run and a subscription `schedule:<job>`
  routed to its session; a fire runs one pass of the session's lane (pa-core's
  `AgentCronScheduler`, never started: its own timer never runs), whose claim, catch-up,
  failure backoff and result records are the store's, then the timers follow the store. A plain
  job is a `follow_up` task over its prompt; a heartbeat steers its `heartbeat_prompt` row onto
  the session's agent and queues an empty input, deferred by pa-core's rule; a fire waits for its
  task's `done`. A job due in a session the daemon holds dormant (idle when its predecessor died,
  `"dormant": true` in the held record) opens it under its lease; a session stopped or deleted
  on purpose is disarmed and not held. A job runs only while `DaemonOptions::selected` says the
  daemon is the selected one; otherwise its fire is recorded as a skip. Subagents keep no jobs.
  Not yet: cross-session features (7e), a running
  session's model switch, and the lane's suspension across a restart (a restored lane starts
  unparked). Under the print modes (`Runtime::open`) a panic still only fails its task and the
  next one runs on a fresh engine.

- **ACP mode** (`acp`, `prime-agent --mode acp`): the Agent Client Protocol JSON-RPC surface on
  stdio (initialize, session/new, session/load, session/list, session/prompt, session/cancel,
  session/close, session/set_config_option, and the outgoing session/update notifications with
  the namespaced `_meta` correlation), served over a session of the supervisor (`pa-daemon`),
  which it reaches as a socket client on the old wire (`pa_types::daemon`): it links no daemon
  code. Every daemon request goes through a command-level transport (`acp/transport.rs`:
  `DaemonTransport`, the old wire's `DaemonLink`), whose one frame stream delivers each response
  after the events sent before it. The TS capture corpus it is checked against is
  `testdata/acp/`. With the runtime daemon selected it drives that daemon on wire2 instead
  (`acp/wire2_link.rs`, `Wire2Link`): `create` is `session.create {path, cwd, options}`
  (`--no-session` an `ephemeral` one), `attach`/`detach` subscribe to and unsubscribe from
  `agent/<id>/#` (an attach answers at its `backfilled`), `prompt_and_wait` publishes on
  `agent/<id>/input` as a follow-up and answers at that task's `agent/<id>/done` (the task kept
  under the prompt's admission id for `cancel_prompt_admission`, `session.cancel_task`, also
  when the cancel came before the publish's reply), `abort_and_clear_queue` is `session.abort` +
  `session.withdraw` + `session.resume_queue`, the waits are `session.wait_idle`, and the rest
  map one to one (`sessions.list`, `session.context`, `session.model`, `models`,
  `session.set_model`, `session.set_effort`, `session.children`, `session.cancel_child`,
  `session.pause_input`/`release_input`, `session.set_mcp_servers`, `session.stop`). Stream
  events become `session_event` frames (their payloads the old worker's, `meta` taken off) and `agent/<id>/schedules` `heartbeats_changed`; a
  subagent's status change reaches its parent's stream as the old worker's `rlm_child_update`
  (its `session.children` row, once per status, with `parentId` under a subagent parent; its
  `label` is the task's one-line label, `pa_core::session_engine::rlm_notices::rlm_child_label`).
  `rlm.collect` reports a child's `tool_use_count` (the tool calls on its branch) and
  `queued_ms` (how long its spawn waited for its engine), as the old daemon's collect does.

## Non-goals
No storage engine (the SQLite log is `pa-log`'s, behind `EventStore`, `StateStore` and
`TaskStore`). No engine assembly (the embedder's `SessionBuilder`). No model calls for `Call`
routes yet (behind `ModelCaller`). No source of its own: the sources are `pa-sources`', and
webhooks wait for the daemon.
No socket binding or stale-socket cleanup (the embedder binds the runtime daemon's endpoint
through the old daemon's socket lifecycle). No model resolution for a subagent (its builder's, from its
`SubagentSpec`). No persistence of other subscriptions yet (the kernel's event subscriptions are kept).
No `agent_message.wait` (it stays the daemon's), and no pause or clear of agent messages. Nothing
bounds how wide a cascade grows within the hop
limit, or how many events an `Intake` or the sessions' outbox buffers.

## Public API
- `Bus`, `BusParts`, `Published`, `Listener`, `Heard`, `Intake`, `BusClosed`,
  `DuplicateSubscription`.
- `WireServer`, `WireConfig`, `Handlers` (`call` names the `ConnectionId` it came on;
  `disconnected` when a connection ends), `ConnectionId`, `Query`, `Call`, `UnknownOperation`,
  `CLIENT_TOPICS`, `IN_FLIGHT`, `MAX_TAIL`.
- `EventStore` (`append`, `since`, `before`), `StateStore`, `StateKey`, `StateWrite`,
  `SessionTree`, `ModelCaller`, `ModelCall`, `TaskSink`, `TaskRequest`, `Announced`.
- `TaskQueue` (`push` answers a `pa_types::event_log::Admission`), `TaskRunner`, `TaskOutcome`,
  `Steered`, `LaneState`, `TaskStore`, `INTERRUPTED_BY_RESTART`.
- `MemoryEventStore`, `MemoryStateStore`, `MemorySessionTree`, `MemoryTaskStore`.
- `LogEventStore`, `LogTaskStore`, `LogStateStore` (each `new(Arc<pa_log::Log>)`;
  `LogStateStore::get`).
- `Runtime` (`open`, `publish`, `listen`, `add_subscription`, `remove_subscription`,
  `define_source`, `running_sources`, `create_session`, `cancel`, `idle`, `settle`, `finish`,
  `shutdown`), `RuntimeOptions`, `SourceDefinition`, `SourceLifetime`, `SessionBuilder` (`build(id, spec, seams)`), `SessionSeams`
  (the session's trust gate, the host requests the runtime answers - its `event.*` and its
  family's `agent_message.*` - its `rlm.*` subagents, the late-receipt sink, the
  background-settle wake and the kernel-notice sink, all of which the engine must install, and
  `scheduled_jobs`, the cron wiring a runtime-daemon session's kernel binds `schedule.*` and
  `rlm_heartbeat.*` to, `None` elsewhere),
  `SessionSpec` (`transcript`: the file a daemon session opens; `ephemeral`: a session kept in
  memory only), `SubagentSpec`,
  `BuiltSession` (`queue_modes`: `QueueModes`), `DriverEvents`, `SettleAfter`, `Settled`, `Finished`;
  `TaskQueue::session_idle`.
- `acp::daemon::{run_daemon_attached_acp_mode, DaemonAcpOptions, AcpDaemon}` (pa-cli
  dispatches `--mode acp` through it; `AcpDaemon` picks the supervisor or the runtime daemon).
- `daemon::{run, DaemonOptions, socket_path}` (`DaemonOptions::selected`: whether the daemon
  is the selected one, asked before each scheduled job). On the wire (strings, not Rust surface): the
  queries `tasks`, `session.context`, `session.usage`, `session.tree`, `session.context_tree`,
  `session.breakdown`, `session.model`, `session.system_prompt`, `session.children`,
  `runtime.roster`, `runtime.traffic`, `sessions.list {shape: "saved"}`, the live-only topics
  `runtime/roster`, `runtime/traffic` and `runtime/session_list/<request>`, and the calls
  `session.abort`,
  `session.abort_and_send_queued`, `session.resume_queue`, `session.withdraw`,
  `session.restore`, `session.compact`, `session.pause_input`, `session.release_input`,
  `session.cancel_task`, `session.cancel_child`, `session.cycle_model`,
  `session.set_scoped_models`, `session.cycle_effort`, `session.set_service_tier`,
  `session.set_transport`, `session.set_steering_mode`, `session.set_follow_up_mode`,
  `session.set_auto_compaction`, `session.set_auto_retry`.

Dependencies: `pa-types`, `pa-log` (the SQLite stores), `pa-sources` (the timer and watch
sources it starts and stops), `pa-core` (the session engine, the
kernel's spare pool, the turn driver's compaction, refinement, goal and autonomous seams) and
`pa-agent` (the agent loop's steering, abort and events), among workspace crates.
