"""Kernel client for the read-only prompt helper behind ``/better``.

``await better("fix the parser")`` returns the deeper version of a prompt a top
professional would ask for, grounded in what the helper reads from the
repository (it only reads: the host refuses every edit and write).
``mode="brief"`` checks a delegation brief against the repository from ``cwd``
instead: every path, name, and command it mentions must exist (or run
read-only); what is wrong is fixed, and a brief that needs nothing comes back
unchanged. ``rlm.spawn(..., better=True)`` runs brief mode before it spawns.

A rewrite or brief ends with an ``<acceptance>`` block of executable checks
(``run: <command>``, ``exists: <path>``, ``grep: <path> <text>``). A prompt that
carries one arms its checks: the host runs them in the background when the turn
ends, until they pass or three rounds have run, and a failed round arrives as a
notice. ``better.checks()`` reads where they stand and ``better.evidence(handle)``
one round's per-check results.
"""

from __future__ import annotations

from typing import Any

from . import host_request

__all__ = ["MODES", "better", "checks", "evidence"]

MODES = ("deepen", "brief")


async def better(task: str, *, cwd: str | None = None, mode: str = "deepen") -> str:
    """The improved ``task`` (``task`` itself when brief mode finds nothing to fix).

    Args:
        task: The prompt or delegation brief.
        cwd: Where the helper reads (relative to the session cwd); the session cwd by default.
        mode: ``"deepen"`` (the deeper prompt) or ``"brief"`` (check and fix a brief).
    """
    if not isinstance(task, str) or not task.strip():
        raise ValueError("task must be a non-empty string")
    if cwd is not None and not isinstance(cwd, str):
        raise TypeError(f"cwd must be str or None, got {type(cwd).__name__}")
    if mode not in MODES:
        raise ValueError(f"mode must be one of {', '.join(MODES)}, got {mode!r}")
    payload: dict[str, object] = {"task": task, "mode": mode}
    if cwd is not None:
        payload["cwd"] = cwd
    reply = await host_request("better.run", payload)
    if not isinstance(reply, dict) or not isinstance(reply.get("rewrite"), str):
        raise RuntimeError(f"better returned an unexpected result: {reply!r}")
    return reply["rewrite"]


async def checks() -> dict[str, Any]:
    """Where the armed acceptance checks stand.

    Returns ``{status, checks, rounds, max_rounds, running, last}``: ``status`` is
    ``"none"``, ``"pending"``, ``"passed"``, or ``"failed"``; ``last`` is the latest
    round's evidence (as :func:`evidence` returns it) or ``None``.
    """
    reply = await host_request("better.checks", {})
    if not isinstance(reply, dict) or not isinstance(reply.get("status"), str):
        raise RuntimeError(f"better.checks returned an unexpected result: {reply!r}")
    return reply


async def evidence(handle: str) -> dict[str, Any]:
    """One check round's results by its handle (``out:<id>``).

    Returns ``{handle, round, passed, failed, checks}``; each check is
    ``{kind, check, passed, reason}`` with the raw failure reason.
    """
    if not isinstance(handle, str) or not handle:
        raise ValueError("handle must be a non-empty string")
    reply = await host_request("better.evidence", {"handle": handle})
    if not isinstance(reply, dict) or not isinstance(reply.get("checks"), list):
        raise RuntimeError(f"better.evidence returned an unexpected result: {reply!r}")
    return reply


better.checks = checks  # type: ignore[attr-defined]
better.evidence = evidence  # type: ignore[attr-defined]
