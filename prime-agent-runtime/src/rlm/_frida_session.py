"""The Frida sessions and scripts `rlm.frida` hands out, and the bounded logs they fill.

Frida calls its `message` and log handlers on its own thread, so every log here is guarded by a
lock and wakes an awaiting cell through that cell's own event loop (each cell runs on its own
loop, see `rlm.cells`). Nothing here prints: a hook's output only ever lands in its log, and the
model reads it with `.messages()`, `.tail()` or `await .wait()`, so hook output never floods a
cell. A long `await .wait(...)` outlives its cell's window like any other await and continues as
a background cell, whose `[cell-done]` notice brings the message.

Every session opened here is registered until it is detached, and `detach_all` (the kernel's
shutdown, its `atexit`, and the owner watchdog) detaches what is left. A session that spawned its
target kills that target when it detaches: a spawned target the kernel stops controlling would
otherwise stay suspended (never resumed) or keep running instrumented with nobody reading it, so
it ends with its session the way a `bash()` child ends with the kernel (frida's own CLI offers
the same as `--kill-on-exit`).
"""

from __future__ import annotations

import asyncio
import atexit
import os
import threading
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any, Generic, Protocol, TypeVar

# Messages (or trace events) one log keeps; past it the oldest go and `dropped` counts them.
LOG_CAP = 1000
# Bytes of a message's binary `data` kept; `data_size` is the full length.
DATA_CAP_BYTES = 4096
# Characters of one payload a `.tail()` line shows.
_LINE_CHARS = 300


def note(fact: str) -> None:
    """One adoption count for the host's telemetry (`frida.used`: a fact name, never a target, a
    pid, a script, or a path). Fire-and-forget, like `bash.consumed`: the reply never matters."""
    from . import repl

    if repl.is_active():
        repl._send(
            {
                "event": "host_request",
                "id": os.urandom(16).hex(),
                "data": {"type": "frida.used", "fact": fact},
            }
        )


class _Sequenced(Protocol):
    seq: int


T = TypeVar("T", bound=_Sequenced)


@dataclass(frozen=True)
class FridaMessage:
    """One message a script produced: `send()` (`type` "send", the JSON `payload`, any binary
    `data` cut at DATA_CAP_BYTES with `data_size` its full length), an uncaught error ("error",
    `payload` the description, `stack`), or a `console.log` line ("log", `level`)."""

    seq: int
    type: str
    payload: Any
    data: bytes | None = None
    data_size: int = 0
    level: str | None = None
    stack: str | None = None

    def line(self) -> str:
        text = self.payload if isinstance(self.payload, str) else repr(self.payload)
        if len(text) > _LINE_CHARS:
            text = text[:_LINE_CHARS] + "..."
        tag = self.level if self.type == "log" else self.type
        suffix = f" (+{self.data_size} bytes)" if self.data_size else ""
        return f"#{self.seq} {tag}: {text}{suffix}"


class BoundedLog(Generic[T]):
    """A thread-safe ring of sequenced items fed from Frida's thread, awaitable from any loop."""

    def __init__(self, cap: int = LOG_CAP) -> None:
        self._lock = threading.Lock()
        self._items: deque[T] = deque(maxlen=cap)
        self._next_seq = 0
        self.dropped = 0
        self._closed = False
        self._waiters: list[tuple[asyncio.AbstractEventLoop, asyncio.Future[None]]] = []

    def append(self, make: Callable[[int], T]) -> None:
        with self._lock:
            if self._closed:
                return
            if len(self._items) == self._items.maxlen:
                self.dropped += 1
            self._items.append(make(self._next_seq))
            self._next_seq += 1
            waiters, self._waiters = self._waiters, []
        for loop, future in waiters:
            _wake(loop, future)

    def close(self) -> None:
        with self._lock:
            self._closed = True
            waiters, self._waiters = self._waiters, []
        for loop, future in waiters:
            _wake(loop, future)

    @property
    def closed(self) -> bool:
        return self._closed

    def __len__(self) -> int:
        return len(self._items)

    def items(self, n: int | None = None) -> list[T]:
        with self._lock:
            items = list(self._items)
        return items if n is None else items[-n:] if n > 0 else []

    async def next(
        self, start: int, predicate: Callable[[T], bool], timeout: float | None
    ) -> T | None:
        """The first item from sequence `start` on that `predicate` accepts, waiting up to
        `timeout` seconds (None: until the log closes); None on timeout or once closed."""
        loop = asyncio.get_running_loop()
        deadline = None if timeout is None else loop.time() + timeout
        while True:
            future: asyncio.Future[None] = loop.create_future()
            with self._lock:
                candidates = [item for item in self._items if item.seq >= start]
                start = self._next_seq
                closed = self._closed
                if not closed:
                    self._waiters.append((loop, future))
            try:
                for item in candidates:
                    if predicate(item):
                        return item
                if closed:
                    return None
                remaining = None if deadline is None else deadline - loop.time()
                if remaining is not None and remaining <= 0:
                    return None
                await asyncio.wait({future}, timeout=remaining)
            finally:
                with self._lock:
                    if (loop, future) in self._waiters:
                        self._waiters.remove((loop, future))


def _wake(loop: asyncio.AbstractEventLoop, future: asyncio.Future[None]) -> None:
    def settle() -> None:
        if not future.done():
            future.set_result(None)

    try:
        loop.call_soon_threadsafe(settle)
    except RuntimeError:
        pass  # The waiting cell's loop already closed: nobody is waiting any more.


class FridaScript:
    """A loaded script: its messages, its RPC exports, and its unload."""

    def __init__(self, session: FridaSession, script: Any, name: str | None) -> None:
        self.session = session
        self.name = name
        self._script = script
        self._log: BoundedLog[FridaMessage] = BoundedLog()
        self._cursor = 0
        self.unloaded = False
        script.on("message", self._on_message)
        script.on("destroyed", self._log.close)
        script.set_log_handler(self._on_log)

    def _on_message(self, message: dict[str, Any], data: bytes | None) -> None:
        size = len(data) if data else 0
        if message.get("type") == "send":
            self._log.append(
                lambda seq: FridaMessage(
                    seq, "send", message.get("payload"), data[:DATA_CAP_BYTES] if data else None, size
                )
            )
        else:
            self._log.append(
                lambda seq: FridaMessage(
                    seq,
                    str(message.get("type")),
                    message.get("description", message),
                    stack=message.get("stack"),
                )
            )

    def _on_log(self, level: str, text: str) -> None:
        self._log.append(lambda seq: FridaMessage(seq, "log", text, level=level))

    @property
    def dropped(self) -> int:
        return self._log.dropped

    def messages(self, n: int | None = None) -> list[FridaMessage]:
        """The buffered messages, oldest first (the newest `n` when given)."""
        return self._log.items(n)

    def tail(self, n: int = 20) -> str:
        """The newest `n` messages, one line each."""
        return "\n".join(message.line() for message in self._log.items(n))

    async def wait(
        self,
        predicate: Callable[[FridaMessage], bool] | None = None,
        *,
        timeout: float | None = 30.0,
    ) -> FridaMessage | None:
        """The next message (after the last one `wait` returned) that `predicate` accepts; None
        on timeout or once the script is gone."""
        message = await self._log.next(self._cursor, predicate or _any, timeout)
        if message is not None:
            self._cursor = message.seq + 1
        return message

    async def call(self, export: str, *args: Any) -> Any:
        """Call an `rpc.exports` function (snake_case names map to camelCase) off the loop."""
        return await asyncio.to_thread(getattr(self._script.exports_sync, export), *args)

    def post(self, message: Any, data: bytes | None = None) -> None:
        """Send a JSON message to the script's `recv()`."""
        self._script.post(message, data)

    async def unload(self) -> None:
        await asyncio.to_thread(self._script.unload)
        self.unloaded = True
        self._log.close()

    def __repr__(self) -> str:
        state = "unloaded" if self.unloaded else "loaded"
        return (
            f"<FridaScript {self.name or 'script'} {state} pid={self.session.pid} "
            f"messages={len(self._log)} dropped={self.dropped}: .tail(), .messages(n), await .wait()>"
        )


def _any(_item: object) -> bool:
    return True


class FridaSession:
    """A session on one process: load scripts into it, resume a spawned target, detach."""

    def __init__(
        self, frida: Any, device: Any, session: Any, pid: int, *, spawned: bool, pause: bool
    ) -> None:
        self.pid = pid
        self.device_id = str(device.id)
        self.spawned = spawned
        self._frida = frida
        self._device = device
        self._session = session
        self._pause = pause
        self._resumed = not spawned
        self._closed = False
        self._lock = threading.Lock()
        self.detached_reason: str | None = None
        self.scripts: list[FridaScript] = []
        session.on("detached", self._on_detached)
        _register(self)

    def _on_detached(self, reason: Any, crash: Any = None) -> None:
        self.detached_reason = str(reason)
        _unregister(self)
        for script in self.scripts:
            script._log.close()

    @property
    def detached(self) -> bool:
        return self.detached_reason is not None

    async def load(
        self, source: str, *, name: str | None = None, runtime: str | None = None
    ) -> FridaScript:
        """Create and load a script; a spawned target resumes after its first script unless the
        session was opened with `pause=True`."""

        def create() -> FridaScript:
            script = FridaScript(self, self._session.create_script(source, name=name, runtime=runtime), name)
            script._script.load()
            return script

        script = await asyncio.to_thread(create)
        self.scripts.append(script)
        note("script_load")
        if not self._pause:
            await asyncio.to_thread(self.resume_now)
        return script

    def resume_now(self) -> None:
        """Resume a spawned target once (a no-op for an attached process)."""
        with self._lock:
            if self._resumed:
                return
            self._resumed = True
        self._device.resume(self.pid)

    async def resume(self) -> None:
        await asyncio.to_thread(self.resume_now)

    def detach_now(self) -> None:
        """Detach (idempotent); a target this session spawned is killed."""
        with self._lock:
            if self._closed:
                return
            self._closed = True
        _unregister(self)
        try:
            if not self._session.is_detached:
                self._session.detach()
        finally:
            for script in self.scripts:
                script._log.close()
            if self.spawned:
                try:
                    self._device.kill(self.pid)
                except self._frida.ProcessNotFoundError:
                    pass  # The target already exited.

    async def detach(self) -> None:
        await asyncio.to_thread(self.detach_now)

    def __repr__(self) -> str:
        state = f"detached ({self.detached_reason})" if self.detached else "attached"
        origin = "spawned" if self.spawned else "attached"
        return (
            f"<FridaSession pid={self.pid} device={self.device_id} {origin} {state} "
            f"scripts={len(self.scripts)}>"
        )


_live: set[FridaSession] = set()
_live_lock = threading.Lock()
_atexit_installed = False


def _register(session: FridaSession) -> None:
    global _atexit_installed
    with _live_lock:
        _live.add(session)
        install = not _atexit_installed
        _atexit_installed = True
    if install:
        atexit.register(detach_all)


def _unregister(session: FridaSession) -> None:
    with _live_lock:
        _live.discard(session)


def live_sessions() -> list[FridaSession]:
    with _live_lock:
        return list(_live)


def detach_all() -> None:
    """Detach every live session (killing the targets they spawned); never raises, because it
    runs on the kernel's shutdown paths."""
    for session in live_sessions():
        try:
            session.detach_now()
        except BaseException as error:  # noqa: BLE001 - shutdown must reach every session
            import sys

            print(f"frida: detach of pid {session.pid} failed: {error}", file=sys.stderr)
