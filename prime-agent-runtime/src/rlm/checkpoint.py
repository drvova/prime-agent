"""Kernel client for workspace checkpoints.

``await checkpoint.create("before refactor")`` snapshots the session's
workspace (the shadow store ``/undo`` and rewind use; the project's own git
repository is never touched) and ``await checkpoint.list()`` lists this
session's checkpoints. ``await checkpoint.restore(id)`` puts the files back,
but only when the user turned on ``replCanRestoreCheckpoints``, only for a
checkpoint this session took, and only after the user confirms the listed
changes; it never restores without a person to ask.
"""

from __future__ import annotations

import builtins
import os
from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["Checkpoint", "RestoreResult", "create", "list", "restore"]

_RESTORE_STATUSES = ("restored", "declined", "no_user")


@dataclass(frozen=True)
class Checkpoint:
    """One checkpoint: its ``id``, ``label``, and ISO-8601 ``created_at``."""

    id: str
    label: str
    created_at: str


@dataclass(frozen=True)
class RestoreResult:
    """``status`` is ``restored``, ``declined`` (the user kept the files), or ``no_user``
    (nobody could confirm, so nothing changed); ``restored`` counts the paths written or removed."""

    status: str
    restored: int = 0

    @property
    def ok(self) -> bool:
        return self.status == "restored"


def _checkpoint(payload: Any) -> Checkpoint:
    if not isinstance(payload, dict) or not isinstance(payload.get("id"), str):
        raise RuntimeError(f"checkpoint request returned an unexpected checkpoint: {payload!r}")
    return Checkpoint(id=payload["id"], label=str(payload.get("label", "")), created_at=str(payload.get("created_at", "")))


async def create(label: str = "") -> Checkpoint:
    """Snapshot the workspace now; ``label`` names it in the list and the confirmation."""
    if not isinstance(label, str):
        raise TypeError("label must be a string")
    reply = await host_request("checkpoint.create", {"label": label})
    return _checkpoint(reply.get("checkpoint"))


async def list() -> builtins.list[Checkpoint]:  # noqa: A001 - the API name
    """This session's checkpoints, oldest first."""
    reply = await host_request("checkpoint.list", {})
    checkpoints = reply.get("checkpoints")
    if not isinstance(checkpoints, builtins.list):
        raise RuntimeError(f"checkpoint.list returned an unexpected result: {reply!r}")
    return [_checkpoint(entry) for entry in checkpoints]


async def restore(id: str) -> RestoreResult:  # noqa: A002 - the checkpoint's id, as listed
    """Put the workspace back to checkpoint ``id`` after the user confirms.

    Raises ``PermissionError`` while ``replCanRestoreCheckpoints`` is off.
    """
    if not isinstance(id, str) or not id:
        raise TypeError("id must be a non-empty string")
    payload = {"id": id, "question_id": os.urandom(8).hex()}
    try:
        reply = await host_request("checkpoint.restore", payload)
    except BaseException:
        # Interrupted while the user had the confirmation open: withdraw it.
        from . import repl

        if repl.is_active():
            repl.host_notify({"type": "question.cancel", "id": payload["question_id"]})
        raise
    status = reply.get("status") if isinstance(reply, dict) else None
    if status == "disabled":
        raise PermissionError(str(reply.get("message") or "checkpoint.restore is off"))
    if status not in _RESTORE_STATUSES:
        raise RuntimeError(f"checkpoint.restore returned an unexpected result: {reply!r}")
    return RestoreResult(status=status, restored=int(reply.get("restored") or 0))
