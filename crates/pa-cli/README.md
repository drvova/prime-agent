# pa-cli

The `prime-agent` binary.

## Scope
Argument parsing, mode selection (interactive/headless/json/daemon subcommands), binary wiring of crates into processes, exit codes and user-facing errors. The daemon-forensics CLI (`incident.rs`, TS `src/cli/incident.ts`): the `prime-agent incident` timeline command — log-source discovery (agent.jsonl plus its `.old` rotation, falling back to the newest per-daemon log), `--since`/`--until` window and `--session` filter parsing, and the report rendering; the shared classifier lives in pa-types (`pa_types::incident`) and the agents-view notice in pa-tui. Client-side MCP auth flows (`mcp_login.rs`): the `/mcp login`/`/mcp logout` hook the TUI calls — the TS interactive client's placement — resolving a server (settings `mcpServers` + the builtin catalog), running the pa-core OAuth login on the suspended terminal, and persisting through the shared auth store. The same placement serves the TUI's client-command hooks: provider auth (`provider_login.rs` — the `/login`/`/logout` catalog, the API-key store, the MCP device flow, the Prime Inference terminal login (`prime_inference_login.rs` — the API-key flow: prime-cli credential reuse, the paste prompt with the whoami access check, the numbered team selector); the subscription OAuth and the Prime browser challenge stay unported and report it), trace-sharing state (`client_traces.rs` — the settings flag writes and the credential resolution `/traces` shows; the upload subsystem stays unported), and the `/update` runner (`client_update.rs` — the out-of-band installer funnel with the output captured, the same body `prime-agent update` runs; `installer_update.rs` is that CLI command's own driver, `--check` included), plus the staged update flow (`update_flow/`, `self_update.rs` — the `package update` self target's body). The descriptor OAuth commands (`descriptor_login.rs`): `login <provider> [--device]` and `logout <provider>`, plus the `/login` panel flow for models.json providers whose `auth` block declares `oauth`; the sign-in itself is pa-core's.

## Non-goals
No business logic; it composes pa-daemon, pa-core, pa-tui, pa-ai via their public APIs only.

## Public API
The `prime-agent` executable surface (flags, subcommands, exit codes). Internal lib types `pub(crate)`, plus the daemon wiring (`interactive_mode::{ensure_daemon_running, ensure_daemon_running_with, resolve_socket_path}`) exported for the integration verifier.

## Depends on
pa-types, pa-ai, pa-agent, pa-core, pa-daemon, pa-runtime, pa-tui (one-way, composition root).

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
(`daemon_client.rs`). The client never spawns a daemon - the TS CLI only auto-starts one for
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
each discovered daemon, and the reap/shutdown planners and executors. Containment is part of
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
fetch, archive staging) lives in pa-core's `update` module; the daemon-side
prepare/commit/stop drivers and the boot restore pass are pa-daemon
(slices 2-3 and 5): the coordinator's `Restoring` phase polls the
successor's `update_restore_status` RPC for the real per-session counts.
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
