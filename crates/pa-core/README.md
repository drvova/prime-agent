# pa-core

The session engine.

## Scope
MCP host side (`mcp/`): auth gating for built-in integrations (the
disabled-skill overrides and `/mcp list` status), the `mcpServers` settings
seam, and the `mcp.*` host-request handlers the kernel's generic MCP
client reaches through (`mcp.config`, `mcp.refresh`, optional
`mcp.begin_login`). The OAuth flow lives behind it (`oauth*` submodules:
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
Platform wall (`platform`): process control (signals/process groups, kernel exit waits), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations (the durable-write rename primitive is pa-telemetry's `rename_onto`, re-exported as `platform::rename_onto`). Scheduled jobs (`cron`, the `AgentCronJobStore` port of `core/cron-jobs.ts`): file-backed job state under session artifacts (`scheduled-jobs.json` partitions) with cross-process locking, plus the public read-only scan (`cron::store::read_scheduled_jobs_artifact`) the update flow's roster projection and boot re-arm read. Tools (python_repl + internal rename/stdout), rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). origin/main

Provider resilience policies in `session_engine`: the shared quick-retry
policy (TS `provider-retry.ts`: permanent-kind classification,
Retry-After-aware capped delays), the interactive auto-retry loop, and the
provider-failover driver (when a provider exhausts its quick retries and
another configured provider serves the same model, the failed turn
re-routes to the next provider in catalog order; the TS
`auto_retry_start`/`reason: "backup"` event vocabulary surfaces the
progression). The drivers are pure decision logic: the host (pa-daemon)
owns the attempt, the wait, and the model switch.

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
kernel; the HTTP client is built on the first search, never at startup.

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
ride the `agent usage reads` event. Non-goals: cost math (pa-ai bills each call),
quota fetching (pa-daemon), and any credential in a reply.

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

Session todos and user questions (`interaction`): the host side of the kernel's `todo.*`
and `ask(...)` — the `todo.get`/`todo.set`/`todo.update` and `question.ask`/`question.cancel`
host requests, the todo list persisted as `session_todo_list` custom entries (the branch's
newest wins; the session window tracks it like the goal state), and one open question whose
wait parks on the reply, the timeout, and the embedding's presence notification. The embedding
supplies the `InteractionSurface` (the daemon worker's attached clients); without one the
session is headless and `ask` settles as `no_user` at once. Adoption counts ride the
`agent todos and questions` telemetry event.
The kernel's `notify(...)` rides the same hub (`notify.send`: an
`InteractionEvent::Notified` published fire-and-forget, `delivered` answered at once from the
surface's presence). The opt-in `model.set_effort` (`session_engine::effort_switch`) is gated on
the global-only `replCanChangeEffort` setting and hands the validated level to the embedding's
`model.apply_effort` handler when one is registered (the daemon worker), else sets the agent's
level and records the session row. Adoption counts ride `agent repl notify`.


## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No update coordination (the pa-cli coordinator owns the FSM; the update-flow seam here is the daemon-free support layer: `update::version` (semver/channel policy), `update::install` (the managed install-root layout), `update::release` (the channel manifest fetch), `update::download` (sha256-verified archive download + staging), `update::installer` (the installer-takeover funnel plus `BUILD_DISTRIBUTION`, the compile-time `PRIME_AGENT_DIST_*` stamp that points a distribution build's updates at its own releases and prefix and names its own command; unstamped builds are `Distribution::Official`)). The package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `export_html` (the HTML exporter plus its renderer seam: `tools_section`, `pre_render_custom_tools` + `ToolHtmlRenderer`/`RenderedToolHtml`/`RenderedToolResult`, and `ansi_to_html::ansi_lines_to_html`/`tool_render::trim_rendered_result_lines` for renderer implementers), `ToolRegistry`, kernel manager, settings (`SettingsManager`; `settings::import_official_config` + `OfficialConfigImport`: a distribution build's one-time first-run copy of the official agent dir's configuration - never its sessions or runtime state - into its own agent dir, staged and renamed into place; `settings::plan_official_history_import` + `HistoryImportPlan`/`HistoryImportCheck`/`HistoryImportReport`/`HistoryImportRun`, `skip_official_history_import`, and `HISTORY_IMPORT_MARKER`: the one-time background copy of the official agent dir's chats, session artifacts, and harness memory into a distribution build's own dir — copy only (a clone where the filesystem can, never a link), merged without overwriting, per-file staged and published, the marker written last — plus the configuration entries the dir still lacks; `session_engine::telemetry::track_sessions_imported` reports it as `agent sessions imported`), packages (`packages::PackageManager` + source types), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children and by pa-cli's print mode for in-process children; the default is no children; the trait crate also owns the shared spawn/depth facts — `DEFAULT_RLM_MAX_DEPTH`, the depth-limit and name errors, the daemonless `create_session` refusal, `rlm_child_session_dir`, and the `persisted_rlm_max_depth`/`resolve_rlm_max_depth` precedence chain), `session_engine::semantic_edges` (the TS `semantic-edges.ts` WRITER: `SemanticEdgeRecorder`/`SemanticEdgeIdentity`, `semantic_edge_ledger_path` + `last_committed_request_id`, and the `X-ACP-Model-Request-ID`/`Idempotency-Key` constants — one durable per-session ledger of model-request events, byte-compatible with the TS lines; the fold and any publishing stay downstream), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through), `mcp::McpManager::connection_roster` + `mcp::McpConnectionEntry` (the `/mcp` view's roster: every resolved integration with its connected state, display kind, transport, and generic-listable flag), `mcp::McpManager::service_catalog_views` + `service_catalog_diagnostics` (the `/mcp` view's resolved service-catalog cards and their diagnostics, answered from local state - the TS `buildServiceCatalogViews` parity; the picker never round-trips the kernel), `mcp::McpManager::api_key_credential_views` + `mcp::API_KEY_CREDENTIALS`/`McpCredentialView` (the `/mcp` view's api-key credential rows - the stored keys the surface manages alongside the connections, e.g. the web-search key - configured from the shared auth store), `models` (the registry and resolver, plus `models::order_for_picker`: the picker-catalog order the composition root applies to its snapshot), `session_engine::provider_adapter` (`switchable_stream_fn`/`ProviderTarget`: the live provider-target slot a host swaps on `set_model`; the adapter bridges the loop's abort into pa-ai's `StreamOptions::signal` cancellation token, so a turn abort cancels the in-flight HTTP fetch at the transport — closed by `ModelStream::close` and the stream's drop; `switchable_stream_fn_with_live_auth` + `LiveAuthResolver` (returning `models::ResolvedRequestAuth`): the same slot plus a per-provider-call request-auth resolution, the TS streamFn's `getApiKeyAndHeaders` — the sync resolver runs off the executor on a blocking thread raced with the turn's abort signal; the resolution is the call's whole request auth (its key, headers, and the per-call request model — the xAI shape, TS `requestModel` — or the configured `models.json` fallback key when no credential serves), a failed resolution fails the call with its auth error, and a captured pair never resends), `session_engine::ipython_state` (the post-compaction `ipython_state` kernel-persistence notice: the `CompactionKernelProbe` view of the kernel provisioner the engine wiring binds via `AgentSession::set_kernel_state_probe`, and the notice row that rides `CompactRun` for the surfaces to broadcast). `session_engine::session_commands` (the `/compact`/`/refine`/`/goal`/`/autonomous` executor: the durable echo/result rows with their persistence order - the echo ahead of the command so its own work sees it - the compaction/refinement/goal/autonomous arms, and the post-execution live-context rebuild that keeps the rows in the agent state; hosts own the wire surface), `session_engine::auto_refine_trigger` (the compact-trigger auto-refine machine on `AgentSession`: the arm/discard/increment accessors and `consume_compact_auto_refine`, the shared gate/review/stamp sequence every scheduling surface applies - TS `_compactAutoRefinePending`/`_lastAutoRefineReviewAt`/`_assistantTurnsSinceAutoRefine` plus `_maybeAutoRefine`'s compact arm, the serialized checkpoint's compact step, and dispose's serialized drain, with the surface parameter carrying the one behavioral difference between a boundary and a disposal). `session_engine::goal_boundary` (the goal arms of the natural turn end on `SessionEngine`: the `--goal` construction seed with its next-turn context row - TS `_pendingNextTurnMessages` on `AgentSession` - the usage accounting, the budget-limit steer mint, the natural continuation mint with its slot rollback, the RLM deferral behind unsettled descendant work (mark/drop/take owed, the wake-up ownership probe, and the latest settled turn the post-settle deliveries decide from), and the terminal-error finish; the transports - the print driver's in-loop hook, the ACP settle loop, the daemon worker's queue - drive the one goal driver through these methods). `SessionEngine::expand_skill_submission` + `AgentSession`'s prompt-path expansion (the `/skill:<name>` submission seam, TS `_expandSkillCommand`: `skills::expand_skill_command` builds the `<skill ...>` block over the shared `pa_types::skill_blocks` parse, the accepted-turn row and the admitted user message agree on the expanded text, and the `skill_use_count` session counter counts at the admission). `interaction` (`InteractionHub` — `headless`/`with_surface`, `todos`, `open_question`, `answer`, `presence_changed`, `cancel_open_question` — plus the `InteractionSurface` trait the embedding implements and its `InteractionEvent`s), `web_search` (`WebSearchEngine::shared`/`search` over `SearchRequest` + `Credentials` -> `SearchReport`/`SearchError`, the `ProviderId`/`ProviderAccess`/`WebSearchProvider` catalog with `KEYED_CREDENTIALS`, and `WebSearchHost` registering the `web.search` handler with an optional `SearchObserver`), `session_engine::effort_switch::{MODEL_APPLY_EFFORT_REQUEST_TYPE, REPL_CAN_CHANGE_EFFORT_SETTING}` (the request the daemon worker answers with its thinking switch, and the setting that gates it), `SettingsManager::get_repl_can_change_effort`, `usage_host` (`branch_spend`, the active branch's billed usage, and `USAGE_QUOTAS_REQUEST_TYPE`, the request the daemon worker answers with its quota report). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent (one-way).

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
of 256 registered controllers and four deliveries bounds resident work. Admission
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
