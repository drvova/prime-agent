# REPL runtime protocol

`python -m rlm.repl` starts a CPython REPL runtime that executes code cells in
one persistent `__main__` namespace, each cell on its own thread (see
Execution). The wire
format is newline-delimited JSON: one object per line, UTF-8, no other framing.
The current protocol version is `3`; the runtime announces it in the `ready`
event.

## Channels

- Requests arrive on fd 0 (stdin).
- Events leave on a private dup of the original fd 1, made before anything else
  runs. Every frame is one locked write sequence, so frames never interleave.
- Python-level writes through `sys.stdout`/`sys.stderr` are intercepted at
  write time, tagged with the writing context's cell id, and shipped straight
  to the protocol.
- fds 1 and 2 are redirected into pipes at startup; pump threads read them and
  ship the bytes as `stdout`/`stderr` events with `id: null` — raw fd bytes
  (`os.write`, `sys.stdout.buffer.write`, C extensions, subprocesses) are never
  attributed to a cell.
  Neither channel can corrupt protocol framing. Ordering is preserved within
  each channel, not across them.
- fd 0 is rebound to `/dev/null` after the reader thread takes it, so user
  `input()` sees EOF instead of consuming protocol frames.

## Requests

| Request | Fields |
|---|---|
| `execute` | `{"type":"execute","id":str,"code":str,"timeoutMs"?:int,"yieldMs"?:int}` — `timeoutMs` is the host's armed cell time limit; bounded `bash()` awaits in the cell degrade before it. `yieldMs` is the yield window: a cell still running when it ends continues in the background (see Background cells) |
| `interrupt` | `{"type":"interrupt","id"?:str}` — no reply |
| `host_reply` | `{"type":"host_reply","id":str,"data":{"status":"ok","result":{...}}}` or an error envelope — no reply |
| `snapshot` | `{"type":"snapshot","id":str,"path":str,"manifest_path":str,"max_bytes"?:int,"max_variable_bytes"?:int,"prune_oversized"?:bool}` |
| `restore` | `{"type":"restore","id":str,"path":str}` |
| `list_names` | `{"type":"list_names","id":str}` |
| `mcp_status` | `{"type":"mcp_status","id":str,"servers":[str,...],"timeout_ms"?:number}` — host-side view query: per-server tool listing (opens each server on demand, bounded by `timeout_ms` per server; default 10s); the `done` frame carries `connections: [{server, tools: [{name, description}] | null, error: str | null}]` |
| `bash_activity` | `{"type":"bash_activity","id":str,"action":"list"|"tail"|"kill","activityId"?:str,"lines"?:int}` — out-of-band even during a running cell; tail lines 1–200, response capped at 16 KiB; opaque IDs resolve only against this kernel’s handles |
| `shutdown` | `{"type":"shutdown","id"?:str}` |

Requests other than `interrupt`, `host_reply`, and `bash_activity` run strictly in order, one at
a time. A malformed line
produces `{"event":"error","id":null,"ename":"ProtocolError",...}` and the
runtime keeps serving. Closing stdin is equivalent to `shutdown`.

## Events

- `{"event":"ready","protocol":3,"python":"3.13.11"}` — sent once at startup;
  the handshake. No banner precedes it.
- `{"event":"stdout"|"stderr","id":str|null,"text":str}` — captured output.
  `id` is the cell whose Python execution context performed the write; asyncio
  tasks inherit the spawning cell's id (even after that cell finished). `null`
  for user threads, raw fd writes (`os.write`, C extensions, subprocesses),
  and anything else without provable ownership — bytes read from the fd pipes
  are never attributed to a cell. A Python-level write ships at most 64 Ki
  characters per frame; a larger write arrives as multiple events in order.
- `{"event":"result","id":str,"text":str}` — `repr` of the cell's trailing
  expression when the body ends in an expression whose value is not `None`.
  The value is also bound to `_` in the namespace. The `repr` content is capped
  at 1,048,576 characters; a longer `repr` is truncated to the cap and a trailing
  truncation marker is appended after it, so the total `text` can exceed the cap
  by the marker's length.
- `{"event":"display","id":str|null,"data":{mime:payload,...}}` — one dict of
  MIME type to JSON payload, shipped verbatim from `emit()`. A payload whose
  JSON encoding exceeds 16 Mi characters is refused: `emit()` raises
  `ValueError` in the calling cell. `id` rides task
  context: an asyncio task spawned by a cell keeps that cell's id even after
  the cell finishes; user threads emit `null`.
- `{"event":"host_request","id":str,"data":{...}}` — one typed request from
  runtime code to the host; the host answers with a `host_reply` request
  carrying the same id. `data` is subject to the same encoding cap as a
  `display` payload: `host_request()` raises `ValueError` in the calling cell
  instead of sending an oversized request.
- `{"event":"error","id":str|null,"ename":str,"evalue":str,"traceback":[str,...]}`
  — `evalue` and each `traceback` entry are capped like `result` text (same
  cap, same trailing marker).
- `{"event":"done","id":str,"status":"ok"|"error"}` — exactly one per id'd
  request, always after all of that request's other events. An execute whose
  cell outlived its `yieldMs` ends with `{"status":"backgrounded","cell":str}`. A snapshot `done`
  adds `saved`, `skipped`, `pruned`, `bytes`; a restore `done` adds `restored`,
  `failed`; a `list_names` `done` adds `names`; a failed snapshot/restore adds
  `reason`. Bash activity `done` carries `activities` (list), `tail` (tail),
  or `killed` (kill); `status:"error"` with `reason` on unknown IDs.
  The daemon advertises `kernel_bash_activity`; clients poll `list_kernel_bash`
  for updates (no push events). Each row contains opaque `id`, `command`,
  `pid`, `durationMs`, and `status` (`running` or `finished`). Finished rows
  are retained for the latest 64 completions within a live kernel only; restart
  invalidates all IDs. `tail_kernel_bash` returns `{id,tail}`, and
  `kill_kernel_bash` returns `{id,killed}`. Neither action accepts a PID.
  Restoring a missing file reports `status:"ok"` with empty
  `restored`/`failed` lists and `reason:"snapshot not found"`.

Before a cell's `done`, the runtime drains both channels: tagged Python-level
writes ship synchronously from the writing thread, and the fd pipes are fenced
with a marker byte sequence awaited in the pumps, so every byte the cell wrote
synchronously — including direct fd writes — precedes its `done`. Ordering
between a cell's Python-level writes and its raw fd writes is not guaranteed
(two channels).

## Execution

Cells compile with `PyCF_ALLOW_TOP_LEVEL_AWAIT` (on the main loop, so a
`SyntaxError` fails the request at once). Each cell then runs on its own
thread, `rlm-cell-…`, with its own event loop (`rlm.cells`): the cell body and
every task the cell starts run there, so `await` works at top level, asyncio
inside one cell stays single-threaded, and a cell stuck in synchronous code
stops only itself. The thread keeps its loop running after the body ends for
as long as the cell's tasks run, so background tasks keep running between
cells. Across cells: a task (or any future) of another cell is awaitable, and
asyncio's `Lock`/`Event`/`Semaphore`/`Condition`/`Queue`/`Barrier` run each
operation on the thread of the loop they are bound to. The main loop serves
the protocol, the host bridge, and MCP.

The cell body sees the cell's context variables (cell id, completion barrier),
so output attribution works as before. `signal.signal` cannot be called from a
cell (Python allows it only on the main thread). The trailing expression's
`repr` runs on the cell thread; its value is bound to `_` when the result is
delivered. Each cell's source is registered
in `linecache` under `<cell-N>`, so tracebacks show the offending source line.
Tracebacks are plain `traceback` formatting with the runtime's own frames
stripped, keeping cell and library frames; no colors, no decoration.

## Background cells

An execute that carries `yieldMs` waits that long for its cell. A cell still
running then (synchronous or suspended at an `await`) continues in the
background, as Nodecode's evaluations do: the request ends with
`done` `{"status":"backgrounded","cell":id}` and the next request runs. From
then on, the cell's Python-level writes go to its handle instead of the
protocol, so they never reach another cell's output; raw fd writes stay
`id:null` as always. Its value is not bound to `_`.

When it ends, the runtime sends one `host_request`
`{"type":"cell.completed","cell":id,"status":"ok"|"error"|"interrupted","output":str,"outputTruncated":bool,"text":str}`:
`output` is the tail of what it printed in the background (the newest 20
lines / 2,000 characters, as `bash.completed`), `text` its value's `repr` or
its traceback. A read of the result inside a cell after the host accepted the
notice sends `{"type":"cell.consumed","cell":id}` (no reply awaited).

`rlm.cells.get(id)` returns its `CellHandle`: `await h` (a `CellResult`:
`status`, `value`, `text`, `output`, `duration`), `await h.wait(seconds)`
(None while it runs), `h.poll()`, `h.output()`, `h.tail(n)`, `h.running`, and
`h.cancel()` (an interrupt). `rlm.cells.handles()` lists them; the newest 64
finished ones are kept.

## Interrupt

`{"type":"interrupt"}` raises `KeyboardInterrupt` in the running cell; with an
`id`, also in a backgrounded cell. Without
an `id` the interrupt applies to the running request, or — when none is running
yet — to the next queued one; with an `id` it applies to that request only.
An interrupt that arrives before its request starts executing is parked and
delivered the moment the request becomes active, so `execute` + `interrupt`
written back-to-back still interrupts the cell. A request stays
interrupt-targetable until its `done` event is emitted: this covers the
post-run trailing-expression `repr` and output drain. Interrupts for finished
or unknown requests are dropped.

Cell delivery happens on the cell's thread:

- The cell is suspended at an `await` (its loop is idle): the body task is
  cancelled there and the runtime reports the cancellation as a
  `KeyboardInterrupt` whose traceback names the await. User code catching
  `KeyboardInterrupt` around an `await` does not intercept it.
- Code is running on the cell thread (the body's synchronous bytecode, its
  `repr`, or one of the cell's tasks hogging the thread): after 50 ms a
  `KeyboardInterrupt` is injected into the thread
  (`PyThreadState_SetAsyncExc`) and fires at its next bytecode; a task that
  takes it dies with it, as before.
- `time.sleep` on a cell thread waits on the cell's interrupt and raises it at
  once. A single other blocking C call (`selectors.select()` with no timeout,
  a blocking `recv`) takes the interrupt only when it returns: Python cannot
  break a C call on a non-main thread, so the host's stop-then-restart path
  covers it.

Snapshot and restore requests still run on the main loop and take SIGINT
there, as before.

Both paths end with an `error` event (`ename` `KeyboardInterrupt`) and
`done` with `status:"error"`; the runtime keeps serving. When nothing is
running or queued, SIGINT and interrupt requests are ignored.

## Display bridge

`from rlm.repl import emit` inside a cell (or any user thread) ships a
`display` event. `emit(data)` takes one non-empty dict keyed by MIME type
strings; the dict is forwarded verbatim as the event's `data`.

## Host bridge

`await rlm.repl.host_request(data)` ships a `host_request` event with a
runtime-minted id and awaits the matching `host_reply`, returning its `data`
dict verbatim. Replies are routed on the reader thread like `interrupt` —
never through the request queue, since the awaiting cell is itself the
in-flight execute. Replies for unknown ids, or for a request whose awaiting
cell was cancelled, are dropped. `rlm.repl.is_active()` reports whether the
process is serving the protocol (importing the module does not count).

## Snapshot / restore

`snapshot` serializes the user namespace with `dill` (recurse mode), one name
at a time: `_`-prefixed names and
`{rlm, mcp, bash, asyncio, In, Out, get_ipython, exit, quit, open}` are always
skipped; a name whose pickle exceeds `max_variable_bytes` or would push the
total over `max_bytes` is skipped and reported. With `prune_oversized`, only
names exceeding the per-variable cap (`max_variable_bytes`) are also deleted
from the namespace and listed in `pruned`; names skipped for the aggregate
`max_bytes` cap are reported in `skipped` but kept in the namespace. The
payload is written atomically (tmp file + `os.replace`) and a JSON manifest
(`version`, `savedNames`, `skipped`, `pruned`, `bytes`, `pythonVersion`,
`timestamp`) is written to `manifest_path`. A manifest write failure fails the
snapshot (and nothing is pruned).

`restore` loads the payload and revives each name independently; a missing
file yields an ok empty restore with `reason:"snapshot not found"`, a corrupt
file fails with a `reason`, and per-name failures are listed in `failed`.
Names `In`, `Out`, and `get_ipython` in a payload are never restored. A pickled file
handle is never reopened: one saved by path revives as a closed stub that
keeps its `name` and `mode` and raises `ValueError` on I/O like any closed
file (reopening with `"w"`/`"w+"` would truncate the file, and `"x"` would
fail on an existing one), and one saved as a raw fd number fails that name. `dill` is imported lazily; when unavailable, snapshot and restore
fail with `status:"error"` and a `reason`.

`list_names` replies with `done` carrying `names`: the sorted user-defined
top-level names under the same filter the snapshot applies.

## Shutdown

`shutdown` (or stdin EOF) kills live `rlm.bash` child process groups, replies
`done` (when the request carried an id), stops the loop, and exits 0.
