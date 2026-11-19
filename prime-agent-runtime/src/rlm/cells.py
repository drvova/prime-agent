"""Each cell's evaluation on its own thread, and the handles of cells that
outlived their yield window.

Adapted from Nodecode's eval lifecycle (``src/engine/exec.lisp``: every
evaluation runs on its own thread, the call parks for one yield window, and an
evaluation still running then continues in the background with an id the
model can poll, await, or interrupt). Only the design is borrowed.

A cell thread runs its own event loop: the cell body and every task the cell
starts run there, so asyncio inside one cell is single-threaded as it always
was, and a cell stuck in synchronous code stops only itself. The thread keeps
its loop running after the body ends, for as long as the cell's tasks run.
The kernel's main loop serves the protocol and the host bridge.

Across cells, every task is awaitable from any loop (a future of another loop
is awaited through a local twin), and asyncio's primitives run each operation
on the thread of the loop they are bound to. The namespace is the one shared
``__main__`` dict, as in Nodecode's shared image; only the result history
(``_``) waits until the model reads a value.
"""

from __future__ import annotations

import asyncio
import contextvars
import dataclasses
import functools
import heapq
import threading
import time
from collections.abc import Callable, Generator
from typing import Any

# Buffered output of one backgrounded cell; the oldest text goes first.
_OUTPUT_CAP_CHARS = 1_000_000
# Finished background cells kept for a later read before the oldest is dropped.
_FINISHED_KEPT = 64
# How long an interrupt waits for a body suspended at an await to take its
# cancel (with the await's traceback) before it injects into running code.
_CANCEL_GRACE_S = 0.05

_LoopBase: type[asyncio.AbstractEventLoop] = getattr(
    asyncio.DefaultEventLoopPolicy, "_loop_factory", asyncio.SelectorEventLoop
)


class KernelLoop(_LoopBase):  # type: ignore[misc, valid-type]
    """An event loop whose scheduling is safe from any thread.

    A future completed on another thread schedules its callbacks through its
    loop's ``call_soon``; off the loop's own thread that becomes
    ``call_soon_threadsafe``, which also wakes the selector. Timers scheduled
    from another thread are pushed on the loop's own thread.
    """

    def _off_thread(self) -> bool:
        owner = self._thread_id  # type: ignore[attr-defined]
        return owner is not None and owner != threading.get_ident()

    def call_soon(self, callback: Any, *args: Any, context: contextvars.Context | None = None) -> Any:
        if self._off_thread():
            return self.call_soon_threadsafe(callback, *args, context=context)
        return super().call_soon(callback, *args, context=context)

    def call_at(self, when: float, callback: Any, *args: Any, context: contextvars.Context | None = None) -> Any:
        if not self._off_thread():
            return super().call_at(when, callback, *args, context=context)
        timer = asyncio.TimerHandle(when, callback, args, self, context)

        def push() -> None:
            if not timer.cancelled():
                heapq.heappush(self._scheduled, timer)  # type: ignore[attr-defined]
                timer._scheduled = True  # type: ignore[attr-defined]

        self.call_soon_threadsafe(push)
        return timer


class _CellLoop(KernelLoop):
    """The loop of one cell thread: its body and the tasks it starts."""


# --- awaiting another loop's futures ------------------------------------------


def _copy_outcome(local: asyncio.Future[Any], done: asyncio.Future[Any]) -> None:
    if local.done():
        return
    if done.cancelled():
        local.cancel()
    elif done.exception() is not None:
        local.set_exception(done.exception())  # type: ignore[arg-type]
    else:
        local.set_result(done.result())


def _relay(local: asyncio.Future[Any], done: asyncio.Future[Any]) -> None:
    """The bridge's callback on a foreign future (bash's await walk follows
    it from the foreign future to the local one the waiter is parked on)."""
    try:
        local.get_loop().call_soon_threadsafe(_copy_outcome, local, done)
    except RuntimeError:
        pass  # the waiting loop already closed: nobody waits any more


def _chain(source: asyncio.Future[Any], loop: asyncio.AbstractEventLoop) -> asyncio.Future[Any]:
    """A future on ``loop`` that settles as ``source`` (another loop's) does."""
    local = loop.create_future()
    source.get_loop().call_soon_threadsafe(source.add_done_callback, functools.partial(_relay, local))
    return local


class _Bridged:
    """A coroutine awaited so that every future it yields (all nested awaits
    bubble up here) may belong to another loop: a foreign future is awaited
    through a local one chained to it. Cancelling the waiter cancels the
    foreign future it waits on, as asyncio does for its own."""

    def __init__(self, coro: Any) -> None:
        self._coro = coro

    def __await__(self) -> Generator[Any, Any, Any]:
        coro = self._coro
        loop = asyncio.get_running_loop()
        send: Any = None
        error: BaseException | None = None
        while True:
            try:
                yielded = coro.throw(error) if error is not None else coro.send(send)
            except StopIteration as stop:
                return stop.value
            send, error = None, None
            blocking = getattr(yielded, "_asyncio_future_blocking", None)
            if blocking is not None and asyncio.futures._get_loop(yielded) is not loop:
                yielded._asyncio_future_blocking = False
                try:
                    send = yield from _chain(yielded, loop).__await__()
                except asyncio.CancelledError as exc:
                    yielded.get_loop().call_soon_threadsafe(yielded.cancel)
                    error = exc
                except BaseException as exc:  # noqa: BLE001 - delivered into the coroutine
                    error = exc
            else:
                try:
                    send = yield yielded
                except BaseException as exc:  # noqa: BLE001 - delivered into the coroutine
                    error = exc


async def _bridged(coro: Any) -> Any:
    return await _Bridged(coro)


def _bridged_task(loop: asyncio.AbstractEventLoop, coro: Any, **kwargs: Any) -> asyncio.Task[Any]:
    """The cell loop's task factory: a task that can await any loop's futures."""
    task = asyncio.Task(_bridged(coro), loop=loop, context=kwargs.get("context"))
    name = kwargs.get("name")
    if name is not None:
        task.set_name(name)
    return task


def _lenient_ensure_future() -> None:
    """asyncio helpers (``wait_for`` and ``as_completed`` on 3.11) pin their
    futures to the running loop with ``ensure_future(f, loop=...)``, which
    refuses a future of another loop. Between kernel loops awaiting one is
    bridged, so a future of another kernel loop passes as is."""
    from asyncio import futures, tasks

    original = tasks.ensure_future
    if getattr(original, "_rlm_cells", False):
        return

    def ensure_future(coro_or_future: Any, *, loop: asyncio.AbstractEventLoop | None = None) -> Any:
        if (
            isinstance(loop, KernelLoop)
            and futures.isfuture(coro_or_future)
            and isinstance(futures._get_loop(coro_or_future), KernelLoop)
        ):
            return coro_or_future
        return original(coro_or_future, loop=loop)

    ensure_future._rlm_cells = True  # type: ignore[attr-defined]
    tasks.ensure_future = ensure_future
    asyncio.ensure_future = ensure_future


# --- asyncio primitives across cells ----------------------------------------
# A Lock/Event/Queue a cell keeps in a global is shared by every later cell,
# each on its own thread. Their state is not thread-safe, so each operation
# runs on the thread of the loop the primitive is bound to: synchronous ones
# wait for it there, coroutines run there and are awaited across.

_SYNC_METHODS = {
    "Event": ("set", "clear"),
    "Lock": ("release",),
    "Semaphore": ("release",),
    "BoundedSemaphore": ("release",),
    "Condition": ("notify", "notify_all"),
    "Queue": ("put_nowait", "get_nowait", "task_done"),
}
_ASYNC_METHODS = {
    "Event": ("wait",),
    "Lock": ("acquire",),
    "Semaphore": ("acquire",),
    "BoundedSemaphore": ("acquire",),
    "Condition": ("acquire", "wait", "wait_for"),
    "Queue": ("put", "get", "join"),
    "Barrier": ("wait", "reset", "abort"),
}


def _owner_elsewhere(primitive: Any) -> asyncio.AbstractEventLoop | None:
    """The running loop the primitive is bound to, when that is another thread's."""
    loop = getattr(primitive, "_loop", None)
    if not isinstance(loop, KernelLoop) or loop.is_closed() or not loop.is_running():
        return None
    return loop if loop._off_thread() else None


def _run_on(loop: asyncio.AbstractEventLoop, fn: Callable[..., Any]) -> Any:
    outcome: list[tuple[bool, Any]] = []
    ready = threading.Event()

    def call() -> None:
        try:
            outcome.append((True, fn()))
        except BaseException as exc:  # noqa: BLE001 - re-raised on the calling thread
            outcome.append((False, exc))
        finally:
            ready.set()

    loop.call_soon_threadsafe(call)
    ready.wait()
    ok, value = outcome[0]
    if ok:
        return value
    raise value


def _marshal_sync(method: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(method)
    def wrapper(self: Any, *args: Any, **kwargs: Any) -> Any:
        owner = _owner_elsewhere(self)
        if owner is None:
            return method(self, *args, **kwargs)
        return _run_on(owner, functools.partial(method, self, *args, **kwargs))

    return wrapper


def _marshal_async(method: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(method)
    async def wrapper(self: Any, *args: Any, **kwargs: Any) -> Any:
        owner = _owner_elsewhere(self)
        if owner is None:
            return await method(self, *args, **kwargs)
        return await asyncio.wrap_future(asyncio.run_coroutine_threadsafe(method(self, *args, **kwargs), owner))

    return wrapper


def _marshal_primitives() -> None:
    from asyncio import mixins

    original = mixins._LoopBoundMixin._get_loop
    if getattr(original, "_rlm_cells", False):
        return

    def _get_loop(self: Any) -> asyncio.AbstractEventLoop:
        # A primitive bound to a cell loop that has since closed rebinds:
        # nothing can wait on the dead loop any more.
        if self._loop is not None and self._loop.is_closed():
            self._loop = None
        return original(self)

    _get_loop._rlm_cells = True  # type: ignore[attr-defined]
    mixins._LoopBoundMixin._get_loop = _get_loop
    for names, wrap in ((_SYNC_METHODS, _marshal_sync), (_ASYNC_METHODS, _marshal_async)):
        for class_name, methods in names.items():
            cls = getattr(asyncio, class_name)
            for name in methods:
                if name in cls.__dict__:
                    setattr(cls, name, wrap(cls.__dict__[name]))


# --- cooperative sleep ------------------------------------------------------
# An exception injected into a thread fires at its next bytecode, never inside
# one blocking C call: a cell sleeping in time.sleep(600) would ignore its
# interrupt for ten minutes. On a cell thread, sleep waits on the cell's
# interrupt instead; every other thread keeps the real one.

_real_sleep = time.sleep
_cell_threads: dict[int, CellRun] = {}


def _sleep(seconds: float) -> None:
    run = _cell_threads.get(threading.get_ident())
    if run is None:
        _real_sleep(seconds)
        return
    if not isinstance(seconds, (int, float)):
        raise TypeError(f"'{type(seconds).__name__}' object cannot be interpreted as an integer or float")
    if seconds < 0:
        raise ValueError("sleep length must be non-negative")
    if seconds == 0:
        _real_sleep(0)
        return
    if run._interrupt_requested.wait(seconds):
        run._deliver_in_sleep()


def install() -> None:
    """Process-wide setup, once: the sleep shim and the cross-loop rules."""
    if time.sleep is not _sleep:
        time.sleep = _sleep
    _lenient_ensure_future()
    _marshal_primitives()


# --- the registry -----------------------------------------------------------

_lock = threading.Lock()
_runs: dict[str, CellRun] = {}
_finished_order: list[str] = []
_window: CellRun | None = None


@dataclasses.dataclass(frozen=True)
class CellResult:
    """How a backgrounded cell ended: ``status`` is ``ok``, ``error`` or
    ``interrupted``; ``value`` is its trailing expression's value (None when
    it had none or failed); ``text`` is the value's repr or the error's
    traceback; ``output`` is everything it printed after it was backgrounded."""

    status: str
    value: Any
    text: str
    output: str
    duration: float

    def __repr__(self) -> str:
        return f"CellResult(status={self.status!r}, duration={self.duration:.2f}, text={self.text!r})"


Outcome = tuple[str, Any, "str | None", "dict[str, Any] | None"]


class CellRun:
    """One execute request's evaluation on its own thread (Nodecode's
    ``eval-N``: ``spawn-background-eval`` and ``run-background-eval``)."""

    def __init__(self, cell_id: str, codes: list[Any], has_trailing: bool, ns: dict[str, Any]) -> None:
        from . import repl

        import secrets

        self.cell_id = cell_id
        self.activity_id = secrets.token_hex(16)
        self.ns = ns
        self._codes = codes
        self._has_trailing = has_trailing
        self._state_lock = threading.Lock()
        self._interrupt_requested = threading.Event()
        self._injected = False
        self._closing = False
        self._backgrounded = False
        self._window_open = True
        self.started = time.monotonic()
        self.thread: threading.Thread | None = None
        self.loop: _CellLoop | None = None
        self.task: asyncio.Task[Any] | None = None
        self.task_ready = threading.Event()
        # status, value, result text, error event
        self.outcome: Outcome | None = None
        self.result: CellResult | None = None
        self._output: list[str] = []
        self._output_chars = 0
        self._output_dropped = False
        self._notice_accepted = False
        self._result_read = False
        assert repl._loop is not None
        self._main = repl._loop
        # Settles when the request leaves its window: finished in it, or backgrounded.
        self.window_closed: asyncio.Future[bool] = repl._loop.create_future()
        # Settles when the evaluation itself has ended.
        self.done: asyncio.Future[CellResult | None] = repl._loop.create_future()

    # -- the thread ------------------------------------------------------

    def start(self, context: contextvars.Context) -> None:
        self.thread = threading.Thread(
            target=context.run, args=(self._thread_main,), name=f"rlm-cell-{self.cell_id[:12]}", daemon=True
        )
        self.thread.start()

    def _thread_main(self) -> None:
        from . import repl

        tid = threading.get_ident()
        _cell_threads[tid] = self
        loop = _CellLoop()
        loop.set_task_factory(_bridged_task)
        asyncio.set_event_loop(loop)
        self.loop = loop
        outcome: Outcome | None = None
        try:
            try:
                outcome = self._evaluate(loop)
                self._close()
            except KeyboardInterrupt as exc:
                # The interrupt landed in this thread's own bookkeeping.
                self._close()
                if outcome is None:
                    outcome = ("error", None, None, repl._interrupt_event(self.cell_id, exc))
            _cell_threads.pop(tid, None)
            self._finish(outcome)
            # The cell's own tasks keep this thread: they run on its loop.
            _run_remaining_tasks(loop)
        finally:
            _cell_threads.pop(tid, None)
            try:
                loop.close()
            except BaseException:  # noqa: BLE001 - nothing waits on a closing loop
                pass
            asyncio.set_event_loop(None)

    def _evaluate(self, loop: _CellLoop) -> Outcome:
        from . import repl

        task = asyncio.Task(_bridged(repl._run_codes(self._codes, self.ns)), loop=loop)
        self.task = task
        execution = repl._current_cell_execution.get()
        if execution is not None:
            execution.owner = task
        self.task_ready.set()
        try:
            with self._state_lock:
                if self._interrupt_requested.is_set() and not self._injected:
                    # Interrupted before its first step.
                    self._injected = True
                    task.cancel()
            value = loop.run_until_complete(task)
        except BaseException as exc:  # noqa: BLE001 - every cell failure becomes an error event
            _cancel_and_drain(loop, task)
            if self._interrupt_requested.is_set() and isinstance(exc, (KeyboardInterrupt, asyncio.CancelledError)):
                return "error", None, None, repl._interrupt_event(self.cell_id, exc)
            return "error", None, None, repl._error_event(self.cell_id, exc)
        if not (self._has_trailing and value is not None):
            return "ok", value, None, None
        try:
            return "ok", value, repl._cap_text(repr(value)), None
        except BaseException as exc:  # noqa: BLE001 - a broken __repr__ is a cell error
            if self._interrupt_requested.is_set() and isinstance(exc, KeyboardInterrupt):
                return "error", None, None, repl._interrupt_event(self.cell_id, exc)
            return "error", None, None, repl._error_event(self.cell_id, exc)

    def _close(self) -> None:
        """No interrupt may land after this: the outcome is decided."""
        with self._state_lock:
            self._closing = True
            if self._injected and self.thread is not None and self.thread.ident is not None:
                # Clear an injected exception that has not fired yet.
                _set_async_exc(self.thread.ident, None)

    def _finish(self, outcome: Outcome | None) -> None:
        from . import repl

        assert outcome is not None
        self.outcome = outcome
        status, value, text, error = outcome
        with self._state_lock:
            in_window = self._window_open and not self._backgrounded
            self._window_open = False
        if in_window:
            repl._deliver_cell_outcome(self)
            self._main.call_soon_threadsafe(_settle, self.window_closed, True)
            self._main.call_soon_threadsafe(_settle, self.done, None)
            return
        if error is not None:
            if error.get("ename") == "KeyboardInterrupt":
                status = "interrupted"
            text = "".join(error.get("traceback") or []) or f"{error.get('ename')}: {error.get('evalue')}"
        self.result = CellResult(
            status=status,
            value=value if status == "ok" else None,
            text=text or "",
            output=self.output(),
            duration=time.monotonic() - self.started,
        )
        _note_finished(self)
        self._main.call_soon_threadsafe(_settle, self.done, self.result)
        self._main.call_soon_threadsafe(_schedule_completion_notice, self)

    # -- the window ------------------------------------------------------

    def background(self) -> bool:
        """Hand the evaluation to the background unless it already finished
        (``mark-exec-backgrounded``). True when it now runs in the background."""
        from . import repl

        with repl._divert_lock, self._state_lock:
            if not self._window_open:
                return False
            self._window_open = False
            self._backgrounded = True
        return True

    @property
    def backgrounded(self) -> bool:
        return self._backgrounded

    # -- interrupt -------------------------------------------------------

    def interrupt(self) -> bool:
        """The soft stop (``exec-stop-process`` without ``:hard``). A body
        suspended at an await takes a cancel there, so its traceback names the
        await; code running on the thread (the body, or a task of the cell
        hogging it) takes an injected KeyboardInterrupt at its next bytecode;
        a cooperative sleep raises it at once. A single blocking C call takes
        it when the call returns. True when the cell was still running."""
        with self._state_lock:
            if self._closing:
                return False
            first = not self._interrupt_requested.is_set()
            self._interrupt_requested.set()
        if not first:
            return True
        loop = self.loop
        if loop is not None and self.task_ready.is_set():
            try:
                loop.call_soon_threadsafe(self._cancel_at_await)
            except RuntimeError:
                pass  # the loop closed: the thread is past its body
        timer = threading.Timer(_CANCEL_GRACE_S, self._inject)
        timer.daemon = True
        timer.start()
        return True

    def _cancel_at_await(self) -> None:
        """On the cell thread, with its loop idle: the body is at an await."""
        with self._state_lock:
            if self._closing or self._injected:
                return
            self._injected = True  # the cancel is the delivery
        if self.task is not None and not self.task.done():
            self.task.cancel()

    def _inject(self) -> None:
        with self._state_lock:
            thread = self.thread
            if self._closing or self._injected or thread is None or thread.ident is None:
                return
            self._injected = True
            _set_async_exc(thread.ident, KeyboardInterrupt)
        loop = self.loop
        if loop is not None:
            try:
                loop.call_soon_threadsafe(_wake)
            except RuntimeError:
                pass

    def _deliver_in_sleep(self) -> None:
        """A cooperative sleep woke for the interrupt: it raises it there
        unless an injection or a cancel already carries it."""
        with self._state_lock:
            if self._closing or self._injected:
                return
            self._injected = True
        raise KeyboardInterrupt

    # -- output ----------------------------------------------------------

    def divert(self, text: str) -> None:
        self._output.append(text)
        self._output_chars += len(text)
        while self._output_chars > _OUTPUT_CAP_CHARS and len(self._output) > 1:
            self._output_chars -= len(self._output.pop(0))
            self._output_dropped = True

    def output(self) -> str:
        text = "".join(self._output)
        if self._output_dropped:
            return f"[... older output dropped: kept the newest {_OUTPUT_CAP_CHARS} characters ...]\n{text}"
        return text


def _cancel_and_drain(loop: _CellLoop, task: asyncio.Task[Any]) -> None:
    """Let an interrupted body run its finally blocks (a bash() await kills
    its child there)."""
    if not task.done():
        task.cancel()
        try:
            loop.run_until_complete(task)
        except BaseException:  # noqa: BLE001 - the first failure is the one reported
            pass
    if task.done() and not task.cancelled():
        task.exception()


def _run_remaining_tasks(loop: _CellLoop) -> None:
    """Run the loop until every task the cell started has ended."""
    while True:
        pending = [task for task in asyncio.all_tasks(loop) if not task.done()]
        if not pending:
            return
        try:
            loop.run_until_complete(asyncio.wait(pending))
        except BaseException:  # noqa: BLE001 - a task's stray KeyboardInterrupt ends only that task
            continue


def _settle(future: asyncio.Future[Any], value: Any) -> None:
    if not future.done():
        future.set_result(value)


def _wake() -> None:
    pass


def _set_async_exc(thread_id: int, exc_type: type[BaseException] | None) -> None:
    """Raise ``exc_type`` in the thread at its next bytecode (None clears a
    pending one): CPython's per-thread asynchronous exception."""
    import ctypes

    ctypes.pythonapi.PyThreadState_SetAsyncExc(
        ctypes.c_ulong(thread_id), ctypes.py_object(exc_type) if exc_type is not None else None
    )


def register(run: CellRun) -> None:
    global _window
    with _lock:
        if run.cell_id in _runs:
            raise ValueError(f"cell id {run.cell_id!r} is still running in the background")
        _runs[run.cell_id] = run
        _window = run


def leave_window(run: CellRun) -> None:
    """The request left its window: a finished cell drops out of the
    registry, a backgrounded one stays until it is aged out."""
    global _window
    with _lock:
        if _window is run:
            _window = None
        if not run.backgrounded:
            _runs.pop(run.cell_id, None)


def find(cell_id: str | None) -> CellRun | None:
    """The running cell an interrupt targets: by id, or the cell in its window."""
    with _lock:
        if cell_id is None:
            return _window
        run = _runs.get(cell_id)
        return run if run is not None and run.result is None else None


def window_task() -> asyncio.Task[Any] | None:
    with _lock:
        run = _window
    if run is None or run.task is None or run.task.done():
        return None
    return run.task


def diverting(cell_id: str | None) -> CellRun | None:
    """The backgrounded, still-running cell whose writes go to its buffer."""
    if cell_id is None:
        return None
    run = _runs.get(cell_id)
    if run is not None and run.backgrounded and run.result is None:
        return run
    return None


def _note_finished(run: CellRun) -> None:
    with _lock:
        _finished_order.append(run.cell_id)
        while len(_finished_order) > _FINISHED_KEPT:
            _runs.pop(_finished_order.pop(0), None)


# --- the completion notice --------------------------------------------------


def _schedule_completion_notice(run: CellRun) -> None:
    task = asyncio.get_running_loop().create_task(_notify_completion(run))
    task.add_done_callback(lambda done: done.cancelled() or done.exception())


_ACTIVITY_MIME = "application/vnd.prime-agent.bash-activity+json"


def announce_background(run: CellRun) -> None:
    """Tell the host a backgrounded cell is live background work (the
    activity track bash() handles ride), before its request ends."""
    from . import repl

    repl.emit({_ACTIVITY_MIME: {"id": run.activity_id, "cell": run.cell_id, "active": True}})


async def _notify_completion(run: CellRun) -> None:
    """The ``cell.completed`` host request, once, unless the model already read
    the result; a read after the host accepted it withdraws it (``cell.consumed``).
    The activity is released after it, so the wake is admitted first."""
    from . import repl

    try:
        await _send_completion(run)
    finally:
        if repl.is_active():
            repl.emit({_ACTIVITY_MIME: {"id": run.activity_id, "cell": run.cell_id, "active": False}})


async def _send_completion(run: CellRun) -> None:
    from . import repl
    from .bash import _output_tail

    result = run.result
    if result is None or run._result_read or not repl.is_active():
        return
    tail, truncated = _output_tail(result.output)
    request: dict[str, Any] = {
        "type": "cell.completed",
        "cell": run.cell_id,
        "status": result.status,
        "output": tail,
        "outputTruncated": truncated,
        "text": result.text[-2000:],
    }
    try:
        reply = await repl.host_request(request)
    except (OSError, RuntimeError):
        return
    if isinstance(reply, dict) and reply.get("status") == "ok":
        run._notice_accepted = True
        if run._result_read:
            repl.host_notify({"type": "cell.consumed", "cell": run.cell_id})


def _note_read(run: CellRun) -> None:
    """A result read inside a live cell reaches the model: its notice is stale."""
    from . import repl

    if run.result is None or run._result_read or repl._current_cell.get() is None:
        return
    run._result_read = True
    if run._notice_accepted:
        repl.host_notify({"type": "cell.consumed", "cell": run.cell_id})


# --- the model's handle -----------------------------------------------------


class CellHandle:
    """A cell that outlived its yield window, still running or finished.

    ``await h`` waits for it and returns its CellResult; ``await h.wait(s)``
    waits at most ``s`` seconds (None while it still runs); ``h.poll()`` is
    one non-blocking look; ``h.output()`` / ``h.tail(n)`` read what it
    printed since it was backgrounded; ``h.cancel()`` interrupts it.
    """

    def __init__(self, run: CellRun) -> None:
        self._run = run

    @property
    def id(self) -> str:
        return self._run.cell_id

    @property
    def running(self) -> bool:
        return self._run.result is None

    def poll(self) -> CellResult | None:
        result = self._run.result
        if result is not None:
            _note_read(self._run)
        return result

    def output(self) -> str:
        return self._run.output()

    def tail(self, n: int = 50) -> str:
        return "\n".join(self._run.output().splitlines()[-n:])

    def cancel(self) -> bool:
        """Interrupt it; True when it was still running."""
        return self._run.result is None and self._run.interrupt()

    async def _wait(self, seconds: float | None) -> CellResult | None:
        done = self._run.done
        if seconds is None:
            await asyncio.shield(done)
        else:
            await asyncio.wait({done}, timeout=max(0.0, seconds))
        return self.poll()

    def wait(self, seconds: float) -> Any:
        return self._wait(seconds)

    def __await__(self) -> Generator[Any, None, CellResult]:
        result = yield from self._wait(None).__await__()
        assert result is not None
        return result

    def __reduce__(self) -> Any:
        raise TypeError("CellHandle is bound to a live kernel thread and cannot be pickled")

    def __repr__(self) -> str:
        result = self._run.result
        state = "running" if result is None else result.status
        return f"CellHandle(id={self.id!r}, {state}, {time.monotonic() - self._run.started:.1f}s)"


def get(cell_id: str) -> CellHandle:
    """The handle of a backgrounded cell, by the id its result named."""
    with _lock:
        run = _runs.get(cell_id)
    if run is None or not run.backgrounded:
        raise KeyError(f"no background cell {cell_id!r}; cells.handles() lists them")
    return CellHandle(run)


def handles() -> list[CellHandle]:
    """Backgrounded cells, oldest first: running ones and kept finished ones."""
    with _lock:
        runs = [run for run in _runs.values() if run.backgrounded]
    return [CellHandle(run) for run in runs]
