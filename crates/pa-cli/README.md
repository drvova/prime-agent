# pa-cli

The `prime-agent` binary.

## Scope
Argument parsing, mode selection (interactive/headless/json/daemon subcommands), binary wiring of crates into processes, exit codes and user-facing errors. The daemon-forensics CLI (`incident.rs`, TS `src/cli/incident.ts`): the `prime-agent incident` timeline command — log-source discovery (agent.jsonl plus its `.old` rotation, falling back to the newest per-daemon log), `--since`/`--until` window and `--session` filter parsing, and the report rendering; the shared classifier lives in pa-types (`pa_types::incident`) and the agents-view notice in pa-tui. Client-side MCP auth flows (`mcp_login.rs`): the `/mcp login`/`/mcp logout` hook the TUI calls — the TS interactive client's placement — resolving a server (settings `mcpServers` + the builtin catalog), running the pa-core OAuth login on the suspended terminal, and persisting through the shared auth store. The same placement serves the TUI's client-command hooks: provider auth (`provider_login.rs` — the `/login`/`/logout` catalog, the API-key store, the MCP device flow, the Prime Inference terminal login (`prime_inference_login.rs` — the API-key flow: prime-cli credential reuse, the paste prompt with the whoami access check, the numbered team selector); the subscription OAuth and the Prime browser challenge stay unported and report it), trace-sharing state (`client_traces.rs` — the settings flag writes and the credential resolution `/traces` shows; the upload subsystem stays unported), and the `/update` runner (`client_update.rs` — the out-of-band installer funnel with the output captured, the same body `prime-agent update` runs; `installer_update.rs` is that CLI command's own driver, `--check` included), plus the staged update flow (`update_flow/`, `self_update.rs` — the `package update` self target's body). The descriptor OAuth commands (`descriptor_login.rs`): `login <provider> [--device]` and `logout <provider>`, plus the `/login` panel flow for models.json providers whose `auth` block declares `oauth`; the sign-in itself is pa-core's.

## Non-goals
No business logic; it composes pa-daemon, pa-runtime, pa-core, pa-tui, pa-ai via their public APIs only.

## Public API
The `prime-agent` executable surface (flags, subcommands, exit codes). Internal lib types `pub(crate)`, plus the daemon wiring (`interactive_mode::{ensure_daemon_running, ensure_daemon_running_with, resolve_socket_path}`) exported for the integration verifier.

## Depends on
pa-types, pa-ai, pa-agent, pa-core, pa-daemon, pa-runtime, pa-tui (one-way, composition root).

## Runtime daemon
`--mode daemon` starts the runtime daemon, the default, or the supervisor when the global
setting `"daemon": "supervisor"` or `PRIME_AGENT_RUNTIME_DAEMON=0` selects it (kept for one
release; `PRIME_AGENT_RUNTIME_DAEMON=1` selects the runtime daemon over the settings;
`daemon_mode::DaemonKind::selected`). The runtime daemon (`print_runtime/daemon_session.rs`): the agent
dir's lock (`pa_core::platform::RuntimeDaemonLock`), then `daemon2.sock` beside the resolved daemon
socket through the daemons' shared socket lifecycle (`pa_core::platform::daemon_socket`: stale-socket cleanup,
owner-only permissions, the exit cleanup that spares a successor) and `pa_runtime::daemon::run`
until SIGINT or SIGTERM. Its `SessionBuilder` opens the transcript a session's spec names
(creating it under its id), applies the client's `SessionOptions` over the daemon's own run
options (provider, model, thinking, system prompt and appended ones, skills, prompt templates;
the autonomous run on `BuiltSession`, the goal seeded on a fresh branch, `executionMode` `acp` or
`interactive` as the telemetry mode, the API key, the telemetry opt-out), reads a session's
profile's settings overlay (`set_active_profile` in the session's scope; an unknown profile fails
the build), pins a new session's header as the old worker's create does (the profile's credential
for its provider, else `credential`, else its parent's pin) and tags its profile, names a scoped
child's parent and depth in its header and runs it at that depth, builds the engine through the headless assembly (a session that names no model and finds none
signed in starts on the catalog's first, as the old worker's does, so it opens and its prompt
reports the missing sign-in; a print or RPC run still fails; the faux provider
under `PRIME_AGENT_FAUX_SCRIPT`, each session answering from a queue of its own,
`faux-<session id>`, presenting the script's `provider` and `modelId`, as each supervisor worker
does), streams the turn driver's events, and
builds subagents as the print run does (`print_session::build_subagent`; a scripted session's
subagent runs `PRIME_AGENT_FAUX_CHILD_SCRIPT` when the script names none for it, the
supervisor's `childScript`); a session's engine binds its kernel's
`schedule.*` and `rlm_heartbeat.*` to the seams' `scheduled_jobs` (a print run has none, so its
`schedule.*` still refuses). The daemon's `selected` re-reads the global settings before each
scheduled job (`daemon_mode::DaemonKind::from_settings`; the environment switch is read
once). A kept launch still splits into the keeper
first; the runtime daemon has no keeper of its own yet (phase 7c). A launcher that waits on the supervisor's socket sets `PRIME_AGENT_DAEMON_SUPERVISOR=1` on the
daemon it spawns (`daemon_mode::SUPERVISOR_ENV`): the update coordinator's successor or rollback
while the supervisor is the daemon it drives, and a launch that selected the supervisor. That
process and the daemon its keeper keeps are the supervisor whatever its settings say. The update
coordinator prepares and stops the daemon it finds holding the sessions
(`update_flow::endpoint::Endpoint::running`: the selected one, else the other one if only it
answers) and starts and restores on the selected one, so a user whose supervisor an earlier
release started is moved onto the runtime daemon by the update; it adopts the staged status by
the socket the invoking update keyed it by.
The interactive launch runs the TUI on the runtime daemon by default (phase 8f):
`interactive_mode` ensures it (`ensure_runtime_daemon_running`), declares `daemon2.sock` a wire2
socket (`pa_tui::daemon_client::use_transport`), hands the TUI and its agents view that socket,
and installs the TUI's `DaemonLauncher` (`interactive_mode::daemon::RuntimeDaemonLauncher`, the
same ensure) for a window whose daemon nothing brought back; with the supervisor selected it
ensures the supervisor. The runtime ensure also stops an idle supervisor it finds on the sibling
socket (no session running a turn, none with a window attached), on its own task, so the
runtime daemon can lease those sessions; a busy one is left for a later launch. A session the TUI creates carries `executionMode: "interactive"`, stamped on
its telemetry as the supervisor's are. `tests/runtime_default_launch_tmux_e2e.rs` is the launch's
verifier (the default, and the idle supervisor's retirement), and the tmux verifiers that run the
real binary under `env -i` run on the default and again on the supervisor
(`support::on_the_supervisor!`). `prime-agent status` lists the runtime daemon and `doctor --fix`
reaps it as it does the supervisor (`daemon_discovery`, each probed and stopped on its own wire).
`tests/runtime_daemon_e2e/` drives it over wire2 against the real binary; `turns.rs` there ports
the supervisor's steer batching, queue order and abort tests (`steer_boundary_batch_e2e`,
`queue_delivery_e2e`, `abort_kernel_e2e`) onto it, with withdraw, restore, compaction and the
state queries.

## Print runtime
The headless print/json modes (`print_runtime/print_session.rs`) run on an
in-process `pa_runtime::Runtime` over the agent dir's database (a scratch
dir's for `--no-session`, removed after the run). pa-cli supplies the
`SessionBuilder` (the headless engine assembly over the session the CLI
flags selected, which keeps its `SessionLease`, plus the `--goal` seed, and
each spawned subagent's engine: its model lane - a scripted child's queue
under `children.<spawn name>`, or the catalog model the spawn names or the
parent runs, resolved through pa-daemon's `rlm_child_model` - over its own
transcript under the parent's artifacts), publishes each prompt as the session's
`agent/<id>/input` (the `@file` images with the first), prints the
session's `agent/<id>/stream` payloads in json mode until each task's
`done`, stops at a failed command (its raw error, exit 1) or turn (`Error:`),
arms the settle behind the last prompt, prints the session header and the
exit contract (text mode's terminal result on stdout/stderr, the goal cap,
the compaction outcomes, the autonomous gates), and finishes the run. What
runs around each turn (the turn boundary, the goal and autonomous
continuations, the session slash commands, the quiescence loop) and the
subagents themselves (each a runtime session; the family's messages, the
settle that waits for every descendant - the #2270 fix - and the exit
disposal) are pa-runtime's.
The json stream emits the TS session-event surface byte-for-shape: the
session header row (version 3), the `message_update` streaming deltas with
the slim `assistantMessageEvent` (the daemon wire drops the nested
`partial`), `tool_execution_update`, the harness digest's message pair at
the first-turn boundary, the compaction `compaction_start`/`compaction_end`
pairs with their durable outcome rows, the `goal_update`/`session_action_update`
frames of the goal loop, and the refinement rows with
`refine_complete`/`refine_failed`. Verifier: the `print_runtime_e2e` rows.

## Daemon client
The daemon-backed public commands (`list`, `stop`, `rename`, `send`, `schedule`) talk to the
pa-daemon supervisor over its JSONL Unix socket through the crate-private client module
(`daemon_client.rs`), or, with the runtime daemon selected, to the runtime daemon over wire2 on
`daemon2.sock` beside the same socket (`runtime_daemon_client.rs`; `daemon_command.rs` maps
each command to its query or call and resolves the session selector against `sessions.list`),
rendering the same output. The client never spawns a daemon - the TS CLI only auto-starts one for
the internal `daemon start`/`open` commands, which are not reachable from the public surface.

## Packaged layout

The release artifact is exe-adjacent (TS install.sh native layout): the binary
plus `package.json` (the version manifest `--version` reads at runtime, with
the compiled-in version as the fallback), `skills/`, and `docs/`. The kernel is
built into the binary, so the layout carries no runtime of its own.
`scripts/package_release.py` (`make package`) assembles, validates,
version-pins, hashes, and tars it. The hidden `--prime-agent-bootstrap` flag,
which installers released before the kernel was built in still run after
extracting a release, succeeds without doing anything. Verifier:
`tests/packaged_layout_e2e.rs`.

## Daemon discovery
The discovery commands (`status`, `doctor [--fix]`, `shutdown [--force]`, TS
`cli/daemon-ps.ts`) live in the crate-private `daemon_discovery` module: an OS census of
listening unix sockets owned by product processes, a socket-dir sweep, probing/classifying
each discovered daemon, and the reap/shutdown planners and executors. The runtime daemon on `daemon2.sock` beside the root's
socket speaks wire2, so the old-wire census leaves it out and `shutdown` stops it first through
its own `daemon.shutdown {force}` (a deliberate stop its keeper does not undo), confirmed once its
socket stops answering and the process its agent-dir lock records has exited; with `--force` a
daemon that does not stop gets the verified kill. Containment is part of
the contract: every scan, probe, and stop is scoped to an explicit `DaemonStateRoot`
(the env-resolved current root for the CLI). All e2e daemons live in
test-created fixture directories only.

## Update flow (staged activation)

`prime-agent update` (the `update_flow` module): the invoking CLI plans, downloads, and
stages the candidate (`Acquire`..`Staged`), then spawns the detached
coordinator - the new binary running from its release dir - which adopts the
status file and owns the FSM to a terminal state (`Preparing..Complete`,
`Rollback` first-class). `intent.json` is the coordinator lock (Join on a
live holder, dead-holder steal), the status file keeps the TS schema plus
`updateId`/`state`/`epoch`, and the activation owns the bin-symlink swap
(`bin/previous` rollback pointer, `.activation-state` recovery record).
Mechanism support (version/channel policy, install-root layout, manifest
fetch, archive staging) lives in pa-core's `update` module, and so do the
daemon side's prepare transaction and boot restore pass (`update::restart`),
which both daemons serve: the coordinator's `Restoring` phase polls the
successor's `update_restore_status` RPC for the real per-session counts.
The daemon the update drives is the selected one (`update_flow/endpoint.rs`):
the supervisor on its socket, or, with the runtime daemon selected, the
runtime daemon on `daemon2.sock`, greeted with `daemon.identity` and sent the
update commands through pa-tui's `wire2_compat` (a connection per request),
its successor spawned with `PRIME_AGENT_RUNTIME_DAEMON=1`; the update's lock
and status are keyed by that daemon's socket. The legacy TS handoff below
restores the TS sessions on the selected daemon the same way.
The legacy restart adapter accepts the fresh status path supplied by shipped
TypeScript updaters after they activate a release. It reports the original
`requestId`/`phase` status schema, translates their session checkpoint to a
Rust roster, and restarts the daemon without repeating activation. Recovery
rosters remain on disk when restoration fails. Socket-scoped checkpoints resume
after an interrupted handoff when no daemon is running, and reject a different
installed version. The adapter holds the shipped TypeScript shutdown-admission
lease in both historic supervisor registries until Rust owns the replacement
socket, preventing old clients from launching a competing daemon. Admission
records are refreshed and removed only while their ownership token matches.
