# shell: commands and their jobs

Each command is its own shell process in the session's working directory: shell state (`cd`, variables) does not carry over to the next call, so chain with `&&` inside one command. A job argument is a job id or any hash these functions return.

- `(bash command [options])`: run `command` and wait for it. The result is `{running #f, exit-code, output, truncated, timed-out, duration-ms, cached, job, pid, handle, tests}`: `output` is stdout and stderr together in the order written; `handle` is the `out:<id>` handle of a large output (page it with the `ctx-*` functions) or `#f`; `tests` is the parsed summary of a test run (cargo test, pytest, unittest, go test, jest, vitest: counts and each failure, also printed) or `#f`; `cached` marks a repeated read-only command (`git status`/`log`/`diff`, `ls`, `cat`, `rg`, `grep`) answered from its last result because its inputs did not change. A failing command is a result with a non-zero `exit-code`, never an error
  - The wait has a window: 10 seconds in a session that delivers `[bash-done]` notices, at most 600. A command that outlives it keeps running in the background and the call returns a snapshot instead: `{running #t, exit-code #f, output, output-truncated, duration-ms, job, pid, queued, command, note}` with `output` its newest 20 lines. Its result arrives with the `[bash-done]` notice, or through `bash-wait`
  - Options: `'wait-ms` (this call's window; `#f` for none), `'timeout-ms` (kill the command after this long and wait for it until then; `timed-out` is then `#t`), `'background #t` (do not wait: returns the job `{job, pid, command, running, queued, persistent}`), `'persistent #t` (POSIX, saved sessions only: a shell that survives kernel, worker, and daemon restarts and ends only on its own exit or an explicit kill; only for a server or long job that must outlive a restart)
  - A command the call waits for is killed when its cell is stopped or times out. A background job, a snapshot's job, and a persistent shell outlive their cell. Over the session's job limit a background command is held `queued` until a slot frees
- `(bash-wait job [seconds])`: wait for a job, for `seconds` or the session's window; its result, or a snapshot when it is still running
- `(bash-poll job)`: a finished job's result, or `#f` while it runs
- `(bash-tail job [lines])`: the last `lines` (default 50) the job has written so far, as a string
- `(bash-output job)`: everything the job has written so far, as a string
- `(bash-kill job [options])`: signal the job's whole process group; whether it was still running. Options: `'signal` (`'term`, the default, or `'kill`), `'grace-ms` (how long after a `'term` the group is killed; default 5000)
- `(bash-running? job)`: whether anything of the job still runs
- `(bash-jobs)`: the kernel's jobs, each `{job, pid, command, status, exit-code, duration-ms, started-at, persistent}`
- `(bash-persistent-shells)`: the session's persistent shells, re-attached ones included; after a restart this is how to get their jobs back
