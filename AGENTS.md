# AGENTS.md

Development rules for Prime Agent (Rust) on PrimeIntellect-ai/prime-agent, branch `main`.
Every contributor (human or agent) must read this before working on this repo.

## Repository

- **Never set git identity for commits to this repo's history.** Agents must not set
  `user.name`/`user.email` (config or `-c`) or `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
  `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` for commits that land in this repository.
  Those commits carry the identity of the ambient git config. If such a commit fails for a
  missing identity, report it instead of adding config. Allowed: the `*_DATE` variables
  (the golden corpus pins them for reproducibility), throwaway temp repos created by
  tests, and the release automation's bot identity.
- **Never add `Co-authored-by` trailers** — not in commit messages, not in squash
  suggestions, not as attribution for work merged from branches. A PR's commits carry
  one identity: the ambient git config's. Fabricated or hand-written trailers (including
  noreply forms) misattribute commits to unrelated GitHub accounts.
- The repo is PrimeIntellect-ai/prime-agent; the Rust implementation lives on the `main` branch.
- PRs go to the org repo with base `main`:
  `gh pr create --repo PrimeIntellect-ai/prime-agent --base main`.
- CI runs on the org's billing: `.github/workflows/continuous.yml` on `main` pushes and
  `release.yml` on version tags.

## Style and structure

- Workspace crates are prefixed `pa-`. The hard ownership rules: one owned area per crate,
  pa-types is the only shared crate, cycle-free dependency direction, minimal public APIs,
  no god-modules. The dependency direction is pinned in the Crates table below; each crate's
  README.md states its scope, non-goals, and public API surface.
- Prefer private modules with an explicitly exported public crate API. Internals are `pub(crate)`.
- Aim for files under 2,000 lines, tests included. The 2,000-2,100 range is a soft review
  signal, not a merge limit: when adding to an already-large file, consider splitting by
  responsibility rather than growing it further. Move related tests and docs with extracted code.
- Name modules for what they own, not `utils` or `common`. Follow existing structure: in
  `crates/pa-core/src/kernel/scheme/`, `mod.rs` connects `cell.rs`, `session.rs`, and
  `confinement.rs`; put a new kernel concern beside them. In
  `crates/pa-core/src/cron/store/`, `mod.rs` connects `heartbeat.rs`, `jobs.rs`, and
  `session_artifacts.rs`; keep storage work there rather than growing the scheduler.
- Inline format args: always prefer `format!("{x}")` over positional.
- Collapse if statements per clippy::collapsible_if.
- Prefer method references over closures per clippy::redundant_closure_for_method_calls.
- Make `match` statements exhaustive; avoid wildcard arms.
- New traits need doc comments explaining their role and how implementations are expected to behave.
- No opaque positional `bool`/`Option` parameters (`foo(false)` is unreadable). Prefer enums,
  named methods, or newtypes. If you must pass an opaque literal by position, use an exact
  `/*param_name*/` comment matching the callee signature.
- Prefer native RPITIT trait methods with explicit `Send` bounds
  (`fn foo(&self) -> impl Future<Output = T> + Send;`) over `#[async_trait]` or
  `#[allow(async_fn_in_trait)]`. Implementations may use `async fn` when they satisfy the contract.
- No single-use helper methods. Do not create a helper referenced only once.
- Instrument async work at the definition (`#[tracing::instrument(...)]`), not with
  `.instrument(...)` at call sites. Check whether the callee is already instrumented first.

## Change hygiene

- One logical change per PR. Fixes ride with the change that introduced them when possible; a PR
  that mixes a feature with unrelated lint fixes or refactors hides both from review.
- If you change dependencies (`Cargo.toml`), regenerate/commit `Cargo.lock` in the same change.
  A new dependency is an architectural change: state in the PR why the existing crates cannot
  host the functionality, and `make deny` must stay clean for it.
- If a change starts forcing edits across many crate internals, stop and fix the boundary instead.
- Do not commit scripts or tools that no workflow, Makefile target, or other code runs (one-off review, parity, or reproduction scripts). Put their results in the PR body as a one-off run with its command and result, not as coverage.
- Do not commit evidence logs, reports, generated outputs, or local venvs and caches that no code or test reads.

## Tests

- Prefer whole-object equality comparisons over field-by-field checks.
- Do not add tests for statically defined values.
- Do not add negative tests for logic that was removed.
- Bug fixes need a regression test that fails without the fix.
- Keep test code in proportion to the change: one discriminating test per behavior change, not several that pin the same behavior or re-check behavior the PR does not change.
- A new test file must be run by something: a runner, glob, loop, or run-time path in `.github/workflows/` or the Makefile. A name missing from those files can still be reached by a glob or a built path, so check those before calling a test unrun.
- Verifiers over self-assessment: tmux user-level tests, golden corpora replayed against real
  captured data. No lane merges without its verifier passing, rerun by the reviewer where
  feasible.
- Tests must wait for observable readiness, not fixed sleeps or retry-to-green loops. If a
  test must be disabled, give a reason at the test. Use isolated ports and temporary paths, and
  restore shared state. Run focused tests when changing their behavior.

## Lint discipline

- Zero-warning posture: the merge gates run fmt + clippy + tests at `-D warnings`, so a `warn`
  lint is a merge blocker. Keep the enabled set deliberate and selective; enable a new lint family
  only with every current site fixed in the same change.
- Explain lint exceptions next to the `#[allow(...)]` or workspace configuration; keep
  exceptions narrow. Advisory ignores in `deny.toml` need a reason and review date.

## Performance and the critical path

The UI critical path is paint-latency-critical. First paint, and every
frame the user waits on, must never block on network, disk sync, or
background bookkeeping. The launch benchmarks measure this surface
(cold/warm ready, first paint); a merge that regresses them undoes the
product's core claim.

Rules:

- Nothing user-visible awaits the network. Any HTTP/socket call on a
  render path (telemetry, catalog fetch, update check) runs on a
  background worker; the paint path hands the work off and returns
  immediately. Delivery semantics (timeout, retry, ordering) live with
  the worker, not the caller.
- Telemetry is fire-and-forget from user-facing code. `pa-telemetry`'s
  background worker owns delivery end to end; no startup, session
  create, tab open, or first-render path may hold a flush or await a
  sink.
- Fire-and-forget boundaries need a red-first test: a hanging sink
  (one that never replies) must not delay paint. The test fails if any
  await on the critical path reaches the worker's delivery.
- Tests that mock a sink or run offline cannot prove the above (the
  benchmark's mock hides the POST). The regression test with a hanging
  sink is the required verifier.
- A session never waits to build its kernel. Building a Scheme engine
  costs about 200 ms, so the daemon keeps spare engines built ahead of
  time and refills them in the background; engines are built
  independently, never copied (copies share interpreter state between
  sessions). `first-eval` and `warm-first-eval` in `crates/pa-perf`
  measure this.

Example (the #3288 regression this rule exists to prevent): the
telemetry startup flush was awaited on the interactive launch path, so
the analytics POST — a real HTTPS round-trip, ~157 ms — ran before the
first frame. Every CI check passed because tests set DO_NOT_TRACK=1 and
the benchmark mocked the provider offline; only real-network launches
paid the cost. The fix moved the flush to the background worker and
added `the_startup_flush_never_blocks_the_first_frame` (a hanging sink
must not delay paint) as the permanent guard. Any change touching the
launch path must keep that test red-first.


## Merge gates

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` must pass before every merge. Run `make check` — the local mirror of
  the same gates; CI runs on the org's billing (`.github/workflows/continuous.yml` on `main`
  pushes; `release.yml` on version tags). The PR-only codebase-health check reports growth
  past 2,000 lines as review guidance; it never blocks a merge.
- PRs that change shipped paths need a `.changes/<branch-slug>.md` fragment (one user-facing
  bullet; see `.changes/README.md`) or the `no-changelog` label; the changelog-fragment check enforces it.
- PRs must state ownership compliance (crate README scope/non-goals/public API, dependency
  direction) and classify the change: internals, new `pub` surface, or a new
  dependency/cross-crate re-export. The latter two are architectural changes — an innocent-looking
  `pub use` is a very simple way to break encapsulation.
- The compiled fallback model catalog `crates/pa-ai/src/models_generated.rs` is
  hand-maintained Rust — edit it by hand (there is no generator). The catalog fixture
  `crates/pa-models/tests/fixtures/catalog.v1.json` refreshes with
  `scripts/generate-catalog-fixture.py`. Bulk data files are exempt from the size guidance.

## Adoption telemetry

Every user-visible feature ships its adoption telemetry event in the same PR as the feature:
the event name + properties join the versioned schema (`pa-telemetry`), and a
seam emits it from day one. Telemetry properties never carry prompt, session, or file content
(primitives only; see `pa-telemetry`); delivery is fire-and-forget from user-facing code
(see Performance and the critical path).

## Branding

The product is Prime Agent - we are not a pi fork. Scrub "pi"/"pi-mono"/"pi-ai"/
"Prime Intellect"-style naming from all user-visible surfaces (docs, READMEs,
CLI help text, error messages, splash/onboarding strings, keybinding hints, TUI
labels, and code comments that quote user-facing strings); brand everything
Prime Agent. Audit with a repo-wide grep and classify every hit (user-visible
vs wire-internal vs comment) before scrubbing, and list the preserved wire
identifiers in the PR body so the reviewer can verify none were wrongly scrubbed.

EXPLICIT EXCEPTION: wire-protocol identifiers (e.g. the PI_PACKAGE_DIR env var, settings keys, provider IDs like prime-inference, harness _meta namespaces like ai.primeintellect.prime-agent, lockfile names) stay unchanged, because existing installs, settings files, and clients depend on them — COMPATIBILITY BEATS BRANDING ON THE WIRE.



## Surface contract (must not change)

- Tool exposed to the model: `eval`, a persistent Scheme kernel (Steel) that runs inside the agent process. Everything else the model does is a Scheme function called from an `eval` cell. Saved sessions that contain `python_repl` cells from before the kernel moved to Scheme still render and replay (`pa_types::ai::kernel_tool_language`).
- `bash` runs each command on the built-in shell (`crates/pa-shell`) on every platform: a bash-compatible interpreter that runs the script and its utilities (`ls`, `grep`, `sed`, `find`, `jq`, ...) inside the daemon and starts only the programs the script names. The reference in `crates/pa-core/src/prompts/capabilities/shell.md` tells the model what is built in and where the shell differs from bash; a utility added, removed or changed changes that reference in the same PR. A user asks for a shell process per command instead with `"shell": "system"` in global settings, or by setting `shellPath`, which names the program and so selects it unless `"shell": "builtin"` says otherwise (`crates/pa-core/src/platform/shell.rs`). Both shells give the same Scheme results; `crates/pa-core/src/kernel/scheme/native/bash_tests.rs` runs the same tests on each. A test that watches a process a command started starts a program by its path (`/bin/sleep 600 & echo $!`): under the built-in shell `$$` and a job's `pid` are job numbers and a bare `sleep` is the shell's own. Persistent shells, the user's `!` commands and a hypothesis check always run on a shell process.
- Kernel API, as Scheme functions: `bash` and its job functions, `fs-read/write/edit/grep/glob/find/symbol` and `edit`, `rlm-spawn/find-models/collect/list-subagents/delete-subagent/create-session/progress-note`, the `harness-*` store, `agent-message-*`, `agent-observe-*`, `event-*`, `compact-*`, `goal-*`, `refine-*`, `mcp-*`, `web-*`, `worktree-*`, `factory-*`, `rz-*`, `attach-image`, and skills (markdown, optionally with Scheme source) per the skill contract in the base system prompt. The reference in `crates/pa-core/src/prompts/capabilities/` is the authority, and `crates/pa-core/tests/prompt_guards.rs` fails when a kernel function and that reference disagree: change both in the same PR.
- A kernel function refuses a call it does not declare before it runs: an unknown keyword or options-hash key, a keyword given twice or without a value, an MCP tool argument its schema does not list, a wrong number of arguments. The accepted keywords are read from each `(define (name ...) ...)` as it is loaded (`crates/pa-core/src/kernel/scheme/contracts.rs`), so a new Scheme source is loaded through `contracts::load`, never `engine.run`; the guard in `contracts/tests.rs` calls every kernel function with an undeclared option and fails for one that runs. Only a function with a rest parameter is exempt.
- A kernel asks its embedder's call policy (`pa_core::kernel::CallPolicy`, `SessionEngineConfig::call_policy`) before every host request reaches its handler (the session's handlers and the kernel's own are wrapped as the kernel is built, so a request a native sends itself is asked too) and at the entry of every side-effecting native (`bash` and its job functions, the file writers, `fs-symbol`, `worktree-*`, `rlm-hypotheses`, `harness-*`, `factory-*`, `rz-*`, `mcp-*`, by `NativeFamily`); a refusal is the call's error, "refused for an untrusted turn: <call>". A new side-effecting native consults it at its entry. A runtime session's policy is pa-runtime's trust gate (`crates/pa-runtime/src/trust/`): an owner's task is admitted everywhere; an untrusted task only for `fs.read`/`fs.grep`/`fs.glob`/`fs.find` under the session's directory, `ctx.*`, `todo.*`, `rlm.end_turn` and the kernel's own reports (never `web.*`: a URL could carry out what it read), and its prompt reaches the model as `<untrusted-event source="...">...</untrusted-event>`. `crates/pa-runtime/src/trust/classes.rs` gives every host request pa-core registers exactly one class, and its guard fails for one it does not list.
- A cell reaches the system only through those functions. Steel's own file, process, loader, environment, thread and blocking builtins are closed (`crates/pa-core/src/kernel/scheme/confinement.rs`) in both engines a cell's code runs in: the session's own, and the one Steel's compiler keeps for macro bodies (`defmacro`, `syntax-case`, `begin-for-syntax`), which also prints to the cell's ports and is interrupted with the cell. Steel builds every engine from one image, so no engine keeps an object of it a cell can change (modules, the documentation and memo tables, the will executor): each gets its own, and `#%module-add`, `#%module-add-doc`, `#%gc-collect`, `%memo-table-set!` and `command-line` are closed. The names a cell can reach in either are pinned in `confinement/open_globals.txt`: a Steel upgrade or a new builtin that changes the list fails its test and needs a deliberate decision. `vendor/steel-core/PRIME_AGENT_PATCH.md` lists every change to the vendored interpreter.
- The product ships and installs no Python: no interpreter, venv, uv, or packages. The one place a Python is used is a factory machine's optional `python` state, which runs the `python3` already on `PATH` and fails that state when there is none. `--prime-agent-bootstrap` is accepted and does nothing, and a release archive keeps two placeholder files under `prime-agent-runtime/`, because installers and updaters already in the field require both (`docs/legacy-upgrade-contract.md`).
- System prompt structure: layered — cache-stable static layer files (core harness description that names every kernel function, mandatory usage rules, opinionated guidelines, per-model additions from the model-prompts TOML rule map) followed by one dynamic tail (project context, skills inventory, MCP servers, environment, session role); the harness digest stays a separate `[harness-digest]` user message. The full reference is not in the prompt: the model reads it with `(api-help [topic])`. `prime-agent prompt` dumps the assembled prompt with its layer breakdown.
- CLI shape: `prime-agent` keeps its existing commands and flags; headless modes (RPC/daemon) keep their behavior.

## Crates

One owned area per crate; `pa-types` is the only shared vocabulary crate.
Dependencies flow from the composition root toward lower crates, never back upward.
The table summarizes each crate's current direct workspace dependencies:

| Crate | Purpose | Direct workspace dependencies |
| --- | --- | --- |
| `pa-types` | Shared wire and domain types, per-session scope | none |
| `pa-telemetry` | Events and sinks | none |
| `pa-agent` | Provider-independent agent loop | none |
| `pa-shell` | Built-in bash-compatible shell and its utilities | `pa-types` |
| `pa-log` | SQLite store: the event log, session transcripts and the agent's other state (the transcript store's database) | `pa-types` |
| `pa-runtime` | Event-driven runtime: bus, subscriptions, task lanes and their SQLite stores, the sessions that run tasks as turns, the event sources its subscriptions start and stop, the headless turn driver, the RPC and ACP modes, and the runtime daemon (wire2 on its own socket, opt-in until phase 8) | `pa-types`, `pa-log`, `pa-core`, `pa-agent`, `pa-ai`, `pa-sources` |
| `pa-sources` | Event sources: timers, folder watches, webhooks; the schedule grammar and evaluator timers and scheduled jobs share | `pa-types` |
| `pa-ai` | Providers, model registry, streaming | `pa-types` |
| `pa-models` | Live model catalog and transport | `pa-ai`, `pa-types` |
| `pa-core` | Session engine, tools, skills, Scheme kernel, settings, transcript store, session leases | `pa-types`, `pa-ai`, `pa-models`, `pa-agent`, `pa-shell`, `pa-telemetry`, `pa-log`, `pa-sources` |
| `pa-daemon` | Supervisor, hosted sessions, restart owner, wire protocol | `pa-types`, `pa-core`, `pa-agent`, `pa-ai`, `pa-telemetry`, `pa-models` |
| `pa-tui` | Terminal UI, daemon-wire client | `pa-types` |
| `pa-cli` | `prime-agent` binary, composition root | `pa-types`, `pa-core`, `pa-ai`, `pa-agent`, `pa-daemon`, `pa-runtime`, `pa-tui`, `pa-telemetry` |
| `pa-perf` | Performance corpora + pty bench harness (dev/CI tooling, never shipped) | `pa-types`, `pa-core`, `pa-daemon` |

Layering rules:

- Dependencies only point from higher crates to lower crates shown above. No cycles or
  reverse edges. The leaf crates are peers; add only the dependencies needed for their work.
  Shared wire/domain vocabulary belongs in `pa-types`, not in a higher crate or a duplicate.
- `pa-tui` renders from wire types and events; it does not link the session engine.
- `pa-cli` is a composition root: it wires crates together and contains no business logic.

## Reliability model

Sessions run inside the daemon process: each on its own thread and runtime
(`crates/pa-daemon/src/session_host.rs`), with its kernel in the same process. There is no
worker process per session, so the isolation a process boundary gave is built from the rules
below. A change that weakens one of them needs the same scrutiny as a change to the wire.

- Per-session state is data, never process state. A session's launch environment, working
  directory, settings profile, admission priority and log go through
  `pa_types::session_scope`; session code never calls `std::env::var`, `std::env::current_dir`
  or `Command::new` directly, and a thread or runtime started on a session's behalf
  propagates the scope. Process-wide by design, and therefore not per session:
  `SSL_CERT_FILE`/`SSL_CERT_DIR`, `TMPDIR`, `STEEL_HOME`, the telemetry origin. The
  switches Steel's compiler reads from the environment (`STEEL_DEBUG_AST`, `STEEL_INLINE`,
  `STEEL_INLINE_RECURSIVE`, `STEEL_MODULE_INLINE`, `STEEL_CLOSURE_LIFTING`,
  `STEEL_SEARCH_PATHS`) are removed from the process at start (`pin_scheme_home`): no
  environment changes how a session's cells compile.
- Containment: a panic on a session's thread ends that session only. Every panic is either
  attributed to a session or recorded as a daemon-level crash; none goes to the default hook
  unrecorded. Locks shared between sessions tolerate poisoning.
- Visibility: a session crash is appended to the crash log (`session-crashes.jsonl`, kept in the
  agent database), stamped on the session
  (`lastCrash`), sent to attached clients (`session_crashed`, then `session_recovered` or
  `session_failed`), saved in the session file as a display-only row, and shown in the TUI.
- Self-heal: a crashed session is rebuilt from its transcript with a fresh kernel (250 ms
  backoff doubling to 30 s, give up after 5 failures in a row). The turn that was running
  fails with the real reason and is not run again, and the model is told once that its
  kernel definitions are gone.
- A stopped cell must leave the engine promptly: every Scheme-callable host function either
  returns at once or observes the cell's cancellation. A cell still on the engine after the
  grace is reported, its session is rebuilt, and nothing of the ended lifetime may write
  (`session_scope::ensure_live`). A new blocking host function without cancellation is a bug.
- The built-in shell is session code that runs other people's programs' worth of logic in the daemon, so the rules above bind it too (`crates/pa-shell/README.md`). A shell is data: its working directory and environment are the script's, seeded from the session scope, and nothing in `pa-shell` reads the process's working directory, environment or standard streams. A utility that cannot work that way is not linked in: a `uumain`-style entry point (uutils) reads all three and may exit the process, so such a command stays a program. A program is started by the embedder (`pa_shell::Children`), in its own process group or Windows job, and is killed when its job is stopped, killed, dropped, or its lifetime ends. The one exception is by Windows design: an elevated program the user consents to through UAC (`Start-Process -Verb RunAs`) is created by Windows in the job of the program that asked but runs with the user's administrator token, which the daemon may not end; ending the job spares it and the job then no longer ends it on close (`crates/pa-core/src/kernel/scheme/native/bash_winjob.rs`). A script sees a job number for `$$`, never the daemon's pid. A panic in a utility is that command's failure and a panic in the interpreter that script's; recursion (functions, `eval`, `.`) and nesting are bounded, and the shell's threads have large stacks, because a stack overflow there is an abort-class fault. A refused allocation is one too, so the expansions that grow without the script writing anything long are bounded (`pa_shell` `engine::LIMITS`: 1,000,000 words from a word's brace expansions, 32 MiB in one expanded word or assignment, 32 MiB of command-substitution output), each a failure of its command. `jq` runs on an interpreter that bounds nothing itself: JSON nested deeper than 10,000 levels is refused wherever it is parsed, a filter that is long, deeply nested or defines a recursive function goes to the `jq` program, and a filter runs on its own thread with a 1 GiB stack (`crates/pa-shell/src/util/jq.rs`). A program is listed in the orphan journal as it starts. Not bounded: how many variables and array elements a script keeps, what `read`, `mapfile` and `printf` build, the input a utility holds whole (`sort`, `tac`), the memory a `jq` filter takes, the depth of a value a `jq` filter builds one level at a time past what that stack holds (about two million), and the time a `jq` filter computes without reading or printing after its command was stopped.
- Limits a cell cannot exceed: source nesting, value depth across the host bridge, source
  size, and single-allocation counts (`crates/pa-core/src/kernel/scheme/limits.rs`). A count
  Steel would read as unsigned (a negative one) and a number JSON has no form for are the
  cell's error there too, where Steel itself panics. Cells run
  on their own large-stack thread. Not bounded: a cell's total memory, and CPU between
  sessions.
- Daemon death: an abort-class fault (stack overflow, out of memory, a fault in native code)
  ends the daemon and every session in it. A restart owner (`crates/pa-daemon/src/keeper.rs`,
  unix, for interactive, ACP and post-update launches) restarts the daemon with the same
  backoff and give-up limit and records every death; a deliberate stop is not restarted. It
  keeps whichever daemon the launch selects: the runtime daemon's keeper keeps its records
  under `daemon2.sock`, apart from the supervisor's. At
  boot a session that was mid-turn is revived, its interrupted turn continued or settled by
  what the transcript shows was saved, a command its kernel left running is reaped and
  reported to the model, and an idle session with an armed scheduled job is woken when the
  job is due. Attached clients reconnect on their own and show what happened.
- One session-event builder: the `session_event` payloads a daemon sends for a session's turns
  (wire forms, update routing, the `message_update` coalescer, the live `contextUsage`
  estimate, flush pacing) are built in `pa_core::session_engine::wire_events`, never copied into
  a daemon; the print modes' json stream (`session_events::agent_event_json`) is a separate
  surface. The runtime daemon's sessions (`DriverEvents::SessionEvents`) publish those payloads
  on `agent/<id>/stream` (`pa-runtime` `session/stream.rs`), each numbered with the session's
  own `meta.sequence` (the counter `session.snapshot` reports as `lastEventSequence`), and a
  crash is mirrored onto the stream as the supervisor sends it: `session_crashed`, a cut-off
  run's `turn_end {error}` and `agent_end`, then `session_recovered` or `session_failed`. The
  crash a daemon's death left is mirrored when its revived session opens, and a client that
  attaches later replays it from the stored fault topics (`wire2_compat` maps a replayed
  `agent/<id>/crashed|recovered|failed` to the same frame). The stream also carries the queue
  view the old worker sent, `session_action_update`, built from the lane and the rows waiting
  on the agent (`session/stream/actions.rs`), and frames a session command's task as a run
  (`agent_start` … `agent_end`); the turn driver's own projection is not sent there. pa-cli's
  `runtime_daemon_e2e` `goldens` replays pa-tui's five inbound goldens through `wire2_compat`
  and pins what still differs from the recording (`tests/fixtures/inbound_goldens/*.diff`),
  the replies to the session reads included.
- One display transcript: the rows the old worker's attach window and `get_messages` pages
  serve, where a page may be cut (never at a tool result or custom row, never between a call
  and its result) and the attach's image elision are `pa_core::session::display_transcript`,
  shared by pa-daemon's session store and the runtime daemon's old-shape reads
  (`session.snapshot {shape: "attach" | "state"}`, `transcript.page {shape: "messages"}`,
  `pa-runtime` `daemon/handlers/attach.rs`), which `wire2_compat` maps `attach`, `get_state`
  and `get_messages` to.
- The agents view's feeds are live only and display only: the runtime daemon's roster
  (`runtime/roster`, the old `roster_update` without its `type`; `runtime.roster` reads the
  entries) and agent traffic (`runtime/traffic`, the old `agent_traffic` entry;
  `runtime.traffic` reads each root's ring) are never stored, reach no model's context and are
  built off every turn's path (`pa-runtime` `daemon/handlers/feeds.rs`: one task hears the
  bus, another composes the due entries from the same summary the attach serves). The inbox
  reports a delivery's traffic once it landed, from the runtime daemon only.
- One saved-session catalog: the rows both daemons list, their fold and its warm cache and
  sidecar, the walk's pages and cursor, and a row's wire form are `pa_core::session::catalog`,
  and a session's own usage and its billing split are `pa_core::session::usage_scan`, read by
  the old daemon's `list_saved_sessions` and summaries and by the runtime daemon's
  `sessions.list {shape: "saved"}` and summaries. The runtime daemon streams a list's rows on
  the request's live-only `runtime/session_list/<request>` while it walks the store, never
  waiting for a slow reader, and its reply is the catalog (`pa-runtime`
  `daemon/handlers/saved.rs`); `wire2_compat` maps `list_saved_sessions` to it.
- One set of setting-switch rules: the queue mode and transport a switch persists, the scoped
  model list it accepts, and the model or thinking level a cycle lands on are
  `pa_core::settings::switches`, applied by the old worker (`pa-daemon` `setting_switches.rs`)
  and by the runtime daemon's `session.cycle_model`, `session.set_scoped_models`,
  `session.cycle_effort`, `session.set_service_tier`, `session.set_transport`,
  `session.set_steering_mode`, `session.set_follow_up_mode`, `session.set_auto_compaction` and
  `session.set_auto_retry` (`pa-runtime` `daemon/handlers/switches.rs`), which answer the old
  responses' `data` and publish the old `service_tier_changed` on the session's stream when the
  active tier moves; `wire2_compat` maps the TUI's commands to them. A switch of a runtime
  session retargets its next request: the embedder hands the slot its engine streams from to
  the runtime (`BuiltSession::provider_target`), and `session.set_model`, `session.cycle_model`
  and `session.set_service_tier` write the model, its request auth (through the session's
  credential pin; a model whose auth does not resolve is refused before anything moves) and the
  active tier there, as the old worker and the RPC mode do.
- One copy of what a client reads beside a session's turns: the command catalog
  (`ResourceReload::connection_commands`), the `/mcp` page (`McpManager::connections_view`), the
  reload's MCP half (`McpManager::reload_live_inputs`), the Anthropic warning's marker
  (`SessionManager::mark_anthropic_warning_shown`), the `/model` picker's catalog and its
  coalesced refresh (`pa_core::models::CatalogRefresh`) and the `/usage` page
  (`pa_core::usage_report`) are pa-core's; the old worker and supervisor and the runtime
  daemon's `session.commands`, `session.mcp_connections`, `session.model_catalog`, `usage`,
  `session.reload` and `session.mark_anthropic_warning_shown` (`pa-runtime`
  `daemon/handlers/info.rs`) serve them, and a changed catalog is the old
  `model_catalog_changed` broadcast or the runtime's live-only `runtime/models`.
- One set of tree-move rules: where a `navigate_tree` lands and the text it re-enters, the
  `/fork` selector's user messages and the `/copy` text (`pa_core::session::navigation`), the
  abandoned branch's summary run (`branch_summarization::run_branch_summary`, its entry carrying
  the serving model) and the export's tool renderer (`export_html::RegistryToolRenderer`) are
  pa-core's; the old worker and the runtime daemon's `session.fork_messages`,
  `session.last_assistant_text`, `session.navigate_tree`, `session.abort_branch_summary` and
  `session.export_html` (`pa-runtime` `daemon/handlers/tree.rs`) apply them. A runtime
  navigation holds the session's lane for the move and aborts a running turn first, rebuilds
  the context onto the new branch and publishes a changed goal as the old `goal_update`.
- One workspace checkpoint store (`pa_core::session::checkpoints::CheckpointStore`: a shadow git
  repository per workspace under `<agent dir>/checkpoints/`, one index and record log per
  transcript, the user's git config shut out). Both daemons record each prompt's turn
  (`record_turn`: the workspace before it under its first appended entry, as it settled under its
  new leaf) and rewind a `restoreFiles` navigation with it (`rewind`: the target's tree, paths
  changed outside the session left alone and named, the abandoned state recorded for the way
  back). The runtime's store belongs to a root session's lifetime on disk (`pa-runtime`
  `session/checkpoints.rs`; none for a subagent or a session in memory), and every git call runs
  on a blocking thread; a navigation restores the files after its move commits, before its lane
  hold ends.
- One set of whole-session replacement rules: where a `fork {entryId, position}` starts and the
  text it hands back, where an `import_jsonl` copies its file, and the working directory a
  replaced session runs in with its refusals and their `errorInfo`
  (`pa_core::session::replacement`); the old worker and the runtime daemon's
  `session.fork_entry` and `session.import_jsonl` (`pa-runtime` `daemon/handlers/replace.rs`)
  apply them. A runtime replacement holds the session's lane and aborts a running turn, holds
  the new transcript's lease before it lets the old one go, and goes on under the same id: the
  session's host ends its lifetime and builds the engine and a fresh kernel from the new
  transcript as a crash's rebuild does, without the crash (`Runtime::replace_session`), so its
  topics' subscribers, its stream and its tasks are kept. A wire2 error reply carries the
  refusal's `errorInfo` as its `info` (`ErrorWithInfo` in the chain). A replaced session's
  scheduled jobs are rebound onto its new transcript in place, as its rebuilt engine arms them:
  a disarm and re-arm would let the old arming's timer removal race the new timers.
- A compaction whose caller brings no abort signal runs under the session's own
  (`AgentSession::abort_compaction`), and a prompt's auto-retry wait under the session's retry
  abort (`AgentSession::abort_retry`, reset as each prompt starts); the runtime daemon's
  `session.abort_compaction` and `session.abort_retry` reach them. An aborted compaction is
  recorded as cancelled (`compaction_end` with `aborted: true`), never as failed. The runtime
  daemon's queue edits (`session.mutate_queued_message`, `pa-runtime` `runtime/queue_edit.rs`)
  change only the lane's tasks, addressed by the queue view's row and the text the client saw;
  a row waiting on the agent is the running turn's and is rejected.
- Two daemons until phase 8: `prime-agent --mode daemon` is the supervisor (`pa-daemon`, the
  old wire on `daemon.sock`) unless `PRIME_AGENT_RUNTIME_DAEMON=1` or the global setting
  `"daemon": "runtime"` selects the runtime daemon and its launcher did not ask for the
  supervisor (`PRIME_AGENT_DAEMON_SUPERVISOR=1`, set by the interactive TUI's spawn and the
  update coordinator's successor, which speak the old wire until the TUI moves to wire2 in 8e;
  the keeper keeps the supervisor for them) (`pa_runtime::daemon`, wire2 on
  `daemon2.sock` beside it, one per agent dir by an OS lock on `runtime-daemon.lock`, whose
  recorded holder and restart owner the supervisor's boot reap spares though their command lines
  name the same socket; a holder that does not answer on the same socket is a wedged
  predecessor, stopped once shown to be a runtime daemon holding that lock, SIGTERM then
  SIGKILL, before the new one takes the lock). Both
  keep sessions in the same transcript store; a session is owned by whichever process holds its
  session lease (`pa_core::session::lease`), so the two never write one session. The runtime
  daemon runs interactive turns on its task lanes (steers, follow-ups, `session.abort` parking the
  lane with the unread steers back at its front, `abort_and_send_queued`, withdraw and restore,
  `session.compact` in the lane, and an `agent/<id>/accepted` per input naming its task); a
  parked lane does not survive its restart. Its sessions have the supervisor's recovery
  (`crates/pa-runtime/src/session/faults.rs`, with the policy and the revival rules shared from
  `pa_core::session::crash` and `pa_core::session::revival`): the process panic hook attributes
  a panic on a session's threads to that session, and a stuck cell ends its lifetime the same
  way; the work running fails with the crash's reason and is not rerun; the crash is appended to
  `<agent dir>/runtime-daemon/session-crashes.jsonl` (the database's `crashes` table), stored as
  `agent/<id>/crashed`, and kept in the transcript as display-only rows (`session.snapshot`'s
  `lastCrash`); the programs the session's commands left running are reaped from its orphan
  journal; the session is rebuilt from its transcript with the same backoff, give-up
  (`agent/<id>/recovered`, or `/failed` and the session closed) and one-time kernel-reset
  notice, the calls the crash cut off answered once as of unknown outcome. It keeps the
  sessions it holds in the database, and a runtime daemon that starts after one that died
  revives those with unfinished tasks: their orphans reaped, the death recorded as a
  `daemon_restart` crash, the interrupted task run again, continued behind the
  interrupted-turn notice (as the same task, its prompt not repeated) or left over, or, on the
  second death in a row under it, settled as failed. Its sessions' kernels serve `schedule.*` and
  `rlm_heartbeat.*` on the same stored jobs as the supervisor's (`pa_core::cron`), fired by the
  runtime's kept timer sources (`crates/pa-runtime/src/daemon/schedules.rs`) under the store's
  claim and catch-up rule, and only while it is the selected daemon (the settings are read again
  before each job; a fire while it is not is a skip), so a job never fires in both daemons. An
  idle session it held when it died is held dormant and woken under its lease when a job of its
  is due; a session stopped on purpose is not. Its sessions' subagents are runtime sessions
  recorded in the supervisor's RLM spawn ledger and display files
  (`pa_core::session_engine::rlm_ledger`, written by `crates/pa-runtime/src/subagents/family.rs`
  at spawn, settle, rename and delete; a spawn whose edge is not written is refused), with RLM
  heartbeats of their own; a parent it opens again (created, revived or woken) reopens the live
  children the ledger records under its transcript, each on its own transcript, as settled
  children. `agent_message.wait` parks on pa-core's receiving side
  (`pa_core::session_engine::agent_messaging::wait_for_arrival`, the supervisor's too) and refuses
  a cycle from the wait edges of every session it runs; `agent_observe.*` reads the family it
  runs. A row delivered to one of its root sessions (a settled child's notice, an agent
  message within the sender's wake budget, a steered kernel notice) wakes the session as the
  supervisor's notice turn does: a `Steer` empty input that a turn still reading steering takes,
  and that otherwise runs as its own task over the session's own rows, not waiting for its
  children; a `Record` notice only persists and a `WakeIdle` one is dropped by a busy session
  (`pa_core::session_engine::bash_notices`). `session.wait_idle` waits for those notice turns
  too. Its kernel timer and watch sources are kept in the database and run again after a
  restart for the subscriptions the bus keeps. A client opening a session it holds open
  already (one its boot revived) is answered with that session, as the supervisor's create-open
  reuse does, and a change or state read right after `session.create` waits for the engine's
  first build. The daemon-backed CLI commands (`list`, `sessions`, `stop`, `rename`, `send`,
  `schedule`) speak wire2 to it while it is selected (`crates/pa-cli/src/daemon_command.rs`:
  `sessions.list` rows in the supervisor's summary shape, `session.send` delivering the
  supervisor's `agent_message` row and waking a saved session, `schedules`/`schedule.add`/
  `schedule.cancel` on the same job store). For the ACP client it answers what that client asks
  of the supervisor (phase 7g-2): `session.create` takes the client's launch `options` (model,
  thinking level, system prompt, resources, autonomous run, goal, execution mode), kept in the
  database so the session's revival or wake builds it the same way; `session.set_model`
  switches a running session within the settings' `allowedModels` (pa-core's
  `load_model_allowlist`/`assert_model_allowed`, the old daemon's gate too: unreadable settings or a
  malformed `allowedModels` refuse every model); `session.set_mcp_servers` admits one client's MCP servers, fenced by its owner id;
  a scheduled job made or cancelled publishes `agent/<id>/schedules`; and the `models`,
  `settings`, `sources` and `subscriptions` queries answer. A root session stopped and opened
  again waits for its new engine's build. The user's `!` commands run on pa-core's user bash
  (`pa_core::user_bash`: one command at a time per session, the run, its `bash_*` events and its
  `bashExecution` row), which both daemons share: the supervisor's worker answers `execute_bash`,
  `execute_bash_and_wait` and `abort_bash` with it, and the runtime daemon `session.bash` (its
  `bash_start`/`bash_output`/`bash_end` on `agent/<id>/stream`, its row saved and joined to the
  model's context, or steered into a running turn) and `session.abort_bash`; stopping a session
  or the daemon kills its running command first and admits none while it stops (phase 7g-3).
  The rest of what the ACP client asks of the supervisor it answers too (phase 7g-5):
  `session.pause_input`/`session.release_input` hold a session's input for the calling
  connection (nothing queued starts; resuming a parked lane leaves it held), released by the
  connection's end and by the session's stop, so no departed client keeps a session from
  running; `session.cancel_task` cancels one prompt (queued: taken out; running: its turn
  aborted); `session.wait_idle` with `"subagents": false` waits for the session's own run
  (`wait_for_headless_completion`) and answers the autonomous accounting and the live
  background jobs; `session.model` answers the model, level and supported levels
  (`get_connection_state`), and `session.set_effort` clamps the level to the model, records
  the row only on a change and lets the settings' default follow, the old daemon's
  `set_thinking_level`; `session.children`/`session.cancel_child` answer the subagent roster
  (`get_rlm_children`/`cancel_rlm_child`). The TUI still speaks only to the supervisor.
  The ACP mode lives in pa-runtime (`crates/pa-runtime/src/acp/`, phase 7g-4); it links no
  pa-daemon code and sends its commands through a command-level transport
  (`acp/transport.rs`, `DaemonTransport`). By default that is the supervisor's socket on the old
  wire (`DaemonLink`); with the runtime daemon selected, pa-cli spawns or awaits `daemon2.sock`
  and ACP drives it on wire2 (`acp/wire2_link.rs`, `Wire2Link`, phase 7g-6), each old command
  as the wire2 frames that do the same (a prompt is a publish answered by its task's
  `agent/<id>/done`). `--no-session` there is the old daemon's client-owned in-memory session:
  `session.create {"ephemeral": true}` (`daemon/handlers/owned.rs`) keeps no transcript file, no
  options row, no lease and no held-session record (nothing to revive), and ends on
  `session.stop` (ACP's teardown) or 30 s after its owner's last connection ends, as the old
  daemon's owned worker does; the owner is the creating connection's `hello` name (`acp:<pid>`).
  Nothing of an ephemeral session reaches `agent.db` either (phase 7g-6b): the runtime's stores
  keep no event, task or state of it or of the subagents it spawns (`log_stores.rs`
  `Unstored`); its events still run their routes and reach live listeners, with `seq` 0 as a
  live-only event has. A subagent's status changes reach its parent's `agent/<id>/stream` as
  the old worker's `rlm_child_update` (its roster row, once per status), a cancelled child's
  roster status is `cancelled`, each runtime-daemon session answers a faux script from a queue
  of its own (`faux-<session id>`), its subagents run `PRIME_AGENT_FAUX_CHILD_SCRIPT` when the
  script names none for them, and `session.system_prompt` answers an open session's system
  prompt (the old `get_system_prompt`).
- Sessions persist on disk in the transcript store (`pa_core::session::store`), so every
  recovery above starts from the transcript. The default store is one SQLite database,
  `<agent dir>/agent.db` (`pa-log`): each transcript is kept byte for byte as its JSONL lines,
  addressed by its `<sessions dir>/<id>.jsonl` locator. It runs WAL with `synchronous=NORMAL`:
  a write returns once committed, which survives a crash of the process; a power loss can drop
  the last commits but never corrupts the database. JSONL session files are imported (in the
  background, or whole on their first write) and left in place, read-only, as the rollback;
  `prime-agent export-jsonl` writes the JSONL format back (`docs/legacy-upgrade-contract.md`,
  "Storage migration"). The global setting `"transcriptStore": "file"` keeps one JSONL file per
  session for one release. The agent's other state lives in the same database
  (`pa_core::agent_state`): schedules and heartbeats (`scheduled-jobs.json`), the harness
  stores, the reuse ledger, the event bus and the crash log, each still addressed by its file's
  path. A store reads its file until the database holds it; its first write, or the daemon's
  background import, brings it in whole, and the file is never written again. With the file
  transcript store they stay files.
- Fault-injection seams for tests (`panicOn`, `(%test-block marker)`, the export hold) exist
  in debug builds only.
- No stubs, no `todo!()`, no swallowed errors (`anyhow` bubbling to UI is fine).

## References

- Cursor Bugbot's PR review rules (`.cursor/BUGBOT.md`) mirror the standards in this file - update both whenever either changes.
