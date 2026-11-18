"""Kernel client for asking the user a question.

``await ask("Which database?", ["sqlite", "postgres"])`` shows the question in
the user's terminal and waits for the answer. The wait is a blocked host
request: no model tokens are spent while the user thinks, and an interrupt
withdraws the question. When nobody can answer (print or RPC mode, a daemon
session with no terminal attached, a subagent nobody watches) the call returns
at once with ``status == "no_user"`` instead of waiting.
"""

from __future__ import annotations

import os
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["AskResult", "ask"]

_STATUSES = ("answered", "declined", "timed_out", "no_user", "cancelled")


@dataclass(frozen=True)
class AskResult:
    """How a question settled.

    ``status`` is ``"answered"``, ``"declined"`` (the user dismissed it),
    ``"timed_out"`` (``answer`` is the default), ``"no_user"`` (nobody can
    answer; ``answer`` is the default), or ``"cancelled"`` (the session
    closed). ``answer`` is a string for free-text and single-choice questions,
    a list for multi-choice ones, and ``None`` when there is none.
    """

    status: str
    answer: str | list[str] | None

    @property
    def answered(self) -> bool:
        return self.status == "answered"

    def __str__(self) -> str:
        if self.answer is None:
            return self.status
        return f"{self.status}: {self.answer!r}"


def _strings(value: Any, what: str) -> list[str]:
    if isinstance(value, (str, bytes)) or not isinstance(value, Sequence):
        raise TypeError(f"{what} must be a list of strings")
    if not all(isinstance(item, str) for item in value):
        raise TypeError(f"{what} must be a list of strings")
    return list(value)


async def ask(
    question: str,
    choices: Sequence[str] | None = None,
    *,
    multi: bool = False,
    default: str | Sequence[str] | None = None,
    timeout: float | None = None,
) -> AskResult:
    """Ask the user ``question`` and wait for the answer.

    Args:
        question: What to ask; give the context the user needs to decide.
        choices: Options to pick from (at least two). ``None`` asks for free text.
        multi: Let the user pick any number of ``choices`` (the answer is a list).
        default: The preselected answer (a string; a list for ``multi``),
            returned as ``answer`` on a timeout or when nobody can answer.
        timeout: Seconds to wait before settling as ``"timed_out"``; ``None``
            waits until the user answers, declines, or the turn is interrupted.
    """
    if not isinstance(question, str) or not question.strip():
        raise TypeError("question must be a non-empty string")
    payload: dict[str, Any] = {"id": os.urandom(8).hex(), "text": question}
    if choices is None:
        if multi:
            raise ValueError("multi needs choices")
        payload["kind"] = "text"
    else:
        payload["kind"] = "multi" if multi else "single"
        payload["choices"] = _strings(choices, "choices")
    if default is not None:
        if multi:
            payload["default"] = _strings(default, "default")
        elif isinstance(default, str):
            payload["default"] = [default]
        else:
            raise TypeError("default must be a string (a list only with multi=True)")
    if timeout is not None:
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0:
            raise ValueError("timeout must be a positive number of seconds")
        payload["timeoutSeconds"] = max(1, round(timeout))
    try:
        reply = await host_request("question.ask", payload)
    except BaseException:
        # Interrupted (or the cell was cancelled) while the user had the question open:
        # withdraw it so the terminal stops showing it. No reply can be awaited any more.
        from . import repl

        if repl.is_active():
            repl.host_notify({"type": "question.cancel", "id": payload["id"]})
        raise
    status = reply.get("status") if isinstance(reply, dict) else None
    if status not in _STATUSES:
        raise RuntimeError(f"ask returned an unexpected result: {reply!r}")
    answer = [str(item) for item in reply.get("answer") or []]
    if multi:
        return AskResult(status=status, answer=answer if answer or status == "answered" else None)
    return AskResult(status=status, answer=answer[0] if answer else None)
