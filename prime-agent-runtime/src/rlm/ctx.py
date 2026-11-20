"""Kernel client for paged-out output: read a cut cell output by its handle.

When a cell's output runs past the transcript cap, the host keeps the whole
text in the session's spill store and the transcript shows a handle
(``[out:7f3a9c1e04 · 48,211 chars · 1,204 lines · 31,002 chars not shown]``).
``ctx`` reads it back by that handle instead of re-running the command:
``await ctx.page(h, start=1200)``, ``await ctx.grep(h, "error")``,
``await ctx.tail(h)``, ``await ctx.info(h)``. Line numbers are 0-based page
offsets. Every reply is bounded (about 16k characters), so a read never spills
again; page on with ``start=page.end``.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["Info", "Matches", "Page", "PagedOutUnavailable", "grep", "info", "page", "tail"]

_STATE_TEXT = {
    "evicted": "was evicted from this session's spill store (it keeps the newest 64 MiB); re-run the command to see it again",
    "pending": "is still being written; read it again in a moment",
    "unknown": "names no output stored in this session",
    "unavailable": "cannot be read: this session keeps no paged-out output (it is not saved)",
}


class PagedOutUnavailable(LookupError):
    """The handle has no stored text: evicted, never stored, still being written, or unknown."""

    def __init__(self, handle: str, state: str, reason: str | None) -> None:
        self.handle = handle
        self.state = state
        self.reason = reason
        super().__init__(_describe(handle, state, reason))


def _describe(handle: str, state: str, reason: str | None) -> str:
    if state == "not_stored":
        return f"{handle} was not stored ({reason or 'no reason given'}); re-run the command to see it"
    return f"{handle} {_STATE_TEXT.get(state, f'is {state}')}"


@dataclass(frozen=True)
class Info:
    """One handle's state. ``state`` is ``"stored"`` (then the counts are set), ``"evicted"``,
    ``"not_stored"`` (``reason`` says why), ``"pending"``, ``"unknown"``, or ``"unavailable"``."""

    handle: str
    state: str
    total_chars: int | None = None
    lines: int | None = None
    stored_lines: int | None = None
    reason: str | None = None

    def __str__(self) -> str:
        if self.state != "stored":
            return _describe(self.handle, self.state, self.reason)
        partial = "" if self.stored_lines == self.lines else f" ({self.stored_lines} lines stored)"
        return f"{self.handle}: {self.total_chars} chars, {self.lines} lines{partial}"


@dataclass(frozen=True)
class Page:
    """Lines ``start`` to ``end`` (exclusive) of the output. ``truncated``: the slice cap cut the
    page short; read on from ``end``."""

    handle: str
    start: int
    end: int
    lines: int
    text: str
    truncated: bool

    def _repl_text_(self) -> str:
        more = "; slice cap reached, continue at start=" + str(self.end) if self.truncated else ""
        return f"[{self.handle} lines {self.start}..{self.end} of {self.lines}{more}]\n{self.text}"

    def __str__(self) -> str:
        return self._repl_text_()


@dataclass(frozen=True)
class Matches:
    """``ctx.grep``'s reply: ``N:line`` for a match, ``N-line`` for context, ``--`` between
    groups (``N`` the line ``ctx.page`` starts at). ``count`` is every matching line; ``shown``
    those in ``text``."""

    handle: str
    pattern: str
    count: int
    shown: int
    text: str
    truncated: bool

    def _repl_text_(self) -> str:
        more = " (more not shown: narrow the pattern or page from a line)" if self.shown < self.count else ""
        return f"[{self.handle} grep {self.pattern!r}: {self.count} matching lines, {self.shown} shown{more}]\n{self.text}"

    def __str__(self) -> str:
        return self._repl_text_()


def _check_handle(handle: Any) -> str:
    if not isinstance(handle, str):
        raise TypeError(f"handle must be a str like 'out:7f3a9c1e04', got {type(handle).__name__}")
    return handle


def _count(name: str, value: Any) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{name} must be a non-negative int, got {value!r}")
    return value


async def _read(request: str, payload: dict[str, Any]) -> dict[str, Any]:
    reply = await host_request(request, payload)
    if not isinstance(reply, dict) or not isinstance(reply.get("state"), str):
        raise RuntimeError(f"{request} returned an unexpected result: {reply!r}")
    return reply


def _stored(reply: dict[str, Any]) -> dict[str, Any]:
    if reply["state"] != "stored":
        raise PagedOutUnavailable(str(reply.get("handle", "")), reply["state"], reply.get("reason"))
    return reply


def _page(reply: dict[str, Any]) -> Page:
    reply = _stored(reply)
    return Page(
        handle=reply["handle"],
        start=int(reply["start"]),
        end=int(reply["end"]),
        lines=int(reply["storedLines"]),
        text=str(reply["text"]),
        truncated=bool(reply["truncated"]),
    )


async def page(handle: str, *, start: int = 0, lines: int = 200) -> Page:
    """Lines ``start..start+lines`` of a paged-out output (at most 2000 lines and the slice cap).

    Raises:
        PagedOutUnavailable: the handle has no stored text (evicted, not stored, unknown).
    """
    payload = {"handle": _check_handle(handle), "start": _count("start", start), "lines": _count("lines", lines)}
    return _page(await _read("ctx.page", payload))


async def tail(handle: str, n: int = 200) -> Page:
    """The last ``n`` lines of a paged-out output (within the slice cap)."""
    return _page(await _read("ctx.tail", {"handle": _check_handle(handle), "n": _count("n", n)}))


async def grep(handle: str, pattern: str, *, context: int = 2, max_matches: int = 50) -> Matches:
    """Lines of a paged-out output matching the regex ``pattern``, with ``context`` lines around
    each (at most 20) and at most ``max_matches`` matches (at most 500)."""
    if not isinstance(pattern, str):
        raise TypeError(f"pattern must be a str, got {type(pattern).__name__}")
    payload = {
        "handle": _check_handle(handle),
        "pattern": pattern,
        "context": _count("context", context),
        "max_matches": _count("max_matches", max_matches),
    }
    reply = _stored(await _read("ctx.grep", payload))
    return Matches(
        handle=reply["handle"],
        pattern=pattern,
        count=int(reply["matches"]),
        shown=int(reply["shown"]),
        text=str(reply["text"]),
        truncated=bool(reply["truncated"]),
    )


async def info(handle: str) -> Info:
    """A handle's state and counts; never raises for a handle without stored text."""
    reply = await _read("ctx.info", {"handle": _check_handle(handle)})
    stored = reply["state"] == "stored"
    return Info(
        handle=str(reply.get("handle", handle)),
        state=reply["state"],
        total_chars=int(reply["totalChars"]) if stored else None,
        lines=int(reply["lines"]) if stored else None,
        stored_lines=int(reply["storedLines"]) if stored else None,
        reason=reply.get("reason"),
    )
