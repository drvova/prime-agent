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
Platform wall (`platform`): process control (signals/process groups, kernel exit waits), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations (the durable-write rename primitive is pa-telemetry's `rename_onto`, re-exported as `platform::rename_onto`). Scheduled jobs (`cron`, the `AgentCronJobStore` port of `core/cron-jobs.ts`): file-backed job state under session artifacts (`scheduled-jobs.json` partitions) with cross-process locking, plus the public read-only scan (`cron::store::read_scheduled_jobs_artifact`) the update flow's roster projection and boot re-arm read. Tools (python_repl + internal rename/stdout), rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). origin/main

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
and a last line of its text pointing at `(ctx-page handle)`), never their text: its test files, the
workspace files it imports (Rust `mod`, Python imports, JS/TS relative imports, C `#include "…"`),
and the files calling what it defines (from the symbol index, once that is built). A decaying
per-project share of handles the model went on to use (a `ctx-*` read of the handle, or an
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
thinning (`session_engine::context_thinning`) pages each thinned result out the same way
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
`ctx-*` reads that followed a preview, ride the `agent previews` event. Non-goals: encoding, any
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
`(api-help ...)`. `tests/prompt_guards.rs` holds the reference equal to the functions the kernel's
sources define. Public surface: `API_DOCS`, `documented_calls`, `CORE_LAYER`.

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
captured, an abort or the per-call timeout interrupts them, and `(host-request type payload)`
reaches the same `HostRequestHandlers` the Python kernel's host requests do. It implements the
tool seam's executor and provisioner traits, and every session exposes it as the `eval` tool
(`tools::scheme`) beside `python_repl`.

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No update coordination (the pa-cli coordinator owns the FSM; the update-flow seam here is the daemon-free support layer: `update::version` (semver/channel policy), `update::install` (the managed install-root layout), `update::release` (the channel manifest fetch), `update::download` (sha256-verified archive download + staging), `update::installer` (the installer-takeover funnel plus `BUILD_DISTRIBUTION`, the compile-time `PRIME_AGENT_DIST_*` stamp that points a distribution build's updates at its own releases and prefix and names its own command; unstamped builds are `Distribution::Official`)). The package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `platform::process::{split_process, wait_child, parent_pid, Split, ChildExit}` (the process split and child wait the daemon's restart owner is built on; `split_process` requires a single-threaded caller), `export_html` (the HTML exporter plus its renderer seam: `tools_section`, `pre_render_custom_tools` + `ToolHtmlRenderer`/`RenderedToolHtml`/`RenderedToolResult`, and `ansi_to_html::ansi_lines_to_html`/`tool_render::trim_rendered_result_lines` for renderer implementers), `ToolRegistry`, kernel manager, settings (`SettingsManager`; `settings::Profile` + `load_profile`/`save_profile`/`list_profiles`/`profile_path`/`validate_profile_name` and the process-wide `set_active_profile`/`active_profile`: the `profiles/<name>.json` overlays, layered global < profile < project, with `PROFILE_ENV_VAR`/`PROFILES_DIR`/`DEFAULT_CREDENTIAL`; `settings::import_official_config` + `OfficialConfigImport`: a distribution build's one-time first-run copy of the official agent dir's configuration - never its sessions or runtime state - into its own agent dir, staged and renamed into place; `settings::plan_official_history_import` + `HistoryImportPlan`/`HistoryImportCheck`/`HistoryImportReport`/`HistoryImportRun`, `skip_official_history_import`, and `HISTORY_IMPORT_MARKER`: the one-time background copy of the official agent dir's chats, session artifacts, and harness memory into a distribution build's own dir — copy only (a clone where the filesystem can, never a link), merged without overwriting, per-file staged and published, the marker written last — plus the configuration entries the dir still lacks; `session_engine::telemetry::track_sessions_imported` reports it as `agent sessions imported`), packages (`packages::PackageManager` + source types), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children and by pa-cli's print mode for in-process children; the default is no children; the trait crate also owns the shared spawn/depth facts — `DEFAULT_RLM_MAX_DEPTH`, the depth-limit and name errors, the daemonless `create_session` refusal, `rlm_child_session_dir`, and the `persisted_rlm_max_depth`/`resolve_rlm_max_depth` precedence chain), `session_engine::semantic_edges` (the TS `semantic-edges.ts` WRITER: `SemanticEdgeRecorder`/`SemanticEdgeIdentity`, `semantic_edge_ledger_path` + `last_committed_request_id`, and the `X-ACP-Model-Request-ID`/`Idempotency-Key` constants — one durable per-session ledger of model-request events, byte-compatible with the TS lines; the fold and any publishing stay downstream), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through), `mcp::McpManager::connection_roster` + `mcp::McpConnectionEntry` (the `/mcp` view's roster: every resolved integration with its connected state, display kind, transport, and generic-listable flag), `mcp::McpManager::service_catalog_views` + `service_catalog_diagnostics` (the `/mcp` view's resolved service-catalog cards and their diagnostics, answered from local state - the TS `buildServiceCatalogViews` parity; the picker never round-trips the kernel), `mcp::McpManager::api_key_credential_views` + `mcp::API_KEY_CREDENTIALS`/`McpCredentialView` (the `/mcp` view's api-key credential rows - the stored keys the surface manages alongside the connections, e.g. the web-search key - configured from the shared auth store), `models` (the registry and resolver, plus `models::order_for_picker`: the picker-catalog order the composition root applies to its snapshot), `session_engine::provider_adapter` (`switchable_stream_fn`/`ProviderTarget`: the live provider-target slot a host swaps on `set_model`; the adapter bridges the loop's abort into pa-ai's `StreamOptions::signal` cancellation token, so a turn abort cancels the in-flight HTTP fetch at the transport — closed by `ModelStream::close` and the stream's drop; `switchable_stream_fn_with_live_auth` + `LiveAuthResolver` (returning `models::ResolvedRequestAuth`): the same slot plus a per-provider-call request-auth resolution, the TS streamFn's `getApiKeyAndHeaders` — the sync resolver runs off the executor on a blocking thread raced with the turn's abort signal; the resolution is the call's whole request auth (its key, headers, and the per-call request model — the xAI shape, TS `requestModel` — or the configured `models.json` fallback key when no credential serves), a failed resolution fails the call with its auth error, and a captured pair never resends), `session_engine::ipython_state` (the post-compaction `ipython_state` kernel-persistence notice: the `CompactionKernelProbe` view of the kernel provisioner the engine wiring binds via `AgentSession::set_kernel_state_probe`, and the notice row that rides `CompactRun` for the surfaces to broadcast). `session_engine::better` (`BetterHost` on `SessionEngine::better` with `BetterRequest`/`BetterMode`/`BetterOutcome`: the `/better` read-only prompt-rewrite helper - a side run on the session's model, prompt, and tool definitions whose tools only read, see `docs/better.md`). `session_engine::session_commands` (the `/compact`/`/refine`/`/goal`/`/autonomous`/`/better` executor: the durable echo/result rows with their persistence order - the echo ahead of the command so its own work sees it - the compaction/refinement/goal/autonomous arms, and the post-execution live-context rebuild that keeps the rows in the agent state; hosts own the wire surface), `session_engine::auto_refine_trigger` (the compact-trigger auto-refine machine on `AgentSession`: the arm/discard/increment accessors and `consume_compact_auto_refine`, the shared gate/review/stamp sequence every scheduling surface applies - TS `_compactAutoRefinePending`/`_lastAutoRefineReviewAt`/`_assistantTurnsSinceAutoRefine` plus `_maybeAutoRefine`'s compact arm, the serialized checkpoint's compact step, and dispose's serialized drain, with the surface parameter carrying the one behavioral difference between a boundary and a disposal). `session_engine::goal_boundary` (the goal arms of the natural turn end on `SessionEngine`: the `--goal` construction seed with its next-turn context row - TS `_pendingNextTurnMessages` on `AgentSession` - the usage accounting, the budget-limit steer mint, the natural continuation mint with its slot rollback, the RLM deferral behind unsettled descendant work (mark/drop/take owed, the wake-up ownership probe, and the latest settled turn the post-settle deliveries decide from), and the terminal-error finish; the transports - the print driver's in-loop hook, the ACP settle loop, the daemon worker's queue - drive the one goal driver through these methods). `SessionEngine::expand_skill_submission` + `AgentSession`'s prompt-path expansion (the `/skill:<name>` submission seam, TS `_expandSkillCommand`: `skills::expand_skill_command` builds the `<skill ...>` block over the shared `pa_types::skill_blocks` parse, the accepted-turn row and the admitted user message agree on the expanded text, and the `skill_use_count` session counter counts at the admission). `interaction` (`InteractionHub` — `headless`/`with_surface`, `todos`, `open_question`, `answer`, `presence_changed`, `cancel_open_question` — plus the `InteractionSurface` trait the embedding implements and its `InteractionEvent`s), `web_search` (`WebSearchEngine::shared`/`search` over `SearchRequest` + `Credentials` -> `SearchReport`/`SearchError`, the `ProviderId`/`ProviderAccess`/`WebSearchProvider` catalog with `KEYED_CREDENTIALS`, and `WebSearchHost` registering the `web.search` handler with an optional `SearchObserver`), `session_engine::effort_switch::{MODEL_APPLY_EFFORT_REQUEST_TYPE, REPL_CAN_CHANGE_EFFORT_SETTING}` (the request the daemon worker answers with its thinking switch, and the setting that gates it), `SettingsManager::get_repl_can_change_effort`, `SettingsManager::get_favorite_models`/`set_favorite_models` (the `/model` picker's `favoriteModels` list), `session_engine::resource_reload::{ResourceReload, ReloadReport}` (`SessionEngine::resource_reload`: the current `skills`/`prompt_templates`/`base_prompt` and `reload`), `session_engine::settings_view::SETTINGS_HOST_REQUEST_TYPE` (the request the daemon worker answers with its depth bound and autonomous status), `usage_host` (`branch_spend`, the active branch's billed usage, and `USAGE_QUOTAS_REQUEST_TYPE`, the request the daemon worker answers with its quota report), `session_engine::billing::BillingObserver` + `AgentSession::arm_billing_stamp` (the billing-mode stamp: each recorded assistant message's `usage.billing`, classified by pa-ai from the serving model, the provider credential's kind - stored sign-in, then environment key, never the secret - and the recorded cost; the engine wiring arms it and counts each mode into `agent billing modes`), `session_engine::billing::catalog_billing_mode` (the same classifier over a catalog model and the auth store's document with no recorded cost: the daemon's per-model `billing` map in the `get_model_catalog` response). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only). The Scheme kernel: `kernel::SchemeKernelProvisioner`, `kernel::SchemeKernel`, `kernel::prewarm_scheme_kernel`.

## Depends on
pa-types, pa-ai, pa-agent, pa-shell (one-way).

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
