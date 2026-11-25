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
| `pa-sources` | Event sources: timers, folder watches, webhooks | `pa-types` |
| `pa-ai` | Providers, model registry, streaming | `pa-types` |
| `pa-models` | Live model catalog and transport | `pa-ai`, `pa-types` |
| `pa-core` | Session engine, tools, skills, Scheme kernel, settings | `pa-types`, `pa-ai`, `pa-models`, `pa-agent`, `pa-shell`, `pa-telemetry` |
| `pa-daemon` | Supervisor, hosted sessions, restart owner, wire protocol | `pa-types`, `pa-core`, `pa-agent`, `pa-ai`, `pa-telemetry`, `pa-models` |
| `pa-tui` | Terminal UI, daemon-wire client | `pa-types` |
| `pa-cli` | `prime-agent` binary, composition root | `pa-types`, `pa-core`, `pa-ai`, `pa-agent`, `pa-daemon`, `pa-tui`, `pa-telemetry` |
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
- Visibility: a session crash is appended to `session-crashes.jsonl`, stamped on the session
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
  backoff and give-up limit and records every death; a deliberate stop is not restarted. At
  boot a session that was mid-turn is revived, its interrupted turn continued or settled by
  what the transcript shows was saved, a command its kernel left running is reaped and
  reported to the model, and an idle session with an armed scheduled job is woken when the
  job is due. Attached clients reconnect on their own and show what happened.
- Sessions persist on disk (append-only JSONL, same layout as `~/.prime/agent/sessions`), so
  every recovery above starts from the transcript.
- Fault-injection seams for tests (`panicOn`, `(%test-block marker)`, the export hold) exist
  in debug builds only.
- No stubs, no `todo!()`, no swallowed errors (`anyhow` bubbling to UI is fine).

## References

- Cursor Bugbot's PR review rules (`.cursor/BUGBOT.md`) mirror the standards in this file - update both whenever either changes.
