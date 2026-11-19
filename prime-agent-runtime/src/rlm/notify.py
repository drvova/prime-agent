"""Kernel client for a non-blocking "needs attention" or "done" ping.

``await notify("tests pass; ready for review", level="success")`` shows a notice
row in the user's terminal and asks the terminal for attention (a desktop
notification or the bell, as the user's settings allow). Unlike ``ask`` it never
waits for the user. With nobody attached it does nothing visible and returns
``False``.
"""

from __future__ import annotations

from . import host_request

__all__ = ["LEVELS", "notify"]

LEVELS = ("info", "success", "warning", "error")


async def notify(message: str, *, level: str = "info") -> bool:
    """Ping the user with ``message``; whether a user was attached to see it.

    Args:
        message: What happened or what needs attention (at most 500 characters).
        level: ``"info"``, ``"success"``, ``"warning"``, or ``"error"``.
    """
    if not isinstance(message, str) or not message.strip():
        raise ValueError("message must be a non-empty string")
    if level not in LEVELS:
        raise ValueError(f"level must be one of {', '.join(LEVELS)}, got {level!r}")
    reply = await host_request("notify.send", {"message": message, "level": level})
    if not isinstance(reply, dict):
        raise RuntimeError(f"notify returned an unexpected result: {reply!r}")
    return bool(reply.get("delivered"))
