# pa-core

The session engine.

## Scope
MCP host side (`mcp/`): auth gating for built-in integrations (the
disabled-skill overrides and `/mcp list` status), the `mcpServers` settings
seam, and the `mcp.*` host-request handlers the kernel's generic MCP
client reaches through (`mcp.config`, `mcp.refresh`, and `mcp.begin_login`, the
kernel's `mcp.login`: the wired interactive login, else an `unavailable` answer
naming `/mcp login`). The OAuth flow lives behind it (`oauth*` submodules:
RFC 9728 discovery, dynamic registration, PKCE, the local callback
server, token refresh; `McpOAuthConfig::callback_port` selects an exact registered
desktop callback port, while dynamic clients retain the candidate range.
Slack's endpoint selects its documented public desktop client; `login.rs`: login execution that persists the
endpoint-bound credential into the shared auth store and the
`wire_begin_login`/`McpOAuth` refresh integration hosts wire). The MCP
protocol itself runs kernel-side (Python
stdio/HTTP); the engine never spawns MCP servers. Tools (python_repl + internal rename/stdout), rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings), session manager (persist/resume). Autonomous mode
(`autonomous`): runtime state with limit normalization, continuation and
gate-failure texts, shell quality gates with retry windows and git
worktree snapshotting, and the `AutonomousDriver` policy trait a turn
loop consults after every settled turn (the engine holds no autonomous
logic of its own). The RLM recursion host seam
(`session_engine::rlm_host`): the trait the kernel's `rlm.spawn`/
`rlm.create_session`/`rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent`/`rlm.abort` host requests call into, with the roster/collect/
selector-error vocabulary the daemon implements over the supervisor link.
Platform wall (`platform`): the daemon endpoints both daemons and their clients name (`daemon_endpoint`: the socket dir, `daemon.sock`, the worker endpoints, `hash_key`, `daemon_log_path` and its `RotatingLog`, and `daemon_state_dir`, the per-socket state dir the supervisor's descriptors and the restart owner's records share) and the socket lifecycle (`daemon_socket`: `can_connect`, stale-socket cleanup under the cross-process cleanup lock, owner-only permissions, the exit cleanup that spares a successor), moved from pa-daemon; process control (signals/process groups, kernel exit waits), file locking (including `RuntimeDaemonLock`, the runtime daemon's one-per-agent-dir lock on `<agent dir>/runtime-daemon.lock` with its holder recorded inside (pid, socket, restart owner), which the supervisor's boot reap reads to spare both, and `reap_holder`, which stops a wedged holder shown to be a runtime daemon on that socket holding the lock file), stopping one process by identity (`process_stop`: the boot reaps' SIGTERM-grace-SIGKILL ladder over a pidfd, and the product-binary gate), file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations (the durable-write rename primitive is pa-telemetry's `rename_onto`, re-exported as `platform::rename_onto`). Scheduled jobs (`cron`, the `AgentCronJobStore` port of `core/cron-jobs.ts`): schedules are read and evaluated by `pa_sources::schedule` (`in`/`every`/`at`/cron, UTC, intervals of at least 10 s), and a due job's missed runs collapse into one claim, its next run counted from the claim; job state per `scheduled-jobs.json` path under session artifacts (kept in the agent database, see `agent_state`) with cross-process locking, plus the public read-only scan (`cron::store::read_scheduled_jobs_artifact`, and `scheduled_jobs_kept` for whether a path has state) the update flow's roster projection and boot re-arm read. Both daemons run a session's due jobs through one `cron::scheduler::AgentCronScheduler` pass per session and wait up to `FIRE_SETTLE_TIMEOUT_MS` for a fire's turn to settle; the supervisor's worker starts the scheduler's own timer, the runtime daemon never does (its timer sources run the passes). Tools (python_repl + internal rename/stdout), rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). origin/main

Branched session files (`SessionManager::create_branched_file`, the TS `createBranchedSession`): a fresh file in the source's session dir holding the source's root-to-leaf path (or the header alone, the fork at the root), its labels recorded again after the path, the header naming the source as `parentSession` with its cwd and RLM depth; the RPC mode's `fork` writes it and opens it under its own lease.

Session leases (`session::lease`, the `core/session-lease.ts` port, moved here from pa-daemon so every daemon and the print mode share one ownership record): one process owns one session file through `<agent dir>/session-leases/<sha256(canonical path)>.lock` (`owner.json`, acquired by an atomic rename, a guard directory serializing updates, dead owners reclaimed); `acquire_runtime_session_lease`, `live_lease_owner`, `reclaim_dead_owner_leases`, `canonical_session_path`, and `SessionAlreadyActiveError` (the `session_already_active` refusal). Release writes the window snapshot, and the info sidecar when the holder gave one (`SessionLease::with_info_sidecar`: the daemon's session catalog owns that scan). Hosted lifetimes (`session::hosted`): the pid-shaped id and generation a session hosted in a daemon process records as its owner (`is_hosted_pid`, `hosted_alive`, `session_pid`), registered by the host that runs it. Session crashes (`session::crash`, moved here from pa-daemon so both daemons share them): `SessionCrash` (`now`, `of_panic`, `reason`), the crash log's name `SESSION_CRASH_LOG`, the restart policy (`MAX_CONSECUTIVE_FAILURES`, `STABLE_LIFETIME_MS`, `restart_backoff_ms`), and the process panic hook (`install_panic_hook`, `daemon_crashes`) that hands a panic on a session's thread to the `PanicSink` its host put on the session's scope (with whether it was `Caught`) and records every other uncaught one as the daemon's. Revival (`session::revival`, moved from pa-daemon): `transcript_evidence` (how far an interrupted turn got, `RevivedTurn`, by its `TurnPrompt`), `host_death` (a daemon death's crash message and `InterruptedTurn` verdict), `TURN_INTERRUPTED_NOTICE`/`TURN_INTERRUPTED_CUSTOM_TYPE`, `UPDATE_RESTART_CONTINUATION_PROMPT` (the prompt both daemons queue for a turn a deliberate stop cut off, moved from pa-daemon's `update_restore`), and `interrupted_call_results` (the outcome-unknown results for the calls a `LifetimeEnd` cut off, which the caller saves).

Boot imports (`session::boot_import`, moved from pa-daemon's `history_import.rs` so both daemons run one copy): `BootImport::run`, the chain a daemon starts off its critical path once it serves — a distribution build's one-time history import (`settings::plan_official_history_import`), then `session::store::import_session_trees`, then `agent_state::import_agent_state` with the daemon's own state files — each copy on the blocking pool, each settled one logged and reported (`agent sessions imported`, `agent transcripts imported`) through the daemon's `BootImportReporter` (`announce`, `log`, `telemetry`: a `TelemetrySource`); `BootImport::running`, the running history import's started `history_import` frame for a client that connects mid-import. Debug builds only: `HISTORY_IMPORT_VARIANT_ENV` plans the import as that distribution variant's build, `HISTORY_IMPORT_HOLD_ENV` holds the copy before its first file while the file it names exists (the daemons' end-to-end tests).

Saved-session catalog (`session::catalog`, moved from pa-daemon's session store and `session_scan` so both daemons list the same rows): `read_session_info` (one transcript's `SessionInfo`, its fold kept in a process-wide cache that resumes a grown file and a sidecar a lease holder's release persists, `write_info_sidecar`), `list_sessions` and `list_sessions_with` (a page of a store's files, newest first, each row handed to a callback as it folds, and the `SessionPage` cursor that resumes the walk), `saved_session_row` (a row's wire form, the old `session_list_item`'s `session`), `find_most_recent_session_for_cwd`, `legacy_ledger_usage`, and the header and text reads they share (`parse_session_header_line`, `SESSION_LIST_HEADER_READ_MAX_BYTES`, `content_to_text`, `normalize_state_status`). A session's own usage (`session::usage_scan`, moved from pa-daemon's `session_usage`): `UsageScan` (assistant usage by entry id, child spend subtracted, billed rows added, and the summary's billing split; `fold_entry` folds a stored entry), `read_own_usage_summary`, `read_session_usage`. Epoch milliseconds as RFC 3339 and back (`session::manager::{iso_from_unix_ms, iso_to_unix_ms}`, moved from pa-daemon's `util`).

What a session's client reads beside its turns, shared by both daemons (8d-1b): `ResourceReload::connection_commands` (`get_commands`' catalog: prompt templates, then skills), `McpManager::connections_view` (the `/mcp` page from one fresh local read) and `McpManager::reload_live_inputs` (`reload`'s MCP half), `SessionManager::anthropic_warning_shown` / `mark_anthropic_warning_shown` (the once-per-session marker row), `models::CatalogRefresh` (the `/model` picker's catalog from the current snapshot and its coalesced background refresh, moved from pa-daemon's `model_catalog.rs`; the daemon's `changed` hook announces a changed snapshot) and `usage_report` (the `/usage` page's halves, moved from pa-daemon's `usage_report/`: `quota_page`, `spend_page`, and `register_quota_host_handler` for the kernel's `usage.quotas`). `session_engine::provider_adapter::ProviderTargetSlot` names the slot a live session's stream reads its provider target from (the RPC mode's and the runtime daemon's switches write it).

Tree moves and the reads beside them, shared by both daemons (8d-2): `session::navigation` (`NavigateRequest`, `navigation_point`: where a `navigate_tree` lands and the text it re-enters; `fork_messages`, `user_row_text`: the `/fork` selector's list; `last_assistant_text`: the `/copy` text), `branch_summarization::{run_branch_summary, BranchSummaryRequest, BranchSummaryRun, BranchSummaryOutcome}` (the abandoned branch's summary on the session's model, moved from pa-daemon's engine), `SessionManager::branch_with_summary`'s serving model (pa-types `BranchSummaryEntry::{provider, model_id}`), and `export_html::RegistryToolRenderer` (the live session's export renderer, moved from pa-daemon's `session_export.rs`).

Whole-session replacements, shared by both daemons (8d-2b): `session::replacement` (`ForkPoint`: where a `fork {entryId, position}` starts and its `selectedText`; `import_target`: where `import_jsonl` copies its file; `replacement_cwd`: the replaced session's working directory; `ReplacementRefused`: a refusal with the old `errorInfo`).

Compaction and retry aborts (8d-2c): a `compact` whose caller passes no abort signal runs under the session's own, which `AgentSession::abort_compaction` aborts (the run is raced, so the summary request in flight is dropped and nothing commits; `/compact` reports it as `compaction_aborted`); a host that announces the run arms the abort first with `AgentSession::arm_compaction_abort` (a `CompactionAbortArm` held until the run returns), so an abort sent on seeing `compaction_start` stops a run that has not begun yet; `prompt_with_auto_retry` waits its retries under the session's retry abort, which `AgentSession::abort_retry` ends (the turn settles with `Retry cancelled`), reset as each prompt starts.

Workspace checkpoints, shared by both daemons (8d-2d): `session::checkpoints` (`CheckpointStore`: the shadow git store per workspace, one index and record log per transcript; `record_turn` records a turn's opening and settled trees, `rewind` restores a navigation's target tree leaving paths changed outside the session, `RewindTarget` the record it reads; `capture`, `restore`, `diff` and `repl_log` for the kernel's `checkpoint.*`), moved from pa-daemon's `workspace_checkpoints.rs`.

Side runs, shared by both daemons (8d-3): `session_engine::side_runs` (moved from pa-daemon's `side_question.rs` and `engine/wire.rs`: `SideRuns`, the registry with the `start_side_question` admission rules and their refusal texts, the owner-only abort, the detach and close aborts and the close's settle (`SIDE_RUN_SETTLE_TIMEOUT`); `drive`, a run's event order, the terminal event sent as it leaves the registry; `run_side_run`, a `/btw` question or a `/better` rewrite on a built `SessionEngine`; `SideQuestionRequest`, `SideQuestionOutcome` and `side_question_event_value`, the `side_question_event` payload; `better_outcome`, `record_better_outcome`'s parse).

Kernel activity, shared by both daemons (8d-4a): `session_engine::kernel_activity` (moved from pa-daemon's `user_bash.rs` and `factory_activity.rs`: `KernelBashRequest`, the `list_kernel_bash`/`tail_kernel_bash`/`kill_kernel_bash` checks with their refusal texts and the reply shape, over `KernelBashAction`; `FactoryLaneRequest`, `factory_activity`'s parse, the unscoped graph (`lists_runs`) and its `{runs: []}` while no kernel runs (`settle`); `factory_lane_enabled`, the opt-in gate; `stored_factory_spec_count` and `count_factory_specs`, a session's stored specs for the graph and the saved catalog's rows) and `kernel::persistent_shells` (moved from pa-daemon's `persistent_shells.rs`: the persistent shells' records as the daemons read them, `live_shell_rows_by_session` for the catalog rows, and `list`, `tail` and `kill`, the machine-wide commands' replies and refusals).

Agents-view session controls, shared by both daemons (8d-4b): `session_engine::agent_controls` (moved from pa-daemon's `agent_message_ingest.rs`, `rlm_surface.rs` and the agent engine's `set_rlm_max_depth`: `AgentMessagePause`, a receiver's pause and its delivery refusal `AGENT_MESSAGING_PAUSED`; `agent_message_safety_status`, `unscoped_agent_message_status` and `cleared_agent_messages`, the ingest replies; `rlm_max_depth_status`; `SetRlmMaxDepth`, `set_rlm_max_depth`'s parse (`RLM_MAX_DEPTH_REFUSAL`), its `rlm_max_depth_state` row (`RLM_MAX_DEPTH_STATE_CUSTOM_TYPE`), the global settings write and the reply reporting it; `SubagentDeleteOutcome`, `delete_rlm_subagent`'s outcomes and reply).

Heartbeat commands and the passive roster walk, shared by both daemons (8d-4c): `cron::heartbeat_commands` (moved from pa-daemon's `scheduled_jobs/arms.rs` and `scheduling_catalog.rs`: `HeartbeatSet`, `heartbeat_set`'s parse and replacement input, which keeps the previous delivery mode; `HeartbeatUpdate`, pause, resume or clear; `heartbeat_manage_action`; `withdraws_queued_fire`, the queued fire's withdrawal on every change but a resume; `heartbeat_reply`, `no_active_heartbeat` and `HEARTBEATS_NEED_A_SESSION_FILE`) and `session_engine::rlm_roster` (moved from pa-daemon's `rlm_roster.rs` with its tests: `walk_passive_rlm_children` over `RosterWalkRoot`s, `PassiveRlmChild`, `passive_child_summary` for `list --all`, `passive_child_info` for the saved catalog; and the live roster's row rules both daemons apply: `seeded_child_summary` and `hydrate_seeded_summary` for a resident root's ledger family, `passivated_summary` for a stopped session's row, `attach_deleted_descendant_usage`).

Setting switches (`settings::switches`, the old worker's rules, shared with the runtime daemon): `queue_mode` and `transport_setting` (what `set_steering_mode`/`set_follow_up_mode` and `set_transport` persist), `scoped_models` (the `set_scoped_models` list and its refusals), `cycle_candidates` / `next_candidate` (`cycle_model`'s candidates, scoped or the catalog, and its step, `CycleDirection`), and `next_thinking_level` (`cycle_thinking_level`'s step).

Display transcript (`session::display_transcript`, moved from pa-daemon's session store so both daemons serve the same rows): a row's wire message (`custom_message_row`, `compaction_summary_message`), where a transcript page may be cut (`continues_run`, `settle_cut` over `DisplayRow`s: never at a tool result or custom row, never between a call and its result), the attach snapshot's `elide_image_payloads` and `prompt_timestamp_ms`. pa-daemon's `SessionFile` pages and the runtime daemon's old-shape attach and `get_messages` replies use them.

Transcript store (`session::store`): every session-transcript read and write in pa-core, pa-daemon and pa-cli goes through the process-wide `store()`, a `TranscriptStore` (create, append, open/read, stat, list, sync, repair, import, restore, remove) addressed by the `<sessions dir>/<id>.jsonl` locator. Two stores implement it. The default SQLite store (`pa-log`, the database `<agent dir>/agent.db`) keeps every transcript byte for byte, keyed by its canonical locator; readers are byte streams whose seeks go straight to the line holding the position (each line records its byte offset), so the windowed reverse reads fetch a long session's tail only; generations are minted from the database's identity, the transcript's row id, length and change count. A JSONL file it does not hold yet is read as it is and imported whole on its first write, then left in place, read-only and never written again, as the rollback; `import_session_trees` imports the agent dir's session trees in the background (both daemons run it after boot, `session::boot_import`) and `export_jsonl` writes transcripts back as JSONL files (`prime-agent export-jsonl`). `prime-agent --mode daemon` starts the runtime daemon (`pa_runtime::daemon`) unless the global setting `"daemon": "supervisor"` (or `PRIME_AGENT_RUNTIME_DAEMON=0`) selects the supervisor, kept for one release; like `transcriptStore`, only global settings select it. The global setting `"transcriptStore": "file"` selects the file store instead (one JSONL file per transcript), the escape hatch for one release; `store_kind` says which is in use. A database that cannot be opened leaves the process on the file store, never silently: `fallback_reason` names the database and the error, and the daemon's hello carries it (`transcriptStoreFallback`) to clients, which show it as a warning. Test isolation seam: `PRIME_AGENT_TRANSCRIPT_DB_DIR` (`DATABASE_DIR_ENV`, set for every `cargo test`/`cargo run` by `.cargo/config.toml`) moves each agent dir's database to `<dir>/<sha256 of the agent dir>.db` (`database_path`), so a test run never writes a real agent dir's `agent.db`; installed builds never set it. The window and session-info sidecars stay files beside the locator with either store, certified against its generations. `tests/transcript_store_guard.rs` fails on raw transcript I/O outside the module.

Agent state (`agent_state`): the stores that were files besides transcripts are kept in the transcript store's database (`pa-log` partitions, schema migration 3), each still addressed by its file's path: schedules and heartbeats (`scheduled-jobs.json`, timer sources with each job's claimed run), the harness stores (`harness_state.json`) and the reuse ledger (`kv` documents), the daemon's event bus (`event-bus.json`, bus sources and subscriptions) and crash log (`session-crashes.jsonl`, crash records). A store reads its file until the database holds its path; its first write stores the whole state, and the file is never written again. `import_agent_state` imports the files (and the daemon's bus and crash log, which the daemon names) in the background, idempotently, one transaction per file, skipping and reporting a file that does not parse and making each original it imported read-only (the transcript store's `freeze_original`); `export_jsonl` with `All` writes them back in their files' layout. With the file transcript store there is no database and every store keeps its file. Public: `StateFile`, `import_agent_state` + `StateImportReport`, `EventBusState` with `held_event_bus`/`store_event_bus` (the daemon's bus keeps its file handling) and `held_event_bus_in`/`store_event_bus_in` (a given database: a headless runtime's bus), and the bus's rules both buses apply: `EventBusState::apply` with `EventRequester`, `TopicSubscriptions` (whether a subscription may name a topic filter: a headless runtime's yes, the daemon's no), `SourceKinds` (whether a source may be a timer or a folder watch, which the bus never stores: a headless runtime's yes, the daemon's no) and `EventBusChange`, and `event_delivery_message`, the message a delivered event carries, `append_crash_record`/`crash_records`, and `StateDatabase` (another process's database, for tests that read a daemon's state).

Per-run harness metrics (`session_engine::run_metrics`): `MetricsMeter` folds
one run's model requests, tool results, compactions and subagent spawns into
`RunMetrics` (steps, eval cells and their time, tool errors, the uncached /
cache-read / cache-write / output token split, the token-weighted cache hit
rate, cache-prefix breaks, compaction context fractions, the turn-end reason
with `empty` and `loop_guard`, wall time). The session telemetry feeds it live
and writes the result onto `agent run completed`; `transcript_run_metrics`
feeds the same meter from a saved session file for offline runners
(`pa-perf session-metrics`/`trials`). Time a cell spends blocked in a wait is
not measured separately: it is inside the cell's own duration.

Provider resilience policies in `session_engine`: the shared quick-retry
policy (TS `provider-retry.ts`: permanent-kind classification,
Retry-After-aware capped delays) and the interactive auto-retry loop.
Retries always stay on the session's provider: a spent retry budget fails
the turn there, and nothing re-routes a turn to another provider serving
the same model id (a provider changes only when the user picks one). A
server-requested wait above `retry.provider.maxRetryDelayMs` (an exhausted
quota's reset) fails the turn at once with the provider's raw error: no
session parks until a reset or resumes on its own, and an active goal
stops on that error like on any other. The driver is pure decision logic:
the host (pa-daemon) owns the attempt and the wait.

Session HTML export (`export_html`): the standalone viewer file for
`/export` and `session export` - the embedded product template
(assets/export-html, vendored marked/highlight.js attributed in its
NOTICE.md) plus the theme-to-CSS resolution over the bundled theme data
(pa-types) and custom themes under `<agent-dir>/themes/`, the tools
section mapping (`tools_section`: the session's tool registry to the
template's name/description/parameters list), and the custom-tool
pre-render pipeline (`ToolHtmlRenderer`/`pre_render_custom_tools` +
`ansi_to_html`: the exporter walks the entries, the session layer's
renderer produces line-oriented output, ANSI converts to inline-styled
HTML at the export step), and the two entry points
(`export_session_to_html` for the daemon worker's `export_html`
command, `export_from_file` for the CLI). Terminal theme rendering stays
pa-tui's; tool renderers live with the session's tool surface;
the share-viewer flow (gist creation) is the caller's.

Native web search (`web_search`): the provider catalog (keyed SERP/agent
APIs, a configured SearXNG, keyless HTML engines and Hacker News), the
`auto` fallback chain, structured query parsing with native filter mapping
and a lenient post-filter, the normalized result shape and its rendering,
and the `web.search` host request the kernel's `rlm.web` client and the
bundled websearch skill reach. One process-wide engine serves every
session; credentials resolve per search on the host and never enter the
kernel; the HTTP client is built on the first search, never at startup. The scraped keyless engines run under per-engine
limits (`web_search::engine_limits`: a `pa_types::control` controller per engine through pa-ai's
permit pool; an engine with no permit now is skipped, never waited on; a block, CAPTCHA, or 429
cools it down for 60 s doubling to 15 min; the public aggregate's soft deadline per engine is
twice its recent latency, 1..=5 s), counted by `agent web search engines`.

URL reading (`web_read`, crate-internal): the `web.read` host request the
kernel's `rlm.web.read` reaches — one URL to markdown with its outline of
heading anchors and its links. Site routes (arXiv HTML rendering, else the
PDF with its metadata; GitHub raw files, READMEs, issues and pull requests;
Stack Exchange questions with voted answers) fall back to a generic fetch
rendered by content type (readable HTML, PDF pages via `pdf-extract`,
markdown, JSON, text); a `section` narrows the result to one heading's span
or a code view's line range. Bodies, redirects and the whole read are
bounded; adoption counts ride the `agent web reads` event. Non-goals: a
browser (no JavaScript rendering), crawling, and caching.

Site crawling (`web_crawl`, crate-internal): the `web.crawl` host request
the kernel's `web-crawl` reaches. The `spider` crate (vendored, see
`vendor/spider/PRIME_AGENT_PATCH.md`) keeps the frontier — robots.txt
(always honoured), same host, depth, page limit, include/exclude regexes,
sitemap, a polite delay and concurrency — and hands each URL to
`web_read`'s bounded fetcher; each page is rendered by `web_read`'s
renderer into the same object `web.read` returns. `#:render` drives a local
headless Chrome that `web_crawl::browser` starts under the session's
program rules (own process group or Windows job, orphan journal, killed
with the crawl). Results are bounded (200 pages, a per-page cut, a total
budget past which pages page out to the spill store); adoption counts ride
the `agent web crawls` event. Non-goals: stealth, fingerprint or user-agent
spoofing, proxy rotation, ignoring robots.txt, and remote crawl services.

Native file search (`fs_search`, crate-internal): the `fs.grep`/`fs.glob`/
`fs.find` host requests the kernel's `rlm.fs` client reaches — ripgrep's
library crates in process (`ignore` for the parallel `.gitignore`-aware
walk, `globset`, `grep-searcher` + `grep-regex`), so search needs no `rg`,
`grep`, or `find` on any platform. Results are structured (path, line,
column, text, context), capped and clipped, with `/`-separated paths
relative to the REPL's cwd; each call runs on a blocking thread. Adoption
counts ride the `agent fs search` event. Non-goals: a shell or coreutils
replacement, file watching, and indexing.

Anchored file editing (`fs_edit`, crate-internal): the `fs.read`/`fs.edit`
host requests behind the kernel's `rlm.fs.read`/`rlm.fs.edit`. A read tags
each line `LINE:abc` (three letters of a SHA-256 over the line and its
neighbours); an edit replaces, deletes, or inserts by anchor, checks every
anchor against the file first and rejects the whole patch (stale anchors plus
the current lines, nothing written) on any mismatch, then writes atomically
(temp file + rename, through symlinks, permissions kept) with the file's own
line endings, final newline, and encoding (UTF-8 with or without a BOM,
UTF-16 with one; binary files are refused). Each call runs on a blocking
thread; adoption counts ride the `agent fs edit` event. Non-goals: creating,
moving, or deleting files, and the model-facing `edit` tool (unchanged).

Kernel scheduled jobs (`schedule.add`/`schedule.list`/`schedule.cancel`,
crate-internal, in `session_engine/runtime/schedule.rs` over
`cron/store/repl_schedule.rs`): one-shot or recurring prompts the model
schedules for its own session, stored as ordinary cron jobs (source
`repl_schedule`, so `prime-agent schedule list` shows them) that a session
lists and cancels only when it created them. Capped per session with a
minimum recurring interval; an embedding without a scheduler answers with
an error. Adoption counts ride the `agent repl schedule` event. Non-goals:
managing the user's own jobs.

Past-session recall (`session_engine::session_search`, crate-internal): the
`rlm.session.search` and `rlm.session.saved_messages` host requests behind the
kernel's `rlm.session.search(query)` and `rlm.session.messages(session=...)`.
They scan the session directory `/resume` lists, keep the sessions whose
header cwd is this session's, and read files read-only on a blocking thread
(no repair, resume, edit, or delete). Adoption counts ride the `agent kernel
login and recall` event. Non-goals: indexing and cross-project search.

Kernel workspace checkpoints (`session_engine::checkpoint_host`): the
`checkpoint.create`/`checkpoint.list`/`checkpoint.restore` host requests
over the embedding's `WorkspaceCheckpointController` (the daemon worker's
shadow-git store). Restore is gated by the global-only
`replCanRestoreCheckpoints` setting (default off), limited to the session's
own checkpoints, and confirmed by the user through the interaction hub's
question flow; nobody to ask means refuse. Without a controller every
request answers why. Adoption counts ride the `agent repl checkpoint`
event. Non-goals: the store itself (pa-daemon) and the per-turn rewind.

Session usage (`usage_host`): the kernel's read-only `usage.*` host requests the
`rlm.usage` client reaches — `usage.session` (the session's billed spend and token
counts, the same fold `get_session_stats` reports, with the subagents' attributed
share split out) and a no-daemon fallback for `usage.quotas`, whose real handler the
daemon worker registers over the supervisor's `/usage` quota fetch and cache.
`usage_host::branch_spend` is the one spend fold the RPC stats share. Adoption counts
ride the `agent usage reads` event. Every kernel capability's standalone adoption counts go through one generic counter (`session_engine::telemetry` `CapabilityCounts`, written through `SessionCounters::note_capability` under the property names of pa-telemetry's capability rule files) and are emitted from those rule lists at session end; no capability has its own counter struct. Non-goals: cost math (pa-ai bills each call),
quota fetching (pa-daemon), and any credential in a reply.

Session worktrees (`worktree_host`): the kernel `worktree` capability runs its git work in
the kernel (`rlm.worktree`) and reports each operation through `worktree.record`; the host
keeps the session's `WorktreeLedger` (exposed as `SessionEngine.worktrees`) of the worktrees
made and not removed, which `dispose_kernel` names in the session-end notice. Adoption counts
ride the `agent worktrees` event. Non-goals: git operations on the host, and any name, path,
or diff in telemetry.

Parallel hypotheses (`kernel::hypotheses`): the kernel's `rlm.hypotheses` makes one `worktree`
and spawns one child per approach, runs the acceptance check in each checkout, ranks the
approaches (passing first, then the smaller diff), and removes every worktree but the winner's; it
refuses at the RLM depth bound before making any, and each child's boot goes through session-boot
admission. The host only counts each finished run (`hypotheses.record`, the `agent repl
hypotheses` event). Non-goals: merging the winner (the model calls `worktree.merge`), and any
task, approach, or diff in telemetry.

Background cells (`kernel::manager`, `kernel::cell_window`, `tools::ipython`,
`session_engine::bash_notices`): a `python_repl` cell runs for at most its yield window inside
the call (the model's `timeout`, else a `pythonCellTimeoutMs` the user set, else a window learned
per project; daemonless sessions wait for the end). A cell still running then continues in the
background, never stopped for time: the result carries `backgrounded_cell`, the kernel's activity
track counts it as background work, and its end arrives as the `[cell-done]` notice. The learned
window (`kernel::cell_window`, the opaque `CellWindow` policy carried by
`IpythonKernelProvisionerOptions::cell_window`) is about 1.5x the recency-weighted P90 of the
project's cell times, in 10 s..600 s, cold 60 s; its state lives in
`<agent dir>/cell-windows/<project hash>.json`, loaded and saved off every cell path. Adoption
counts (including each window's source) ride the `agent repl background cells` event.
Non-goals: a hard time limit (Nodecode keeps none), stopping a cell from the host for time, and
a user setting for the learner.

Project card (`project_card`): a compact card (build/test/check commands from Cargo, npm, Python,
and Go manifests, the Makefile, and CI `run:` steps; the largest top-level directories; the
build/test commands this project's cells ran, counted) joins the system prompt's dynamic tail as
the `project-card` segment, after the project context and never in the cached static prefix. Its
full detail pages out to the spill store behind a handle; it stays under 1,500 characters. A
session reads the card the previous refresh stored under `<agent dir>/project-cards/` within a
50 ms budget, then re-derives it on a background thread for the next session; a reload keeps the
session's card. Non-goals: running the commands, and deriving on the session-start path.

Symbol index (`symbol_index`): the kernel's `fs.symbol` host request answers one name's
definitions, references, and a spill handle to each definition's source, from a per-project index
kept under `<agent dir>/symbol-index/<project hash>.json`. Definitions come from per-language line
patterns (Rust, Python, JS/TS, Go, C/C++; no parser dependency), references from each file's
identifier set plus a whole-word scan of the files that hold the name. The walk respects
`.gitignore`; a refresh re-reads only files whose size or mtime changed, and a query re-checks the
files it answers from. The first query starts the build and waits at most 300 ms for it; past that
it answers partial. Adoption counts ride the `agent symbol index` event. Non-goals: type-aware
resolution and cross-language references.

Neighbour prefetch (`prefetch`): an `fs.read` host request (the kernel's `fs-read`) pages the read
file's likely next reads out to the spill store and names one handle each in its reply (`related`,
and a last line of its text pointing at `(output-read handle)`), never their text: its test files, the
workspace files it imports (Rust `mod`, Python imports, JS/TS relative imports, C `#include "…"`),
and the files calling what it defines (from the symbol index, once that is built). A decaying
per-project share of handles the model went on to use (a `output-*` read of the handle, or an
`fs-read` of the file) sets how many each read gets, down to none with a one-handle probe every
tenth read; it lives under `<agent dir>/prefetch/` and is shared by every session on the project in
the process. The read waits at most 150 ms for it. Adoption counts ride the `agent prefetch` event.
Non-goals: putting neighbour text in the context.

Rizin in the kernel (`kernel::scheme::rizin`, `session_engine::rizin_usage`): the `rz-*`
functions drive the `rizin` program installed on the user's machine (0.7.4 or newer). Nothing of
rizin is linked: each open session is a `rizin -q0` child in its own process group, started
through the session's scope, that reads one command per line and whose replies are delimited by
an echoed token. A reader thread per session keeps a bounded part of each reply (its first 8 MiB
and last 256 KiB) whatever rizin writes; a text reply over 768 KiB reaches the cell cut around a
spill handle. A command ends with its reply, its `#:timeout-ms`, or its cell: rizin is then
interrupted (SIGINT to its group) and killed if it does not answer within 2 s, after which the
session reports itself dead with rizin's exit status and last output. Every rizin and what it
started (a debuggee) is killed when the kernel is killed, the lifetime ends, or the registry is
dropped. Rizin is looked up on the session's `PATH` at each `rz-open`; without it the open raises
an error that says how to install it. Each counted call sends one fire-and-forget `rizin.used`,
folded into `agent repl rizin`. Non-goals: wrapping rizin's command set (the model types rizin
commands; `prompts/capabilities/rizin.md` maps them), the `rz-*` side programs (they run through
`bash`), installing rizin, and interrupting a command on Windows (it is killed there).

Error enrichment (`kernel::error_context`): a user cell that raised, or a `bash()` command a cell
awaited that exited non-zero (the kernel's `command-failure` display), gets an `[error context]`
block appended to its result: the source lines (±3) around each error location the output names,
from a parser registry built from captured rustc/cargo, Rust panic, pytest, Python traceback and
unittest, tsc, go, and gcc/clang output. Only files inside the session's working directory (and
outside `site-packages`/`node_modules`) are read, each location once, at most six, the block under
6,000 characters; a failed command's full output pages out to the spill store and the block names
its handle. The reads run on a blocking worker under a 250 ms budget, so a hung file system costs
the lines, never the result. Adoption counts ride the `agent error enrichment` event. Non-goals:
fixing the error, and parsing a successful cell's output.

Read-only command cache (`kernel::command_cache`): the kernel answers an allowlisted read-only
`bash()` command (`git status`/`log`/`diff`, `ls`, `rg`/`grep`, `cat`: one simple command, no shell
syntax, no writing option) or an `fs.read` from its last result while the files it read keep their
size, mtime, and inode (`rlm.command_cache`), marked `cached`; `fs.edit` and the `edit` skill clear
it. Each lookup is reported through the fire-and-forget `command_cache.fact` host request.
Adoption counts ride the `agent readonly cache` event. Non-goals: caching anything with side
effects, and a setting.

Call policy (`kernel::call_policy`): an embedder's `CallPolicy` (`SessionEngineConfig::call_policy`,
`SchemeKernelOptions::call_policy`) is asked before every host request reaches its handler and at
the entry of every side-effecting native, by `CallKind` (`HostRequest { kind, payload }`,
`Native { family, name }` with `NativeFamily`). The kernel's handler registries, the session's and
its file tools' own, are wrapped as the kernel is built, so the requests a native sends itself
(`fs.read`, `bash.completed`, `cell.completed`) are asked like a cell's; `rlm.end_turn`, answered
in the kernel, is asked there. The natives ask in their dispatchers (`%native-call`,
`%worktree-call`, `%hypotheses-call`, `%harness-call`, `%factory-request-json`, `%rz-call`,
`%mcp-call`), which the engine Steel runs macro bodies in does not have. A refusal (`CallRefused`)
is the call's error: "refused for an untrusted turn: <call>". `admit` answers at once and never
blocks. No policy asks nothing.

Call contracts (`kernel::scheme::contracts`): every kernel function refuses a call it does not
declare before the function runs, naming what it accepts and the nearest accepted name: an unknown
`#:keyword` (Steel's own binding only counts keywords, so it binds a misspelt one to nothing),
a keyword given twice or without a value, too few or too many arguments, and a symbol where an
optional argument is given (Steel passes over it). The contract is read from each
`(define (name ...) ...)` when its source is loaded through `contracts::load`, which is how every
kernel source and every skill's Scheme source reaches an engine; the options-hash tools (`bash`,
`fs-*`) refuse an unknown key against the table they read from, and `mcp-call` refuses an
argument outside a tool schema that lists its properties and opens no other door. Each refusal is
reported through the fire-and-forget `call.refused` host request (`kernel::call_refusals`) by its
class and rides the `agent kernel call refusals` event. Non-goals: checking or coercing argument
types, a function with a rest parameter (it reads its own rest list), and functions a cell defines
for itself.

Edit checks (`kernel::edit_checks`): the kernel runs the touched file's cheapest check after
`fs.edit` and the `edit` skill (`rlm.edit_check`: fatal-rule `ruff` or `py_compile`, `cargo check
-p`, `tsc --noEmit`, `go vet`), learns per project which checks pay for themselves, and reports
each outcome through the fire-and-forget `edit_check.fact` host request; a late check's diagnostics
ride the existing `bash.completed` notice. Adoption counts ride the `agent edit checks` event.
Non-goals: running checks in the host, and a setting to choose them.

Test-run handles (`kernel::test_runs`): the kernel summarizes an awaited test run itself
(`rlm.test_results`: cargo test, pytest, unittest, go test, jest, vitest) into `BashResult.tests`;
its `tests.summary` host request asks the host to keep the whole output and each failure's section
in the session's spill store and answers their handles (`null` without a store). The spill only
queues the write, so the reply never waits on disk. Adoption counts ride the `agent test summaries`
event. Non-goals: running tests, and parsing in the host.

The shell behind `bash()` (`platform::shell`, `kernel::scheme::native::bash_engine`): a session's
commands run on the built-in shell (`pa-shell`), which runs the script and its utilities inside this
process and starts only the programs the script names, on every platform. A shell process per
command (`shellPath`, else the platform's default shell) is used instead when the `shell` setting
says `system`, or when `shellPath` is set and `shell` does not say `builtin`: a named program is a
request for that program (`resolve_kernel_shell`; global settings only). `PRIME_AGENT_BASH_SHELL`
(`builtin` or `system`) overrides both for one session. A built-in command's programs are each their
own process group (one Windows job object for all of them), killed with their job; its `pid` is a
job number; and each program is listed in the orphan journal as it starts, without the sync to disk
a shell process's record gets (`orphan_journal::note_orphan_process_state`), and taken out within
250 ms of its end. Each command's counts (utilities run in the process, programs started, utilities
handed to their program, utilities with no program to hand to) are reported through the
fire-and-forget `shell.fact` host request (`kernel::shell_runs`) and ride the `agent builtin shell`
event. Always on a shell process: persistent shells (they outlive the daemon), the user's `!`
commands, and a hypothesis's check command. Non-goals: the interpreter and the utilities
themselves (`pa-shell`).

Elevated commands (`kernel::scheme::native::bash_elevated`): `(bash command (hash 'elevated #t))`
runs one command with administrator or root rights once the user consents through the system's own
prompt, as a job like any other (captured output, waits, kills), never queued. Linux and macOS: no
lasting root process; each command goes through `sudo -A` with a graphical askpass program, `pkexec`,
or `sudo -n`, and root's side holds a lifeline pipe that ends the command when it closes. Windows:
the session's first elevated command starts `prime-agent --elevation-helper` through UAC
(`ShellExecuteExW` `runas`, hand-declared), which connects back over a pipe only elevated
administrators can open, proves itself by process id and one-time token, runs the session's
elevated commands with their output piped back, and exits with the session or after 10 idle minutes
(the module's docs state the threat model). A refused or impossible consent is the command's result
(exit 126, `elevation declined:` / `elevation unavailable:`). Each command's outcome and route are
reported through the fire-and-forget `elevation.fact` host request (`kernel::elevation_runs`) and
ride the `agent elevated commands` event. Public surface: `kernel::run_elevation_helper`, the
helper mode `pa-cli` dispatches to. Non-goals: elevating the daemon or a whole session.

Session-boot admission (`kernel::boot_admission`): `admitted_boot(source, priority, boot)` runs one
session boot (a kernel's interpreter start here; the daemon's child worker spawns and
crash-recovery adoption) under a permit of the session-boot budget
(`pa_ai::admission::AdmissionKey::session_boot`), and returns the permit with the boot's latency
and the host's load (`HostLoad`: Linux `/proc` load average and available memory; pressure is an
overload). The width is `pa_types::control`'s controller with
`pa_ai::admission::session_boot_controller_config`; `PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS`
pins it. A permit covers the boot only; a permit granted after its waiter gave up goes straight
back. `take_stats` feeds the `agent session boot admission` event. Non-goals: bounding how many
sessions run (only how many boot at once), and a user setting.

Paged-out cell output (`kernel::spill`): a persistent session's content-addressed store of the
cell output its transcript cut. When a `python_repl` cell's stdout, stderr, or result runs past
its cap, the kernel manager hands the whole text (up to 8 MiB per stream) to the session's
`SpillStore` and writes the spill's handle (`[out:<id> · N chars · L lines · M chars not shown]`)
where the truncation marker was. The host owns the store, not the kernel runtime: the host sees
every byte and owns the session directory, and the store outlives kernel processes. The write
runs on the store's writer thread (bounded queue, bounded retries), never on the cell-result path;
a spill the writer cannot take or keep answers "not stored". Files live under
`<session artifact dir>/spill/` with an append-only `index.jsonl` (stored, failed, evicted), so
handles survive resume and daemon restart; past 64 MiB per session the oldest spills are evicted
and answer "evicted". The `ctx.page`/`ctx.grep`/`ctx.tail`/`ctx.info` host requests
(`kernel::spill::host`) the kernel's `rlm.ctx` client reaches read a handle back in slices capped
at 16k characters, so a read never spills again; a handle without text answers its state. The request view's context
thinning (`session_engine::context_thinning`; off unless `contextThinning.enabled`, and then it
rewrites already-sent results only when the view reaches its threshold) pages each thinned result out the same way
(`ThinnedRecall::Spill`; the handle is a content hash, so the view stays a pure function of the
history), and `OutputBudget` scales a cell's output cap to the context left (an eighth of the
remaining tokens in characters, 8k..64k), published by the request path and read by the cell
executor through atomics. Adoption counts ride `agent context paging`. Public surface: `SpillStore::for_session_dir`, `OutputBudget`, `ThinnedRecall`, and the
`ExecuteOptions::spill` slot. Non-goals: the kernel's own in-process caps (the 1 MiB result
transit cap and `bash()`'s 2 MiB head/tail buffer bound memory before the host sees the text)
and any cross-session sharing.

Surprise-weighted previews (`kernel::spill::preview`, `kernel::spill::noise`): where a cell's cut
output pages out (`kernel::scheme::cell`), and where a thinned tool result pages out, the text
shown is chosen by information within the same budget: head/tail anchors, error/warning/failure
lines (an error's own location ranks with it), then lines with the rarest tokens of that output;
each distinct line once with its repeats as `×N`, kept lines in order with `[… N lines …]` gaps,
then the handle. A cell's preview is computed over the 262,144 bytes it keeps, and a note after
the handle counts what was printed past them. Line shapes a project's command family prints every
run (three runs, decaying) collapse into one `[… N noise lines …]` marker, unless ever seen on or
beside a failure line; the family is the program of the cell's first `(bash …)` command, else the
tool's name. The estimator is per project and shared by that project's sessions in the process
(poison-tolerant locks); a session previews with the set known at its start (50 ms bounded read
under `<agent dir>/preview-noise/`) so its request views stay a pure function of history, and
saves what it learns fire-and-forget on a scope-propagating thread that writes nothing once its
session lifetime has ended, and nothing over a store it could not read. Adoption counts, with the
`output-*` reads that followed a preview, ride the `agent previews` event. Non-goals: encoding, any
model or dependency, and previewing `bash`'s own `'output` (it is returned whole beside its
handle; it is previewed when a cell prints it).

Kernel capability manifests (`kernel::capabilities`, crate-internal): one TOML file per Python
kernel capability beside the module, embedded with `include_str!`, recording each capability's
runtime-ready probe statements, closure seed, bootstrap binding, plumbing host requests, and
standalone telemetry event. The runtime-ready probe (built once, on the first kernel start that
probes) and the closure freeze's seed derive from them; its tests hold the bootstrap and the
telemetry catalog equal to them. Non-goals: capability behavior, what the model is told, and any
public API.

Kernel API reference (`prompts::layers`): one doc file per topic under `src/prompts/capabilities/`
documents every model-facing function of the Scheme kernel. The core prompt layer splices each
function's call form from them, and the kernel serves a function's full entry on demand through
`(api-help ...)`, and `(api-help #:search query)` ranks its passages (function entries, bullets,
paragraphs) against a query with BM25 (`prompts/layers/reference.rs`), so a cell finds a function
by what it does. `tests/prompt_guards.rs` holds the reference equal to the functions the kernel's
sources define. Public surface: `API_DOCS`, `documented_calls`, `CORE_LAYER`.

Event bus kernel bridge (`session_engine::event_bus_host`): the `event.*` host requests behind
the `event-*` kernel functions, each parsed into a typed `pa_types::events::EventBusRequest` and
sent through an `EventBusController`; the bus itself lives in the daemon's supervisor or in a
headless run's runtime (pa-runtime). A run with neither registers `NoEventBus`, which refuses
every call with the reason; a daemon worker replaces it with its link-backed controller, a
runtime session with the runtime's. `event-define`'s `#:timer`, `#:watch` and `#:webhook` make a
timer, folder-watch or webhook source; `agent_state::EventBusState::apply` takes them only from a
bus that runs them (`SourceKinds::Accepted`, a runtime's) and refuses them on the old daemon's
(`SourceKinds::BusOnly`), and stores them only where its runtime keeps them
(`StoredSources::Every`). A webhook's record holds its URL and which secrets authenticate it,
never a secret: its runtime fills in the port and the secrets before the request is applied. Public surface: `EventBusController`,
`NoEventBus`, `register_event_bus_host_handlers`.

Session-event wire builder (`session_engine::wire_events`): the `session_event` payloads a
daemon sends its clients for a session's turns, kept here once for the hosts that send them
(the supervisor's worker and the runtime daemon's `agent/<id>/stream`): the session wire
forms of the loop's messages, stream deltas (the reply's `start` included) and tool results, how
a streamed update travels (`update_route`: a delta parks, the `start` and a block end go out
direct as `direct_update_event`), the single-slot `message_update` coalescer throttled on both
edges with its merged additive deltas and its once-per-flush live `contextUsage` estimate, and
the adaptive flush interval. A host frames what the coalescer flushes and owns the sequence each
update consumed and the flusher's timer. Non-goals: the print modes' json stream
(`session_events::agent_event_json`, one line per loop event, uncoalesced), the framing, the
transport, and the session store's context reading. A host without the store (the runtime
daemon) reads the settled rows' `contextUsage` over the engine's messages with
`settled_context_usage`, by the store reading's rules (the conversation rows only, anchored on
the last reply's usage, unknown right after a compaction). Public surface: `AssistantSnapshot`,
`session_wire_value`, `stream_event_value`, `tool_result_wire_value`, `UpdateRoute`,
`update_route`, `direct_update_event`, `streaming_context_usage`, `settled_context_usage`,
`UpdateCoalescer`, `FlushPacer`, `UPDATE_FLUSH_INTERVAL`, `LAGGING_BACKLOG`.

Session resource reload and settings view (`session_engine::resource_reload`,
`session_engine::settings_view`): `rlm.reload` (and the daemon's `/reload`) re-runs
resource discovery with the session's creation inputs, rebuilds the system prompt's
dynamic tail from the next call, keeps the current skill and template lists for
`/skill:` and the command catalog, and installs new Python skills into the kernel
venv for the client to bind (later kernel boots bind them too). `settings.session`
answers the live model facts (context window, thinking level) and the armed limits
field by field; `settings.host` is the embedding's half (RLM depth bound, autonomous
status), with a no-daemon fallback. `model.info` carries the context window and
thinking level. Adoption counts ride `agent repl reload`. Non-goals: changing any
setting, and passing a settings document (or any credential) to the kernel.

Provider auth descriptors (`auth/descriptor.rs`, `auth/resolver.rs`,
`auth/token_exchange.rs`): the optional models.json provider `auth` block,
validated at load, and the one request-auth seam (`AuthResolver`, crate
internal) the registry's per-call lookup resolves every inference call
through — `StaticKey` (today's key, in the provider's own key slot) or
`TokenExchange` (the auth.json `token_exchange` credential traded for a
short-lived token at the declared endpoint, cached in the same entry,
single-flighted per provider and account across threads and processes, and
invalidated once when the provider rejects it). Resolution runs only on the
inference path, never at startup.
The descriptor's optional `fallback` list (any strategy, built-in providers
included through a models.json override) is the opt-in quota fallback
(`models/registry/quota_fallback.rs`): `ModelRegistry::arm_quota_fallback`
answers a `QuotaFallback` when a request failed with a reset beyond the retry
cap, noting the serving credential exhausted in memory (`AuthStorage`) until
the reset, and the per-call lookup then resolves through the first listed
provider whose credential resolves and that serves the same model id: its
credential and transport (base URL, API, compat) under the session's own
provider and model id. The retry driver takes it as its `quota_fallback`
seam: one immediate retry per turn, else the raw error with a note.
Validation (known ids, no self-reference, repeats, or cycles) runs at load.

The `oauth` descriptor strategy (`auth/descriptor_oauth.rs`,
`auth/descriptor_oauth_login.rs`): `DescriptorOAuth` serves the provider's
stored `oauth` credential (bound to the descriptor's token endpoint and
client) and refreshes it before expiry, single-flighted per provider in
process and across processes, persisting a rotated refresh token in the same
guarded write; a failed refresh is the "sign in again" error. The sign-in is
public API for the CLI's `login` command and the `/login` panel:
`sign_in_with_descriptor` (authorization code + PKCE S256 through the MCP
loopback callback server bound to 127.0.0.1 with a server-side `state` check,
or the RFC 8628 device-code flow), `oauth_descriptor_providers`, the
`OAuthSignInUi` surface trait, and the `SignInFlow`/`SignInTiming`/
`SignInMethod`/`SignInAttempt`/`SignInError` values. The sign-in never runs
on a paint path.

Session todos and user questions (`interaction`): the host side of the kernel's `todo.*`
and `ask(...)` — the `todo.get`/`todo.set`/`todo.update` and `question.ask`/`question.cancel`
host requests, the todo list persisted as `session_todo_list` custom entries (the branch's
newest wins; the session window tracks it like the goal state), and one open question whose
wait parks on the reply, the timeout, and the embedding's presence notification. The embedding
supplies the `InteractionSurface` (the daemon worker's attached clients); without one the
session is headless and `ask` settles as `no_user` at once. Adoption counts ride the
`agent todos and questions` telemetry event. The kernel's `todo.set`/`todo.update` also show the
new list on the running cell (the `todo` display MIME; the cell keeps its last as the tool
result's `todoList` detail), so the transcript records each plan change; a settled `ask(...)`
shows likewise (the `question` display MIME, every one kept as the `questions` detail). A choice
question also takes the user's own typed answer (`allowOther`; a host confirmation never does),
and a timed question carries its deadline (`expiresAtMs`) for the surfaces' countdown.
The kernel's `notify(...)` rides the same hub (`notify.send`: an
`InteractionEvent::Notified` published fire-and-forget, `delivered` answered at once from the
surface's presence). The opt-in `model.set_effort` (`session_engine::effort_switch`) is gated on
the global-only `replCanChangeEffort` setting and hands the validated level to the embedding's
`model.apply_effort` handler when one is registered (the daemon worker), else sets the agent's
level and records the session row. Adoption counts ride `agent repl notify`.


In-process Scheme kernel (`kernel::scheme`): one Steel engine per session inside this
process, taken from a spare built ahead of time (never a copy of another engine: copies
share interpreter state across sessions). Cells run on a blocking thread with their output
captured, an abort or the per-call timeout interrupts them, and `(%host-request type payload)`
reaches the same `HostRequestHandlers` the Python kernel's host requests do. It implements the
tool seam's executor and provisioner traits, and every session exposes it as the `eval` tool
(`tools::scheme`) beside `python_repl`.

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No update coordination (the pa-cli coordinator owns the FSM; the daemon side of its restart is `update::restart`, below; the rest of the update-flow seam here is the daemon-free support layer: `update::version` (semver/channel policy), `update::install` (the managed install-root layout), `update::release` (the channel manifest fetch), `update::download` (sha256-verified archive download + staging), `update::installer` (the installer-takeover funnel plus `BUILD_DISTRIBUTION`, the compile-time `PRIME_AGENT_DIST_*` stamp that points a distribution build's updates at its own releases and prefix and names its own command; unstamped builds are `Distribution::Official`)). The package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `user_bash` (the user's `!` commands, moved here from pa-daemon so both daemons share them: the `UserBash` slot (one command at a time, `claim`/`release`, awaited runs via `begin_awaited`, `abort`, `abort_and_settle`), `run_bash` over `RunBash` (a shell process in its own process group, stdout and stderr merged, sanitized, the 50KB/2000-line tail window and the private spill file, a `ChunkSink` for each `bash_output`), `BashEnd`/`BashResult`, the `bash_start`/`bash_end` event shapes (`start_event`, `BashEnd::end_event`, `bash_identity`, `merge_identity`) and `record_in_engine`, the `bashExecution` row saved and joined to the agent's messages, or steered into a running turn), `platform::process::{split_process, wait_child, parent_pid, Split, ChildExit}` (the process split and child wait the daemon's restart owner is built on; `split_process` requires a single-threaded caller), `export_html` (the HTML exporter plus its renderer seam: `tools_section`, `pre_render_custom_tools` + `ToolHtmlRenderer`/`RenderedToolHtml`/`RenderedToolResult`, and `ansi_to_html::ansi_lines_to_html`/`tool_render::trim_rendered_result_lines` for renderer implementers), `ToolRegistry`, kernel manager, settings (`SettingsManager`; `settings::Profile` + `load_profile`/`save_profile`/`list_profiles`/`profile_path`/`validate_profile_name` and the process-wide `set_active_profile`/`active_profile`: the `profiles/<name>.json` overlays, layered global < profile < project, with `PROFILE_ENV_VAR`/`PROFILES_DIR`/`DEFAULT_CREDENTIAL`; `settings::import_official_config` + `OfficialConfigImport`: a distribution build's one-time first-run copy of the official agent dir's configuration - never its sessions or runtime state - into its own agent dir, staged and renamed into place; `settings::plan_official_history_import` + `HistoryImportPlan`/`HistoryImportCheck`/`HistoryImportReport`/`HistoryImportRun`, `skip_official_history_import`, and `HISTORY_IMPORT_MARKER`: the one-time background copy of the official agent dir's chats, session artifacts, and harness memory into a distribution build's own dir — copy only (a clone where the filesystem can, never a link), merged without overwriting, per-file staged and published, the marker written last — plus the configuration entries the dir still lacks; `session_engine::telemetry::track_sessions_imported` reports it as `agent sessions imported`), packages (`packages::PackageManager` + source types), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children and by pa-runtime for the headless modes' children, each a runtime session; the default is no children; the trait crate also owns the shared spawn/depth facts — `DEFAULT_RLM_MAX_DEPTH`, the depth-limit and name errors, the daemonless `create_session` refusal, `rlm_child_session_dir`, `resolve_child_cwd` (a requested child cwd against the session's), the roster text caps in `rlm_notices` (`cap_text`, `compact_rlm_text`, `ANSWER_PREVIEW_MAX_CHARS`, `ANSWER_TEXT_MAX_CHARS`, and a child's one-line roster label `rlm_child_label` with `LABEL_MAX_CHARS`, moved from pa-daemon so both daemons label a child alike), and the `persisted_rlm_max_depth`/`resolve_rlm_max_depth` precedence chain), `session_engine::rlm_child_model` (a child's model resolution against the credential-backed catalog, `resolve_child_model` with the `allowedModels` gate and `resolve_child_model_unchecked` without it, and `assert_thinking_supported`; moved from pa-daemon, read by the supervisor's children and the daemonless print host's), `session::hold_refusal` (the refusal a create or open answers when a live process the daemon does not host holds the session's lease: `HoldIdentity` with `from_error`, and `refusal_message`, the holder classified best-effort from its process image; moved from pa-daemon, read by the supervisor's create and the runtime print path), `session_engine::rlm_ledger` (the RLM spawn ledger, moved from pa-daemon: `RlmSpawnLedger` with its record grammar, bounds, replay and deleted-spend fold, `with_legacy_usage` for the embedder's legacy-tombstone usage reader, and the children's display files `read_rlm_subagent_display`/`write_rlm_subagent_display`; both daemons write it), `session::branch_export` (the `/export <file>.jsonl` export: `output_file`, `branch_body`, the current branch re-chained under a fresh header, and `write`; moved from pa-daemon so both daemons' `export_jsonl` write it), `session::usage_summary` (`SessionUsageSummary`, `ModelUsageSummary`), `session_engine::agent_messaging::{AgentMessageWaits, AgentMessageArrival, wait_for_arrival, wait_cycle}` (the `agent_message.wait` receiving side and its park, cycle walk and timeout, both daemons'), `session_engine::semantic_edges` (the TS `semantic-edges.ts` WRITER: `SemanticEdgeRecorder`/`SemanticEdgeIdentity`, `semantic_edge_ledger_path` + `last_committed_request_id`, and the `X-ACP-Model-Request-ID`/`Idempotency-Key` constants — one durable per-session ledger of model-request events, byte-compatible with the TS lines; the fold and any publishing stay downstream), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through), `mcp::McpManager::connection_roster` + `mcp::McpConnectionEntry` (the `/mcp` view's roster: every resolved integration with its connected state, display kind, transport, and generic-listable flag), `mcp::McpManager::service_catalog_views` + `service_catalog_diagnostics` (the `/mcp` view's resolved service-catalog cards and their diagnostics, answered from local state - the TS `buildServiceCatalogViews` parity; the picker never round-trips the kernel), `mcp::McpManager::api_key_credential_views` + `mcp::API_KEY_CREDENTIALS`/`McpCredentialView` (the `/mcp` view's api-key credential rows - the stored keys the surface manages alongside the connections, e.g. the web-search key - configured from the shared auth store), `models` (the registry and resolver, plus `models::order_for_picker`: the picker-catalog order the composition root applies to its snapshot), `session_engine::provider_adapter` (`switchable_stream_fn`/`ProviderTarget`: the live provider-target slot a host swaps on `set_model`; the adapter bridges the loop's abort into pa-ai's `StreamOptions::signal` cancellation token, so a turn abort cancels the in-flight HTTP fetch at the transport — closed by `ModelStream::close` and the stream's drop; `switchable_stream_fn_with_live_auth` + `LiveAuthResolver` (returning `models::ResolvedRequestAuth`): the same slot plus a per-provider-call request-auth resolution, the TS streamFn's `getApiKeyAndHeaders` — the sync resolver runs off the executor on a blocking thread raced with the turn's abort signal; the resolution is the call's whole request auth (its key, headers, and the per-call request model — the xAI shape, TS `requestModel` — or the configured `models.json` fallback key when no credential serves), a failed resolution fails the call with its auth error, and a captured pair never resends), `session_engine::ipython_state` (the post-compaction `ipython_state` kernel-persistence notice: the `CompactionKernelProbe` view of the kernel provisioner the engine wiring binds via `AgentSession::set_kernel_state_probe`, and the notice row that rides `CompactRun` for the surfaces to broadcast). `session_engine::better` (`BetterHost` on `SessionEngine::better` with `BetterRequest`/`BetterMode`/`BetterOutcome`: the `/better` read-only prompt-rewrite helper - a side run on the session's model, prompt, and tool definitions whose tools only read, see `docs/better.md`). `session_engine::session_commands` (the `/compact`/`/refine`/`/goal`/`/autonomous`/`/better` executor: the durable echo/result rows with their persistence order - the echo ahead of the command so its own work sees it - the compaction/refinement/goal/autonomous arms, and the post-execution live-context rebuild that keeps the rows in the agent state; hosts own the wire surface), `session_engine::auto_refine_trigger` (the compact-trigger auto-refine machine on `AgentSession`: the arm/discard/increment accessors and `consume_compact_auto_refine`, the shared gate/review/stamp sequence every scheduling surface applies - TS `_compactAutoRefinePending`/`_lastAutoRefineReviewAt`/`_assistantTurnsSinceAutoRefine` plus `_maybeAutoRefine`'s compact arm, the serialized checkpoint's compact step, and dispose's serialized drain, with the surface parameter carrying the one behavioral difference between a boundary and a disposal). `session_engine::goal_boundary` (the goal arms of the natural turn end on `SessionEngine`: the `--goal` construction seed with its next-turn context row - TS `_pendingNextTurnMessages` on `AgentSession` - the usage accounting, the budget-limit steer mint, the natural continuation mint with its slot rollback, the RLM deferral behind unsettled descendant work (mark/drop/take owed, the wake-up ownership probe, and the latest settled turn the post-settle deliveries decide from), and the terminal-error finish; the transports - the print driver's in-loop hook, the ACP settle loop, the daemon worker's queue - drive the one goal driver through these methods). `SessionEngine::expand_skill_submission` + `AgentSession`'s prompt-path expansion (the `/skill:<name>` submission seam, TS `_expandSkillCommand`: `skills::expand_skill_command` builds the `<skill ...>` block over the shared `pa_types::skill_blocks` parse, the accepted-turn row and the admitted user message agree on the expanded text, and the `skill_use_count` session counter counts at the admission). `interaction` (`InteractionHub` — `headless`/`with_surface`, `todos`, `open_question`, `answer`, `presence_changed`, `cancel_open_question` — plus the `InteractionSurface` trait the embedding implements and its `InteractionEvent`s), `web_search` (`WebSearchEngine::shared`/`search` over `SearchRequest` + `Credentials` -> `SearchReport`/`SearchError`, the `ProviderId`/`ProviderAccess`/`WebSearchProvider` catalog with `KEYED_CREDENTIALS`, and `WebSearchHost` registering the `web.search` handler with an optional `SearchObserver`), `session_engine::effort_switch::{MODEL_APPLY_EFFORT_REQUEST_TYPE, REPL_CAN_CHANGE_EFFORT_SETTING}` (the request the daemon worker answers with its thinking switch, and the setting that gates it), `SettingsManager::get_repl_can_change_effort`, `SettingsManager::get_favorite_models`/`set_favorite_models` (the `/model` picker's `favoriteModels` list), `session_engine::resource_reload::{ResourceReload, ReloadReport}` (`SessionEngine::resource_reload`: the current `skills`/`prompt_templates`/`base_prompt` and `reload`), `session_engine::settings_view::SETTINGS_HOST_REQUEST_TYPE` (the request the daemon worker answers with its depth bound and autonomous status), `usage_host` (`branch_spend`, the active branch's billed usage, and `USAGE_QUOTAS_REQUEST_TYPE`, the request the daemon worker answers with its quota report), `session_engine::billing::BillingObserver` + `AgentSession::arm_billing_stamp` (the billing-mode stamp: each recorded assistant message's `usage.billing`, classified by pa-ai from the serving model, the provider credential's kind - stored sign-in, then environment key, never the secret - and the recorded cost; the engine wiring arms it and counts each mode into `agent billing modes`), `session_engine::billing::catalog_billing_mode` (the same classifier over a catalog model and the auth store's document with no recorded cost: the daemon's per-model `billing` map in the `get_model_catalog` response), `AgentSession::prompt_with_auto_retry` (a daemonless host's user turn - print mode's - under the shared `session_engine::auto_retry` driver: a provider failure retries per the policy, and a final reply `session_engine::provider_retry::settle_final_reply` finds incomplete with no visible answer retries once, and one cut off at the output limit (`provider_retry::OUTPUT_LIMIT_KIND`) is continued: the driver asks the attempt for `auto_retry::TurnAttempt::ContinueAfterOutputLimit`, which `AgentSession::continue_after_output_limit` serves by keeping the cut reply and admitting the hidden `messages::OUTPUT_LIMIT_CONTINUATION_CUSTOM_TYPE` row, until `auto_retry::MAX_CONSECUTIVE_OUTPUT_LIMIT_CUTS` cuts in a row fail the turn; a failure that remains is the loop's trailing assistant message; the daemon's model turn settles and continues its replies through the same seams). `update::restart` (the daemon side of the update restart, moved from pa-daemon's `update_prepare.rs`, `supervisor/update_restart.rs` and `update_roster.rs` so both daemons serve it: `UpdateRestart` — the prepare transaction `Draining -> Fenced -> Snapshotted -> Prepared -> Stopping` with its watchdogs (`PrepareCoordinator`, `PrepareState`, `AbortReason`/`AbortOutcome`, `BeginOutcome`, `PrepareOp`), the admission gate and mutation drain in front of every other command (`admit` -> `MutationGuard`, `MutationDrainLatch`), `prepare` over the daemon's own roster snapshot, `commit`/`commit_reply`/`closing_payload`/`abandon`, `watchdog`, the replies and refusals (`UpdateRefusal`) the old supervisor's verbatim — `socket_hash`, `now_ms`, and `update::restart::roster`: `assemble` (the roster from the daemon's session and worker rows, with the subagent rows from the spawn ledger and the heartbeat rows from the scheduled jobs), `RosterInputs`, `live_child_parents`, `scan_scheduled_jobs`, `process_identity`, `file_stem`). `update::restart::restore` (the update boot's restore, moved from pa-daemon's `update_restore.rs`: `boot_sweep`, `consume_roster_env`, `RestoreProgress` — the hello resume contract, `status_body` (the `update_restore_status` reply), `await_target`/`failure_for` (an attach queued behind its row, its typed failure), `settle_target`/`settle_adopted` — `restore_pass` over a daemon's `RestoreHost` (the trait the daemon implements: find, open and route old worker commands to its sessions), `restore_legacy_session` (the TS manifest's replay over a `RouteReply` route), `remap_legacy_parent`, `validate_actions` (`restore_actions`'s snapshot check, both daemons'), `report_dormant_jobs`, `selector_matches`, `UPDATE_RESTORE_CLIENT_ID`). `session::hosted::process_start_id` (a hosted lifetime's generation, else the process's start id). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only). The Scheme kernel: `kernel::SchemeKernelProvisioner`, `kernel::SchemeKernel`, `kernel::prewarm_scheme_kernel`, and its call policy seam: `kernel::{CallPolicy, CallKind, NativeFamily, CallRefused}` with `SessionEngineConfig::call_policy`. The kernel-notice seam: `session_engine::bash_notices` (`KernelNotice`, `NoticeDelivery`, `NoticeAdmission`, `KernelNoticeSink`) with `SessionEngineConfig::bash_notice_wake` (the in-process sink, which steers each notice onto the session's agent and fires the wake) or `SessionEngineConfig::kernel_notice_sink` (an embedder's own admission, which wins; a consumed notice still withdraws its queued row). `reuse` (the reuse ledger under `<agent dir>/reuse/ledger.json`: `Recorder` with `Event`/`Creation`/`EntryKey`, `load`/`forget`, `report` -> `ReuseRow` with its `Standing`, `skill_standings` for the inventory's `<reuse>` marks, and `prune` -> `Pruned`, the user's removal of the entries they name; counts, timestamps, session ids and a hash of the project directory, never content; `prime-agent reuse` and the kernel's `(reuse-report)` read it, and nothing in it deletes or demotes an entry on its own).

## Depends on
pa-types, pa-ai, pa-models, pa-agent, pa-shell, pa-telemetry, pa-log, pa-sources (one-way). pa-sources owns the schedule grammar and evaluator (`pa_sources::schedule`) the scheduled jobs read their schedules with, the one timer sources use.

The Unix platform process API exposes `current_user_id()` for the daemon
endpoint namespace to match the TypeScript `process.getuid()` contract on
macOS as well as Linux. This reads the OS identity without filesystem or
subprocess work.

Automatic trace delivery (`agent_traces::ContinuousTraceUpload`) owns consent-gated
persist scheduling and startup outbox recovery alongside the existing manual uploader.
Hosts reuse `load_settings` to capture consent before their existing settings parse,
attach `persisted` after successful disk writes, and own the returned installation
for the session lifetime. Replacements use `rebind` with the effective cwd; forks
use `forked`. The installation never drains on drop. A detached process-wide runtime
owns all transcript reads, scans, settings reloads, networking and retries; a maximum
of 256 registered controllers bounds resident work, and every request (live delivery, its
recovery sweep, and `upload_all_traces`) takes a permit from one shared, background-class trace
controller (`TraceRequestGate::shared`: `pa_types::control` through pa-ai's permit pool, 5
requests a minute and 2..=4 in flight, cut on 429/503 with `Retry-After`). Admission
returns `None` at capacity and logs rejection; hosts attach no hook in that case.
Retired descriptors free capacity only after background recovery acknowledges them.
Session replacement transfers its existing slot and cancels the predecessor; forks
require a distinct slot. Replacement retains a bounded retirement flag until the
background recovery snapshot acknowledges its exact controller identity. The synchronous
persist seam checks the cached consent snapshot and performs an exists/create
pending-marker write; consent metadata reads and reloads stay in the worker after
the host captures its initial settings generation; it does not fsync the marker (matching TS v0.9.8 process-crash
recovery rather than promising power-loss durability). A short nonblocking mutation lease prevents cursor/prune races; contention
records one stable fallback marker per session, which is retained conservatively
until a future sender has delivered it. Its cost must be measured
separately; startup/paint parity is not established by scheduler unit tests.
Active requests monitor consent changes every second on the worker and cancel on
revocation/errors; bytes already sent cannot be recalled. Recovery belongs to all
live hosts sharing the agent directory and cancels when the last host disappears.
Catch-up uses at most three delivery cycles per pending entry, honors Retry-After,
and releases upload capacity during retry waits; exhaustion retains durable intent.

`AgentSession::start_refinement` starts an ephemeral background job and returns its
completion handle. Session disposal or branch navigation cancels planning and idle-boundary waiting;
a commit already writing durable state finishes its audit and context append.
Refinement owns the harness storage and session rows; the lower-level agent
only owns atomic admission exclusion. No new dependency is introduced.
Completed commits remain visible after resume through the harness digest, and
unchanged state does not append another digest. Live refinement and digest
refresh append context rows without replacing earlier rows. This preserves the
message prefix when the system prompt, tools, model and provider construction
are unchanged; it does not promise cache identity across restart or compaction.
