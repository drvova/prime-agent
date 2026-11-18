"""Kernel client for the session todo list.

The list lives on the host (session state, persisted in the session file), so
it survives a kernel restart and a session reload, and the terminal UI shows it
live. ``await todo.set([...])`` creates or replaces it, ``await todo.update(id,
...)`` changes one item, and ``await todo.get()`` reads it back; each returns
the whole list.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["STATUSES", "TodoItem", "TodoList", "get", "set", "update"]

STATUSES = ("pending", "in_progress", "done")
_MARKS = {"pending": "[ ]", "in_progress": "[~]", "done": "[x]"}


@dataclass(frozen=True)
class TodoItem:
    """One item: a stable ``id``, its ``text``, ``status``, and an optional ``note``."""

    id: str
    text: str
    status: str
    note: str | None = None


@dataclass(frozen=True)
class TodoList:
    """The whole list. ``str(todos)`` renders it with a ``done/total`` header."""

    items: list[TodoItem]

    @property
    def done(self) -> int:
        return sum(1 for item in self.items if item.status == "done")

    @property
    def progress(self) -> str:
        """``"3/7"``: done items over all items."""
        return f"{self.done}/{len(self.items)}"

    def __str__(self) -> str:
        if not self.items:
            return "todo list: empty"
        lines = [f"todo list: {self.progress} done"]
        for item in self.items:
            line = f"{_MARKS.get(item.status, '[?]')} {item.id}. {item.text}"
            if item.note:
                line += f" ({item.note})"
            lines.append(line)
        return "\n".join(lines)


def _list_from_payload(payload: Any) -> TodoList:
    if not isinstance(payload, dict) or not isinstance(payload.get("items"), list):
        raise RuntimeError(f"todo request returned an unexpected result: {payload!r}")
    return TodoList(
        items=[
            TodoItem(
                id=str(item["id"]),
                text=str(item["text"]),
                status=str(item["status"]),
                note=item.get("note"),
            )
            for item in payload["items"]
            if isinstance(item, dict)
        ]
    )


def _check_status(status: Any) -> None:
    if status not in STATUSES:
        raise ValueError(f"status must be one of {', '.join(STATUSES)}, got {status!r}")


async def set(items: Sequence[str | Mapping[str, Any]]) -> TodoList:  # noqa: A001 - the API name
    """Create or replace the session todo list.

    Args:
        items: Each item is its text, or a mapping with ``text`` and optional
            ``id``, ``status`` (``"pending"``, ``"in_progress"``, ``"done"``;
            default ``"pending"``), and ``note``. Items without an ``id`` get
            the next free number (``"1"``, ``"2"``, ...). ``[]`` clears the list.
    """
    if isinstance(items, (str, bytes)) or not isinstance(items, Sequence):
        raise TypeError("items must be a list of strings or mappings")
    payload: list[dict[str, Any]] = []
    for item in items:
        if isinstance(item, str):
            payload.append({"text": item})
        elif isinstance(item, Mapping):
            unknown = sorted(str(key) for key in item if key not in ("id", "text", "status", "note"))
            if unknown:
                raise ValueError(f"unknown todo field(s): {', '.join(unknown)}")
            if "status" in item:
                _check_status(item["status"])
            payload.append(dict(item))
        else:
            raise TypeError(f"each todo item is a string or a mapping, got {type(item).__name__}")
    return _list_from_payload(await host_request("todo.set", {"items": payload}))


async def update(
    id: str,  # noqa: A002 - the item's id, as listed
    *,
    status: str | None = None,
    text: str | None = None,
    note: str | None = None,
) -> TodoList:
    """Change one item by id: its ``status``, ``text``, or ``note`` (``""`` clears the note)."""
    if not isinstance(id, str) or not id:
        raise TypeError("id must be a non-empty string")
    if status is None and text is None and note is None:
        raise ValueError("pass at least one of status, text, note")
    payload: dict[str, Any] = {"id": id}
    if status is not None:
        _check_status(status)
        payload["status"] = status
    if text is not None:
        payload["text"] = text
    if note is not None:
        payload["note"] = note
    return _list_from_payload(await host_request("todo.update", payload))


async def get() -> TodoList:
    """The current session todo list (empty when none was set)."""
    return _list_from_payload(await host_request("todo.get", {}))
