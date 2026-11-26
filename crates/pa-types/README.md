# pa-types

The single shared vocabulary crate. Nothing else is shared between crates.

## Scope

`compaction::is_valid_threshold_percent` is the shared percentage-domain validator
used by settings deserialization, persistence, runtime policy, and the TUI.
Wire and domain types + serde only: AI messages/content blocks/tool calls/usage/stream events, session JSONL entry schema, daemon wire protocol messages, worker frames.

Daemon wire mechanics shared by the serving side (pa-daemon) and clients (pa-tui/pa-cli) live here because pa-tui depends on pa-types alone:
- `daemon::replay_info`: the attach's replay verdict for a resume cursor (TS `createDaemonReplayInfo`), shared by the supervisor's worker and the runtime daemon's attach snapshot.
- `daemon::framing`: the private-frame codec of the worker socket (direct-attach clients speak it too).
- `daemon::message_delta`: the `message_update_delta` attach capability's codec — the
  per-connection encoder the serving side (supervisor client writer, worker direct link) runs
  over every written `session_event`, and the client decoder that restores each delta
  `message_update` to its full message before anything else reads it.
- `daemon::{DaemonOutbound::HistoryImport, HistoryImportPhase}`: the Rust-only `history_import` broadcast (a distribution build's background chat import started/finished) the daemon sends and the TUI reads; older clients ignore it like any unknown broadcast, so the schema id is unchanged.
- `control`: the IO-free adaptive capacity controller (`Controller`, `ControllerConfig`,
  `Observation`, `Target`, `LearnedState`), the pure `RateBucket`, and the `Priority` classes
  with `service_deadline_ms` aging. Pure decision logic with explicit clocks (no tokio, no IO):
  pa-ai's provider admission is its first user; any later autoscaled resource wraps the same
  controller in its own permit pool and persistence.
- `events`: the event bus's wire vocabulary: the `event_bus` command's typed `EventBusRequest`
  (define, remove, publish, subscribe, unsubscribe, list), the delivery mode, and the source and
  subscription records the daemon's bus keeps and lists, with a source's kind
  (`EventSourceKind`: bus, timer, watch) and a timer's or watch's spec (`TimerSpec`,
  `WatchSpec`). pa-core's kernel functions build the requests and pa-daemon's supervisor (or a
  headless run's pa-runtime) answers them.
- `event_log`: the vocabulary of the event-driven core (being built beside the daemon; it
  replaces `events`): `Event`/`NewEvent` with `Trust` (owner or untrusted, only ever lowered
  along a chain of causes) and a cause chain bounded by `MAX_HOPS`; `Topic` and `TopicFilter`
  (MQTT `+`/`#` wildcards) and `TopicTrie`, which matches a topic against every filter in time
  proportional to its depth; `Topic::is_live_only` (`agent/<id>/stream`, `agent/<id>/tool`, and the
  agents view's feeds `runtime/roster` and `runtime/traffic`): events the runtime fans out but
  never stores, with `seq` 0; `Subscription` (scope, filter, `when` predicate by JSON Pointer,
  throttle and debounce, `Route`: set, publish, call, agent) and the `Task` an agent route queues
  in its session's lane, ordered by `Priority`; an agent route's `AgentTarget` is a named session,
  the event's own session (`Origin`), or a new one (`Spawn`). The payloads of a session's stored
  topics: `AgentInput` (`agent/<id>/input`: the text a turn starts from, the images it carries
  and, optionally, its lane priority; an agent route with an empty prompt starts from it),
  `AgentAccepted` (`agent/<id>/accepted`: the task an input became, its priority and its
  `Admission`: `queued`, `steered` into the running turn, or `parked` in a suspended lane) and
  `AgentDone` (`agent/<id>/done`: task id, outcome, error, and for a failed task its
  `TaskFailure`: a failed reply, command or turn).
- `wire2`: the event wire between clients and the runtime: `ClientFrame` (hello, subscribe with
  a replay start, unsubscribe, publish, query, call) and `ServerFrame` (welcome, event,
  backfilled, reply, closing), versioned by `WIRE2_VERSION`.
- `daemon::plane`: the session/control command-plane table (worker-side peer gating and client-side socket routing both read it).
- `daemon::{DaemonPeerTransportTicket, DaemonWorkerPeerGrant, DaemonPeerCommand}`: the direct-transport ticket and grant wire shapes.
- `goal`: the thread-goal wire state (`GoalState`/`GoalStatus`) shared by the
  goal engine (pa-core), the daemon wire (`goal_update` session events and
  the attach snapshot's `state.goal`), and the TUI (announcement rows, tray
  label). The goal *engine* (validation, accounting, continuation prompts)
  is pa-core's.
- `billing`: how one model call is paid for (`BillingMode`, stamped as the optional
  `usage.billing` of each recorded assistant message; a missing or unknown mode reads as
  unclassified) and the per-mode spend split (`BillingSplit`) the daemon's session summaries carry.
- `agent_traffic`: the display-only agent traffic entry (`AgentTrafficEntry`: from, to,
  relationship, kind, outcome, chars, a sanitized preview of at most 240 chars) the receiving
  worker reports, the supervisor rings per root tree, and the TUI's Traffic pane renders;
  never model context, never a transcript.
- `slash_commands`: the builtin slash-command table every surface shares
  (the TUI dispatch + autocomplete, the session engine's command admission,
  CLI suggestion help) plus its pure parse/suggestion helpers — the TS
  product keeps the same single table in core and imports it from its TUI.
- `incident`: the daemon incident classifier shared by the incident CLI
  (pa-cli's `prime-agent incident`, TS `src/cli/incident.ts`) and the
  agents-view incident notice (pa-tui's `incident_notices`, TS
  `src/modes/agents-view/incident-notices.ts`) — pa-tui depends on
  pa-types alone, and the TS product keeps the same single classifier in
  `cli/incident.ts` with both halves importing it. Pure log-line parsing
  (`agent.jsonl` records, per-daemon plain-text lines), worker pid
  attribution, event classification, and the stall/burst/gap anomaly
  computation — the message shapes are the TS regexes kept 1:1. The CLI's
  log-file discovery, `--since`/`--until` window parsing, and report
  rendering belong to pa-cli; the notice polling, rotation-safe
  incremental reads, and dismissal horizons belong to pa-tui.
- `ai::DiffCut`: the row that ends a kernel cell's edit diff (`details.diffs[].diff`) when
  pa-core cut it, counting the changed lines it left out (`… diff cut: N more lines (+A -R)`);
  pa-core writes it and pa-tui's card and `/diff` panel read it, so a cut diff keeps its true
  totals with no new wire field.
- `themes`: the bundled theme definition files (`prime`/`dark`/`light`) as
  pure data, shared by the TUI's theme loader (pa-tui renders terminal
  colors from them) and the session HTML exporter (pa-core resolves them
  into CSS variables). Everything built on top of the files — terminal
  color rendering, export CSS generation, custom-theme discovery — belongs
  to the consuming crates.

- `daemon::update_flow`: the update-flow state machine's shared vocabulary:
  the coordinator FSM states + legal
  transition table, `UpdateId`, the on-disk artifact schemas (`intent.json`,
  `status.json` — TS status-file shape, `prepared/<id>/{roster,marker}.json`),
  the roster-snapshot projection (sessions/workers/subagents/heartbeats,
  durable session ids, heartbeat re-arm fields only), the timeout-budget
  table with `PRIME_AGENT_UPDATE_*_MS` overrides, and the artifact path
  layout under `<agent-dir>/update-restarts/`. Pure serde + pure data
  helpers; the FSM drivers and watchdogs are owned by pa-daemon/pa-cli. The
  prepare/commit wire commands (`prepare_update_restart`/`
  commit_update_restart`) and the TS mutation classification
  (`is_daemon_mutating_command`, `is_update_drain_command` — the
  update-flow admission gate's read tables) live in `daemon::{command,plane}`.
Platform contracts (`platform`): the cross-crate transport, process-identity, socket-identity, and home-dir helpers, plus the suspend-to-background signal control (`process::{stop_own_process_group, ignore_sigint_for_suspend, restore_default_sigint}`: the TUI's TS-`handleCtrlZ` cycle — SIGTSTP to the own process group, SIGINT ignored for the stopped window and restored on the SIGCONT resume; unsafe lives behind this wall because pa-tui opts into the workspace `unsafe_code` forbid) (`platform::clock::local_clock`: an epoch-ms timestamp as the local `(hour, minute)` through `localtime_r`, `None` on Windows — the TUI's prompt clock) (`platform::windows_clipboard::read_clipboard_image_formats`: the Windows clipboard's `PNG`, `CF_DIBV5`/`CF_DIB` and `CF_HDROP` payloads copied out raw through user32 for the TUI's image paste, and `read_clipboard_text`'s `CF_UNICODETEXT` for the paste key's text fallback - empty off Windows; decoding stays in pa-tui) (`platform::dirs::home_dir`: `HOME`, then on Windows `USERPROFILE` / `HOMEDRIVE`+`HOMEPATH` - Node `os.homedir()` parity, returning `None` so each caller owns its fallback) (`platform::StateLayout`: the per-user state a build owns - the official `~/.prime/agent` and `prime-agent` daemon endpoint names, or a distribution build's own `~/.prime/agent-<variant>` and `prime-agent-<variant>` names from the `PRIME_AGENT_DIST_VARIANT` stamp; every agent-dir, socket-dir, pipe, and kernel-venv default resolves through it). (`platform::{replace_file, AtomicFile, ReplaceOptions}` and `platform::rename_onto`: the one atomic file replacement every persistent write in the workspace goes through - a temp file beside the target, written, synced per `Durability`, renamed onto it with the bounded win32 destination-busy retry, its mode `Private` (0600) or kept from the target, a symlink followed or replaced; it lives here because pa-shell and pa-tui reach pa-types alone, and pa-telemetry, which depends on no workspace crate, keeps its install-id rename to itself). pa-types is the only crate every transport consumer can depend on (pa-tui depends on pa-types alone), so the shared trait vocabulary and its cfg-gated platform implementations live here: AF_UNIX on Unix, named pipes (`\.\pipe\`, `platform/windows_pipe.rs`) on Windows. Remaining platform areas (process control, perms, ...) are implemented per platform behind the same traits, not re-plumbed in callers.

- `platform::elevation`: whether this process runs with the machine's highest rights (`Elevation`: an elevated token on Windows, effective uid 0 on Unix, read once by `process_elevation`) and how a program on this platform gets them (`ElevationRoute::CURRENT`: UAC, or `sudo`). Every session of a daemon runs in its process, so this is the daemon's fact, process-wide by design: pa-core states it in the prompt's environment section.

- `provider_usage`: the `/usage` page's wire vocabulary — the `get_usage` sections, the provider quota cards (`QuotaReport`/`ProviderQuota`/`QuotaWindow` with reset times and the card's source), and the local spend history (`SpendHistory`: spans, 30 local days, per-model rows, observed rate limits). Response-only payloads the daemon builds and the TUI reads (no TS counterpart to round-trip, so no catch-all maps). `platform::clock::local_civil` gives the local date, weekday, and clock the page and the daemon's day buckets share.

- `usage`: the chars/4 context estimators over raw message JSON and the `contextUsage` wire object
  (`ContextUsage`: tokens, window, percent, plus the Rust-only `estimated` flag and the anchoring
  request's `lastRequest` cache split) that `get_session_stats`, the attach state, and the live
  `message_end`/`message_update` frames carry.

- `notification`: the kernel `notify(...)` wire vocabulary (`KernelNotification`, `NotificationLevel`, the
  `kernel_notification` session event) shared by pa-core's hub, the daemon worker, and the TUI.
- `context_breakdown`: the TUI context window's `get_context_breakdown` reply — the request-view category estimate (pa-core measures it), prompt-cache totals, compaction history, session cost, and per-subagent rows (the daemon folds them). Response-only and additive: every field defaults, so older and newer replies both parse.

- `todo` and `question`: the kernel todo list and question wire vocabulary shared by
  the host (pa-core's `interaction` hub persists and serves them), the daemon wire (the
  `todo_update`/`question_open`/`question_closed` session events, the attach snapshot's
  `state.todos`/`state.pendingQuestion`, the `answer_question` command, the `user_questions`
  client capability), and the TUI (the todo tray and the answer prompt). Validation and the
  wait live in pa-core.


## Non-goals
No provider logic, no session logic, no UI. Beyond pure data helpers and the platform contracts, no behavior: a domain type that wants a method belongs in the owning crate.

## Public API
Everything in this crate is deliberately `pub` - it is the cross-crate contract. Unknown fields survive round-trips via catch-all maps so schema revisions stay compatible.

## Depends on
serde, serde_json, thiserror, anyhow, tokio, regex (the incident classifier's TS log-message patterns; see the `incident` scope entry). No workspace crates.
