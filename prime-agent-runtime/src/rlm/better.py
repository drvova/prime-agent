"""Kernel client for the read-only prompt helper behind ``/better``.

``await better("fix the parser")`` returns the deeper version of a prompt a top
professional would ask for, grounded in what the helper reads from the
repository (it only reads: the host refuses every edit and write).
``mode="brief"`` checks a delegation brief against the repository from ``cwd``
instead: every path, name, and command it mentions must exist (or run
read-only); what is wrong is fixed, and a brief that needs nothing comes back
unchanged. ``rlm.spawn(..., better=True)`` runs brief mode before it spawns.
"""

from __future__ import annotations

from . import host_request

__all__ = ["MODES", "better"]

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
