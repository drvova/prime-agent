# Review rules

These rules mirror the repo standards in AGENTS.md - update both whenever
either changes.

## No legacy or migration scaffolding

- When a migration or refactor completes within a PR, its staging scaffolding must be deleted, not kept: no type aliases where two names point at one type, no re-exports "for compatibility" without a live external consumer, no migration-era vocabulary ("staged", "legacy", "new-style") surviving in names, comments, or test titles.
- Flag any parameter, option type, or constant that has exactly one legal value at every call site. That is ceremony, not configuration.
- Flag exported symbols, fields, and methods with zero non-test consumers. Speculative surface is worse than dead code in security-relevant modules: it implies invariants that do not exist.
- Flag predicates that are provably equivalent to an existing check, and call sites that check the same condition twice under two spellings.

## Comments

- Comments must be short, human-readable, and only where the code cannot say it. Reject comments that restate code, narrate control flow, or describe deleted mechanisms or history.
- Doc comments must describe current behavior, not the history of how it got there.

## Module organization

- One concern per module, named for what it owns. Reject new `utils.rs`/`common.rs` grab-bag modules.
- A new concern lands as a new module beside its siblings (for example `crates/pa-core/src/kernel/scheme/` or `crates/pa-core/src/cron/store/`), not as more lines in an already-large orchestration file; when a PR grows such a file, suggest splitting by responsibility.
- When code moves into a new module, its related tests and docs move with it.
- Prefer private modules with an explicitly exported crate API; internals are `pub(crate)`. New traits need doc comments explaining their role and how implementations are expected to behave.

## File size (advisory)

- Flag new or grown Rust files past ~2,000 lines (tests included) as advisory guidance: cite the growth and suggest splitting by responsibility. Size is ADVISORY per the operator's ruling - a soft cap that informs review and never blocks a merge; the repo's `codebase-health` PR check reports the same as warnings only.
- Generated files are exempt from the size rule (see Generated files).

## Dependency direction

- Workspace dependencies flow one way, from the composition root (`pa-cli`) toward lower crates. Flag cycles, reverse edges, or new edges that contradict the Crates table in AGENTS.md.
- `pa-types` is the only shared vocabulary crate: flag domain types duplicated into or re-exported from higher crates.
- A new dependency (workspace edge or third-party crate) is an architectural change: flag it unless the PR states why existing crates cannot host the functionality, commits the regenerated `Cargo.lock` in the same change, and keeps `make deny` clean.
- Flag cross-crate `pub use`/re-exports that widen the public API surface. `pa-tui` renders from wire types and must not link the session engine (its only workspace dependency is `pa-types`); `pa-cli` wires crates and contains no business logic.

## Sessions share one process

Sessions are hosted inside the daemon process, with their Scheme kernel in the same process (AGENTS.md "Reliability model" and "Surface contract").

- Flag session code that reads process state on a session's behalf: `std::env::var`/`var_os`/`vars`, `std::env::current_dir`, `Command::new`, or a `std::thread::spawn`/runtime builder that does not propagate `pa_types::session_scope`. One session's environment or working directory must never reach another.
- Flag a Steel compiler switch from the environment (`STEEL_DEBUG_AST`, `STEEL_INLINE`, `STEEL_INLINE_RECURSIVE`, `STEEL_MODULE_INLINE`, `STEEL_CLOSURE_LIFTING`, `STEEL_SEARCH_PATHS`) that can still reach an engine: `pin_scheme_home` removes them at process start, before any engine is built.
- Flag a new Scheme-callable host function that can block without observing the cell's cancellation, and any durable write reachable from an ended session lifetime that skips `session_scope::ensure_live`.
- Flag `.lock().unwrap()` on a lock shared between sessions: one session's panic must not poison the others.
- Until phase 8 two daemons share an agent dir (the supervisor and the opt-in runtime daemon, `pa_runtime::daemon`): flag a session the runtime daemon opens, changes or deletes without holding its session lease (`pa_core::session::lease`), a write to a session another process's lease holds, and a change that makes the supervisor's behavior depend on the runtime daemon's selection (`PRIME_AGENT_RUNTIME_DAEMON`, `"daemon": "runtime"`). On the runtime daemon's lanes, flag a cancel or abort that drops the steers the aborted turn took but had not read (they go back to the lane), an input queued without its `agent/<id>/accepted`, and a lane change that starts a task while the lane is parked. On the runtime daemon's supervision (`pa-runtime` `session/faults.rs`), flag a copy of the restart policy or revival rules instead of `pa_core::session::crash` / `pa_core::session::revival`, a crash that reruns the work it failed or skips the crash log, `agent/<id>/crashed` or the transcript's fault rows, a rebuild that builds the engine before the cut-off calls are answered, and a revival that opens a held session without its lease or repeats an interrupted turn's saved prompt. On the runtime daemon's keeper and boot reap, flag keeper records (owner, stop intent, death log) for the runtime daemon kept under the supervisor's socket instead of `daemon2.sock`, a copy of the restart policy or of the stop ladder instead of `pa_core::platform::process_stop`, and a reap that signals the lock's recorded holder before it is shown to hold that agent dir's lock file, run the product binary and record the same socket, or before its socket failed to answer a hello. On the runtime daemon's scheduled jobs (`pa-runtime` `daemon/schedules.rs`), flag a job fired, claimed or rescheduled outside pa-core's job store and `AgentCronScheduler` pass (a second catch-up rule or claim), a fire that runs while the daemon is not the selected one, a wake that opens a session without its lease, and a session stopped on purpose that a due job wakes. On the runtime daemon's families (`pa-runtime` `subagents/family.rs`), flag a copy of the RLM ledger's record grammar instead of `pa_core::session_engine::rlm_ledger`, a spawn admitted without its ledger edge, a reopened child given a new transcript instead of the one its spawn wrote, a subagent job woken as a session of its own, and an `agent_message.wait` outside `wait_for_arrival` (a second cycle walk or park).
- On the RPC mode (`pa_runtime::rpc`), flag a turn event written before the response of the prompt it follows (events are buffered while a prompt's response is pending), a `fork` that writes the source transcript or opens the fork without a lease of its own, a replacement that disposes the live kernel before its replacement is built, and a queued-input pump that delivers onto a replaced engine or while a compaction runs.
- Flag a session-transcript read or write that bypasses `pa_core::session::store::store()` (the SQLite store in `<agent dir>/agent.db` by default; `crates/pa-core/tests/transcript_store_guard.rs` is the guard), and any write to a JSONL session file the store imported: the original is the read-only rollback (AGENTS.md "Reliability model"). Flag a read or write of `scheduled-jobs.json`, `harness_state.json`, `reuse/ledger.json`, `event-bus.json` or `session-crashes.jsonl` that bypasses its store (`pa_core::agent_state`): the database holds them once it has imported or written them, and the files are never written again.
- Flag a Steel builtin that allocates by a count, and a path from a Scheme value to JSON, that does not pass `crates/pa-core/src/kernel/scheme/limits.rs` (`GUARDED`, `plain_data`): a count over the limit, a negative count (Steel reads it as unsigned) and a non-finite number are the cell's error, where Steel itself panics or allocates until the process dies.
- Flag a kernel Scheme source loaded with `engine.run` instead of `contracts::load` (`crates/pa-core/src/kernel/scheme/contracts.rs`), and a kernel function that reads an option it does not declare or silently ignores one: every kernel function refuses an undeclared keyword, options-hash key or argument before it runs.
- Flag a host request reached around the kernel's call policy (`pa_core::kernel::CallPolicy`: the kernel's handler registries are wrapped by `HostRequestHandlers::gated` as it is built), a side-effecting native whose dispatcher does not ask it at its entry, and a host request pa-core registers without its class in `crates/pa-runtime/src/trust/classes.rs`: an untrusted turn is admitted only to read the session's directory, its own context and todo list, and the kernel's reports, and never to the web.
- Flag a kernel function added, removed or changed without the matching edit in `crates/pa-core/src/prompts/capabilities/`, and any change to `crates/pa-core/src/kernel/scheme/confinement/open_globals.txt` that opens a Steel builtin without a stated reason. Flag a change that closes or guards a Steel builtin in the session's engine only: the engine Steel runs macro bodies in (`Engine::with_macro_engine`) is closed, given the cell's ports and interrupted with it too, and any change to `vendor/steel-core` missing from its `PRIME_AGENT_PATCH.md`. Flag a Steel object a cell can change that two engines share (Steel builds every engine from one image: `no_two_engines_share_an_object_a_cell_can_change` pins it), and a builtin that acts on the whole process (`#%module-add`, `#%gc-collect`, `command-line`) reopened.
- Flag anything that reintroduces a Python interpreter, venv, uv, or a kernel child process, and fault-injection seams reachable in release builds.
- In `crates/pa-shell` (the built-in shell, which runs inside the daemon): flag a utility or builtin that reads the process's working directory, environment or standard streams instead of its `Invocation`, opens a path argument without `View::path`/`View::file`, starts a process other than through `Children::spawn`, calls `std::process::exit`, or loops over input without `View::alive`. Flag a linked `uumain`-style entry point (uutils and the like): it does all of those. Flag a builtin that acts on the whole process (`exec`, `umask`, `ulimit`, signalling the process's own pid) reaching the shell unreplaced, and any change to `vendor/brush-core` or `vendor/jaq-json` missing from its `PRIME_AGENT_PATCH.md`. The built-in shell is what `bash` runs on by default on every platform: flag a utility added, removed or changed without the matching edit in `crates/pa-core/src/prompts/capabilities/shell.md`; a test or lifecycle check that takes `$$` or a job's `pid` for a process id, or watches a bare `sleep` as a process (it must start a program by its path); a new expansion or builtin that allocates by a number in the script without a bound beside `engine::LIMITS`; and, in `util/jq.rs`, JSON parsed without the `Nesting` depth check, a jaq function that reaches the process (environment, standard streams, time zone, exit) left in, or a filter run on the shell's own thread.

## Change hygiene

- One logical change per PR: flag PRs that mix a feature with unrelated lint fixes or refactors, and fixes that do not ride with the change that introduced them.
- If a change forces edits across many crate internals, the crate boundary is what needs fixing: flag that instead of the edits.
- Risky raw APIs route through the in-house wrappers, not new raw call sites: process control (signals/process groups), file locking, permissions, and shell selection go through `pa-core`'s `platform` wall; durable renames go through `rename_onto`; TS-parity hashing (tool-call id normalization) uses `pa-ai`'s in-house `short_hash`; environment-variable resolution goes through its owning module (telemetry overrides in `pa-telemetry`, provider API keys in `pa-ai`), not duplicated ad-hoc parsing. Flag a raw call site that bypasses these walls.

## Repo hygiene

- Flag scripts and tools that no CI workflow, Makefile target, or other code runs, such as one-off review, parity, evidence, or reproduction scripts (for example a 520-line TS-vs-Rust shutdown comparison script added in a feature PR). Nothing keeps them working; their results belong in the PR body, not in the repo.
- Flag test files that nothing runs. Before flagging, check the globs, loops, and paths built at run time in `.github/workflows/` and the Makefile (for example, ci.yml runs `scripts/test_check_<scenario>.py` through a path built inside its loop over `scripts/check_*.py`). A file name that appears nowhere can still run. Flag only when no runner, glob, or loop reaches the file; such a test only looks like coverage.
- Flag committed artifacts that no code or test reads: evidence logs, reports, generated outputs, and local venvs or caches.
- Flag a PR body that presents a script or run that CI does not run as lasting proof (for example "covered by `scripts/foo.py`"). One-off evidence belongs in the body, but it must read as a one-off run with its command and result, not as coverage.

## Tests

- A bug fix that changes meaningful behavior lands with a regression test that fails without the fix; flag tests that pass without it. Trivial or documentation-only changes need no test.
- Reject incidental or duplicate coverage, tests that re-check behavior the PR does not change, and copy/string snapshot assertions that merely restate labels or descriptions; assert text only when the text is the behavior.
- Flag test code far out of proportion to the product change (for example 2,300 test lines for ~400 product lines). Ask for one discriminating test per behavior change, not several tests that pin the same behavior.
- Tests must wait for observable readiness: flag fixed sleeps, polling loops, and retry-to-green wrappers used as readiness signals. A timeout may bound failure; it must not make the test pass.
- Flag skipped or disabled tests without a reason stated at the test; flag tests that should use isolated ports and temporary paths but do not, and tests that leave shared state unrestored.

## Lint discipline

- Zero-warning posture: the merge gates run clippy at `-D warnings`, so any new warning is a merge blocker. Flag new warnings and any suppression added to hide one.
- Every inline `#[allow(...)]` or lint exception states its reason in a comment next to it; flag unexplained or broad exceptions. Advisory ignores in `deny.toml` need a reason and a review date.
- New lint families land whole-site: enabling a family requires fixing every current site in the same change. Flag partial enablements.

## Generated files

- The model catalog is hand-maintained data: `crates/pa-ai/src/models_generated.rs` is hand-written Rust (no generator, no generated JSON — edit it by hand), and the catalog fixture `crates/pa-models/tests/fixtures/catalog.v1.json` refreshes with `scripts/generate-catalog-fixture.py` from the real `PrimeIntellect-ai/prime-agent-catalog` snapshot. Flag edits that would reintroduce a generator or hand-tweak the fixture rows.
- Bulk data files are exempt from the size guidance; the exemption's reason is that the file mirrors an upstream data source.
