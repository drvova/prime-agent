"""`frida.trace` and `frida.discover`: frida-tools' own `Tracer` and `Discoverer`, driven
programmatically instead of through their console applications.

Both classes are importable and take a session, a UI object, and (for the Tracer) a `Reactor`,
so the kernel reuses them rather than generating its own hook scripts: their agents, handler
stubs, and result decoding stay frida-tools' and move with the pinned version. The UI objects
here only collect: trace events go to a bounded log the model polls, never to a cell's output.
"""

from __future__ import annotations

import asyncio
import threading
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from ._frida_session import BoundedLog, FridaSession


@dataclass(frozen=True)
class FridaTraceEvent:
    """One traced call: the handler's log line `message`, `ms` since the trace started, the
    thread, the call depth, and the traced `function`."""

    seq: int
    ms: int
    thread_id: int
    depth: int
    function: str
    message: str

    def line(self) -> str:
        return f"{self.ms:6d} ms [tid {self.thread_id}] {'   | ' * self.depth}{self.message}"


class FridaTrace:
    """A running trace: its events, the warnings the tracer raised, and its stop."""

    def __init__(self, session: FridaSession) -> None:
        self.session = session
        self.functions = 0
        self.warnings: list[str] = []
        self._log: BoundedLog[FridaTraceEvent] = BoundedLog()
        self._cursor = 0
        self._names: dict[int, str] = {}
        self._stopping = threading.Event()
        self._lock = threading.Lock()
        self._tracer: Any = None
        self._timer: threading.Timer | None = None

    @classmethod
    def start(
        cls,
        tracer: Any,
        reactor: Any,
        session: FridaSession,
        profile: Any,
        duration_s: float | None,
    ) -> FridaTrace:
        """Start tracing (blocking: run it off the loop). A spawned target resumes once the
        tracer's agent is initialized, as `frida-trace -f` does."""
        handle = cls(session)
        loop = reactor.Reactor(lambda _reactor: handle._stopping.wait())
        threading.Thread(target=loop.run, name="frida-trace-reactor", daemon=True).start()
        session._session.on("detached", lambda *_args: handle._finish())

        class CollectingUI(tracer.UI):  # type: ignore[misc, name-defined]
            def on_trace_progress(self, status: str, *params: Any) -> None:
                if status == "initialized":
                    session.resume_now()
                elif status == "started":
                    handle.functions = int(params[0])

            def on_trace_warning(self, message: str) -> None:
                handle.warnings.append(message)

            def on_trace_error(self, message: str) -> None:
                handle.warnings.append(f"error: {message}")

            def on_trace_events(self, events: list[Any]) -> None:
                for target_id, timestamp, thread_id, depth, _caller, _backtrace, message in events:
                    function = handle._names.get(target_id, str(target_id))
                    handle._log.append(
                        lambda seq, ms=timestamp, tid=thread_id, level=depth, name=function, text=message: (
                            FridaTraceEvent(seq, int(ms), int(tid), int(level), name, str(text))
                        )
                    )

            def on_trace_handler_create(self, target: Any, handler: str, source: Any) -> None:
                handle._names[target.identifier] = target.display_name

            on_trace_handler_load = on_trace_handler_create

            def try_handle_bridge_request(self, message: Any, script: Any) -> bool:
                return False

        handle._tracer = tracer.Tracer(
            loop, tracer.MemoryRepository(), profile, [], log_handler=handle._on_log
        )
        try:
            handle._tracer.start_trace(
                session._session, "early" if session.spawned else "late", {}, None, CollectingUI()
            )
        except BaseException:
            handle._finish()
            raise
        if duration_s is not None:
            handle._timer = threading.Timer(duration_s, handle.stop_now)
            handle._timer.daemon = True
            handle._timer.start()
        return handle

    def _on_log(self, level: str, text: str) -> None:
        self.warnings.append(f"{level}: {text}")

    def _finish(self) -> None:
        """End the reactor and the log (the session detached, or the start failed)."""
        self._stopping.set()
        self._log.close()

    @property
    def running(self) -> bool:
        return not self._log.closed

    @property
    def dropped(self) -> int:
        return self._log.dropped

    def events(self, n: int | None = None) -> list[FridaTraceEvent]:
        """The buffered events, oldest first (the newest `n` when given)."""
        return self._log.items(n)

    def tail(self, n: int = 20) -> str:
        """The newest `n` events, one line each, indented by call depth."""
        return "\n".join(event.line() for event in self._log.items(n))

    async def wait(
        self,
        predicate: Callable[[FridaTraceEvent], bool] | None = None,
        *,
        timeout: float | None = 30.0,
    ) -> FridaTraceEvent | None:
        """The next event (after the last one `wait` returned) that `predicate` accepts; None on
        timeout or once the trace ended."""
        event = await self._log.next(self._cursor, predicate or (lambda _event: True), timeout)
        if event is not None:
            self._cursor = event.seq + 1
        return event

    def stop_now(self) -> None:
        """Stop tracing and detach (idempotent; kills a target the trace spawned)."""
        with self._lock:
            tracer, self._tracer = self._tracer, None
        if self._timer is not None:
            self._timer.cancel()
        try:
            if tracer is not None:
                tracer.stop()
        finally:
            self._finish()
            self.session.detach_now()

    async def stop(self) -> None:
        await asyncio.to_thread(self.stop_now)

    def __repr__(self) -> str:
        state = "running" if self.running else "stopped"
        return (
            f"<FridaTrace pid={self.session.pid} {state} functions={self.functions} "
            f"events={len(self._log)} dropped={self.dropped} warnings={len(self.warnings)}: "
            ".tail(), .events(n), await .wait(), await .stop()>"
        )


@dataclass(frozen=True)
class FridaHotFunction:
    """A function the sampled threads called: its module (None for dynamic code), name, absolute
    address, and call count."""

    module: str | None
    name: str
    address: int
    calls: int


@dataclass(frozen=True)
class FridaDiscovery:
    """What `frida.discover` sampled: the threads followed, and the functions they called, the
    most called first."""

    threads: int
    functions: list[FridaHotFunction]

    def __str__(self) -> str:
        rows = [f"{self.threads} threads, {len(self.functions)} functions"]
        rows += [
            f"{hot.calls:10d}  {hot.module + '!' if hot.module else ''}{hot.name}  {hot.address:#x}"
            for hot in self.functions[:50]
        ]
        return "\n".join(rows)

    @classmethod
    async def sample(
        cls, discoverer: Any, reactor: Any, session: FridaSession, duration_s: float
    ) -> FridaDiscovery:
        """Follow every thread for `duration_s` (the caller detaches the session). A spawned
        target resumes first: its suspended main thread is not yet enumerable, so a sample started
        before the resume would follow no thread."""
        collected: dict[str, Any] = {}

        class CollectingUI(discoverer.UI):  # type: ignore[misc, name-defined]
            def on_sample_start(self, total: int) -> None:
                collected["threads"] = total

            def on_sample_result(self, module_functions: Any, dynamic_functions: Any) -> None:
                collected["module"] = module_functions
                collected["dynamic"] = dynamic_functions

        # The Discoverer keeps the reactor it is given but never schedules on it.
        sampler = discoverer.Discoverer(reactor.Reactor(lambda _reactor: None))
        await asyncio.to_thread(session.resume_now)
        try:
            await asyncio.to_thread(sampler.start, session._session, None, CollectingUI())
            await asyncio.sleep(duration_s)
            await asyncio.to_thread(sampler.stop)
        finally:
            await asyncio.to_thread(sampler.dispose)
        functions = [
            FridaHotFunction(module.name, function.name, function.absolute_address, count)
            for module, calls in collected["module"].items()
            for function, count in calls
        ] + [
            FridaHotFunction(None, function.name, function.absolute_address, count)
            for function, count in collected["dynamic"]
        ]
        functions.sort(key=lambda hot: hot.calls, reverse=True)
        return cls(collected["threads"], functions)
