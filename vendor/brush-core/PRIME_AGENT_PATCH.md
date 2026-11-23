# brush-core 0.5.0 embedding patch

Pinned source of crates.io `brush-core` 0.5.0 (MIT), the bash-compatible
interpreter behind Prime Agent's built-in shell (`crates/pa-shell`).

Upstream is written to be the one shell of its process. Prime Agent runs
many shells in one process, one per `bash` command of every session the
daemon hosts, so everything upstream does to or through the process is given
to the embedder instead. Every change is marked `Prime Agent patch` in the
source.

## What an embedder is given (`src/host.rs`)

- `host::Host`, set with the builder's `host(...)` and read with
  `Shell::host()`. A shell with a host:
  - starts no process itself: `commands::execute_external_command` hands the
    composed `std::process::Command` to `Host::spawn`;
  - can be stopped from outside: `interp`'s `Pipeline::execute` fails with
    `ErrorKind::Interrupted` once `Host::interrupted` is true, which ends a
    script that never leaves the interpreter (`while :; do :; done`);
  - reports `Host::process_id` as `$$`, `BASHPID` and `PPID`
    (`expansion.rs`, `wellknownvars.rs`). Upstream gives the process's own
    id, and `kill $$` run by any program would end every shell in it;
  - answers `/dev/stdin`, `/dev/stdout` and `/dev/stderr` from its own
    descriptors and refuses `/dev/tty` (`shell/fs.rs`, `open_file`).
    Upstream opens the paths, which name the process's streams.

## What upstream does to the whole process

- `processes.rs`, `ChildProcess::wait`: upstream listens for SIGTSTP,
  SIGCHLD and SIGINT while it waits for a child. That installs process-wide
  handlers for all three for good and polls every child of the process for
  a stop. The patched wait only awaits its child; a child is never reported
  as stopped.

## Deadlocks and crashes of an interpreter that blocks on its own pipes

The interpreter reads and writes pipes with blocking calls on the threads
of its tokio runtime.

- `interp.rs`, `ExecuteInPipeline for ast::Command`: upstream runs a
  compound command that is a pipeline stage to its end before the next stage
  exists, so `for ...; done | reader` blocks for good once the loop has
  written a pipe's capacity (64 KiB on Linux, 4 KiB on Windows). A stage
  with a shell of its own now runs beside the others.
- `host::spawn_beside` replaces `tokio::spawn` for pipeline stages, command
  substitutions, process substitutions, coprocesses and background lists
  (`interp.rs`, `commands.rs`). A task spawned from a worker thread waits in
  that worker's own queue; when the worker then blocks reading what the task
  is to write, neither runs (`x=$(cat <(echo a))`). Each such task gets a
  blocking thread of the runtime instead.
- `shell/execution.rs`, `run_program`: programs run inside programs (`eval`,
  `.`, trap handlers) count against `max_function_call_depth`. Upstream
  counts function calls only, so `x='eval "$x"'; eval "$x"` recurses until
  the stack overflows, which aborts the process.

## What one expansion may cost

Upstream expands whatever a script asks for, and an allocation the system
refuses ends the process, not the script. `host::Limits`, answered by
`Host::limits`, bounds the three expansions that grow without the script
writing anything long; a command past one fails with
`ErrorKind::LimitExceeded`, which ends the line it is on like any failed
expansion.

- `expansion.rs`, `brace_expand_if_needed`: the words of a word's brace
  expansions are counted before any is built
  (`braceexpansion::expansion_count`) and refused past `brace_words`, or
  when that many words of the word's length would pass `word_bytes`.
- `expansion.rs`, `basic_expand`: a word that expands to more than
  `word_bytes` is refused (`x=$x$x` in a loop).
- `commands.rs`, `invoke_command_in_subshell_and_get_output` and
  `sys/*/async_pipe.rs`: a command substitution keeps at most
  `substitution_bytes` of output. The rest is read and dropped, so the
  command runs to its end as it would have, and the substitution then
  fails.

Not bounded by these: the elements of an array and the variables of a
shell, what `read`, `mapfile` and `printf -v` put in a variable, and
`printf`'s field widths (brush-builtins).

## Background jobs

- `interp.rs`, `spawn_async_ao_list_in_task`: upstream runs every background
  list as one task, so a job has no process and `$!` is always empty. A list
  that is a single program, or any pipeline of several stages, is started
  directly: the job is its processes, last stage first, and `$!` is a real
  pid. A list that runs inside the shell (a loop, a group, a builtin, a
  function) is still a task without one.
- `jobs.rs`, `JobManager::resolve_job_spec` also resolves the pid of one of
  a job's processes, for `kill` and `wait` (pa-shell's own, in `shims.rs`).

## Dependencies

- `sys/unix/input.rs` no longer reads key codes through the `terminfo`
  crate, whose licence (WTFPL) this workspace does not ship; the mapping is
  the one upstream uses on other platforms. The dependency is gone from
  `Cargo.toml`.

`Cargo.toml` is the crate's published manifest without `terminfo`, its
`[[example]]` entries, its dev-dependencies and its `[lints]` table (a path
dependency is not lint-capped, and upstream denies warnings); `examples/`
and the crate's own `Cargo.lock` are not vendored. The host types use
`tokio::process::Child`, so the patched crate no longer builds for wasm.

## Not changed, and why it matters

- Windows reads `PATHEXT`, `SystemRoot` and `USERPROFILE` from the process
  environment (`sys/windows/fs.rs`), not from the shell's.
- A builtin of the shell that blocks in a read no one will feed (a FIFO
  opened by a redirection) cannot be interrupted; pa-shell's embedder
  abandons such a script's thread.

Drop this directory and the `[patch.crates-io]` entry when a brush-core
release offers an embedding interface that covers the list above.
