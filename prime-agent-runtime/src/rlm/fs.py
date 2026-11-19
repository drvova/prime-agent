"""Kernel client for Prime Agent's native, in-process file search and
anchored (hashline) editing.

``await fs.grep(pattern)``, ``await fs.glob(pattern)`` and ``await fs.find()``
send the ``fs.grep``/``fs.glob``/``fs.find`` host requests. The host runs the
ripgrep library crates in process (regex search, ``.gitignore``-aware walks,
globs, file types), so they behave the same on Linux, macOS and Windows and
need no ``rg``, ``grep`` or ``find`` installed. Paths resolve here, against the
REPL's working directory, and results print them relative to it with ``/``.

``await fs.read(path)`` returns the file's lines tagged with anchors
(``12:abc|text``: the line number and a hash of the line and its
neighbours); ``await fs.edit(path, edits)`` edits by pointing at anchors
instead of retyping old text. If any anchor no longer matches the file, the
whole patch is rejected (``StaleAnchorsError``) and nothing is written.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any, Literal

from . import host_request

__all__ = [
    "AnchoredLine",
    "EditResult",
    "FileView",
    "FsEntry",
    "FsListing",
    "GrepMatch",
    "GrepResult",
    "StaleAnchor",
    "StaleAnchorsError",
    "edit",
    "find",
    "glob",
    "grep",
    "read",
]

_CASES = ("sensitive", "insensitive", "smart")
_KINDS = ("file", "dir", "symlink")
_ANCHORED_OPS = ("replace", "delete", "insert_after", "insert_before")


@dataclass(frozen=True)
class GrepMatch:
    """One match: 1-based ``line`` and ``column``; ``text`` is the matched
    line (clipped around the match); ``before``/``after`` are context lines."""

    path: str
    line: int
    column: int
    text: str
    before: list[str] = field(default_factory=list)
    after: list[str] = field(default_factory=list)
    extra_lines: int = 0


@dataclass(frozen=True)
class GrepResult:
    """A finished search. ``str(result)`` is the ripgrep-style rendering."""

    matches: list[GrepMatch]
    files_searched: int
    files_matched: int
    truncated: bool
    unreadable: int = 0
    text: str = ""

    def __str__(self) -> str:
        return self.text

    def __iter__(self):
        return iter(self.matches)

    def __len__(self) -> int:
        return len(self.matches)


@dataclass(frozen=True)
class FsEntry:
    """A listed path: ``kind`` is ``file``, ``dir`` or ``symlink``."""

    path: str
    kind: str
    size: int | None = None


@dataclass(frozen=True)
class FsListing:
    """A finished ``glob``/``find``. ``str(listing)`` is one path per line."""

    entries: list[FsEntry]
    truncated: bool
    text: str = ""

    @property
    def paths(self) -> list[str]:
        return [entry.path for entry in self.entries]

    def __str__(self) -> str:
        return self.text

    def __iter__(self):
        return iter(self.entries)

    def __len__(self) -> int:
        return len(self.entries)


def _roots(path: Any) -> dict[str, str]:
    if not isinstance(path, (str, os.PathLike)) or not os.fspath(path):
        raise TypeError("path must be a non-empty str or os.PathLike")
    return {
        "root": os.path.abspath(os.path.expanduser(os.fspath(path))),
        "cwd": os.getcwd(),
    }


def _count(name: str, value: Any, *, minimum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ValueError(f"{name} must be an integer >= {minimum}")
    return value


def _strings(name: str, value: Any) -> list[str] | None:
    if value is None:
        return None
    if isinstance(value, str):
        return [value]
    if isinstance(value, (list, tuple)) and all(isinstance(item, str) for item in value):
        return list(value)
    raise TypeError(f"{name} must be a str, a list of str, or None")


def _listing(payload: Any, operation: str) -> FsListing:
    if not isinstance(payload, dict):
        raise RuntimeError(f"{operation} returned a non-object result: {payload!r}")
    return FsListing(
        entries=[
            FsEntry(path=str(item.get("path") or ""), kind=str(item.get("kind") or ""), size=item.get("size"))
            for item in payload.get("entries") or []
            if isinstance(item, dict)
        ],
        truncated=bool(payload.get("truncated")),
        text=str(payload.get("text") or ""),
    )


async def grep(
    pattern: str,
    path: str | os.PathLike[str] = ".",
    *,
    glob: str | list[str] | None = None,
    type: str | list[str] | None = None,
    case: Literal["sensitive", "insensitive", "smart"] = "sensitive",
    literal: bool = False,
    word: bool = False,
    multiline: bool = False,
    context: int = 0,
    before: int | None = None,
    after: int | None = None,
    hidden: bool = False,
    gitignore: bool = True,
    max_results: int = 100,
    max_per_file: int | None = None,
    max_columns: int = 300,
) -> GrepResult:
    """Search file contents with ripgrep semantics, in process.

    Args:
        pattern: A regex (Rust ``regex`` syntax), or a plain string with
            ``literal=True``.
        path: A directory to walk or one file (relative to the REPL's cwd).
        glob: ripgrep ``-g`` globs: ``"*.rs"`` matches at any depth,
            ``"!target/**"`` excludes.
        type: ripgrep ``-t`` file types (``"rust"``, ``"py"``, ``"ts"``...).
        case: ``"sensitive"``, ``"insensitive"`` or ``"smart"``.
        word: whole words only.
        multiline: let the pattern span lines (``.`` matches newlines).
        context: lines of context on both sides; ``before``/``after`` override.
        hidden: also search hidden files (``.git`` is never searched).
        gitignore: respect ``.gitignore``/``.ignore``/git excludes.
        max_results: matches returned at most; ``result.truncated`` says more exist.
        max_per_file: matches kept per file at most.
        max_columns: characters shown of one line (clipped around the match).
    """
    if not isinstance(pattern, str) or not pattern:
        raise TypeError("pattern must be a non-empty string")
    if case not in _CASES:
        raise ValueError(f"case must be one of {', '.join(_CASES)}")
    context = _count("context", context, minimum=0)
    payload: dict[str, Any] = {
        "pattern": pattern,
        **_roots(path),
        "case": case,
        "literal": bool(literal),
        "word": bool(word),
        "multiline": bool(multiline),
        "before": context if before is None else _count("before", before, minimum=0),
        "after": context if after is None else _count("after", after, minimum=0),
        "hidden": bool(hidden),
        "gitignore": bool(gitignore),
        "maxResults": _count("max_results", max_results, minimum=1),
        "maxColumns": _count("max_columns", max_columns, minimum=20),
    }
    globs = _strings("glob", glob)
    if globs:
        payload["globs"] = globs
    types = _strings("type", type)
    if types:
        payload["types"] = types
    if max_per_file is not None:
        payload["maxPerFile"] = _count("max_per_file", max_per_file, minimum=1)
    reply = await host_request("fs.grep", payload)
    if not isinstance(reply, dict):
        raise RuntimeError(f"fs.grep returned a non-object result: {reply!r}")
    return GrepResult(
        matches=[
            GrepMatch(
                path=str(item.get("path") or ""),
                line=int(item.get("line") or 0),
                column=int(item.get("column") or 0),
                text=str(item.get("text") or ""),
                before=[str(line) for line in item.get("before") or []],
                after=[str(line) for line in item.get("after") or []],
                extra_lines=int(item.get("extraLines") or 0),
            )
            for item in reply.get("matches") or []
            if isinstance(item, dict)
        ],
        files_searched=int(reply.get("filesSearched") or 0),
        files_matched=int(reply.get("filesMatched") or 0),
        truncated=bool(reply.get("truncated")),
        unreadable=int(reply.get("unreadable") or 0),
        text=str(reply.get("text") or ""),
    )


async def glob(
    pattern: str,
    path: str | os.PathLike[str] = ".",
    *,
    hidden: bool = False,
    gitignore: bool = True,
    max_results: int = 500,
) -> FsListing:
    """List paths under ``path`` matching a glob, ``.gitignore``-aware.

    ``*`` stays inside one directory and ``**`` crosses them (``"**/*.rs"``,
    ``"src/*/mod.rs"``), as in ``pathlib``. Sorted by path.
    """
    if not isinstance(pattern, str) or not pattern:
        raise TypeError("pattern must be a non-empty string")
    payload = {
        "pattern": pattern,
        **_roots(path),
        "hidden": bool(hidden),
        "gitignore": bool(gitignore),
        "maxResults": _count("max_results", max_results, minimum=1),
    }
    return _listing(await host_request("fs.glob", payload), "fs.glob")


async def find(
    name: str | None = None,
    path: str | os.PathLike[str] = ".",
    *,
    kind: Literal["file", "dir", "symlink"] | None = None,
    ignore_case: bool = False,
    max_depth: int | None = None,
    hidden: bool = False,
    gitignore: bool = True,
    max_results: int = 500,
) -> FsListing:
    """Find files and directories by name, like ``find -name`` / ``fd``.

    Args:
        name: A glob on the file name (``"Cargo.toml"``, ``"*.md"``); None
            matches every entry.
        kind: ``"file"``, ``"dir"``, or ``"symlink"``.
        max_depth: levels below ``path`` (1 = its direct children).
    """
    if name is not None and (not isinstance(name, str) or not name):
        raise TypeError("name must be a non-empty string or None")
    if kind is not None and kind not in _KINDS:
        raise ValueError(f"kind must be one of {', '.join(_KINDS)}")
    payload: dict[str, Any] = {
        **_roots(path),
        "ignoreCase": bool(ignore_case),
        "hidden": bool(hidden),
        "gitignore": bool(gitignore),
        "maxResults": _count("max_results", max_results, minimum=1),
    }
    if name is not None:
        payload["name"] = name
    if kind is not None:
        payload["kind"] = kind
    if max_depth is not None:
        payload["maxDepth"] = _count("max_depth", max_depth, minimum=0)
    return _listing(await host_request("fs.find", payload), "fs.find")


@dataclass(frozen=True)
class AnchoredLine:
    """One line: 1-based ``line``, its ``anchor`` (``"12:abc"``), and ``text``."""

    line: int
    anchor: str
    text: str


@dataclass(frozen=True)
class FileView:
    """A finished ``fs.read``. ``str(view)`` is ``LINE:abc|text`` per line."""

    path: str
    lines: list[AnchoredLine]
    total_lines: int
    start: int
    end: int
    truncated: bool
    line_ends: str = "lf"
    encoding: str = "utf-8"
    final_newline: bool = True
    text: str = ""

    def anchor(self, line: int) -> str:
        """The anchor of 1-based ``line`` (which this view must show)."""
        for row in self.lines:
            if row.line == line:
                return row.anchor
        raise KeyError(f"line {line} is not in this view (lines {self.start}-{self.end})")

    def __str__(self) -> str:
        return self.text

    def __iter__(self):
        return iter(self.lines)

    def __len__(self) -> int:
        return len(self.lines)


@dataclass(frozen=True)
class EditResult:
    """An applied ``fs.edit``. ``hunks`` hold the changed regions with fresh
    anchors (plus one line of context each side), so the next edit can chain
    without a re-read; ``str(result)`` renders them."""

    path: str
    edits: int
    removed: int
    added: int
    total_lines: int
    hunks: list[list[AnchoredLine]]
    text: str = ""

    def __str__(self) -> str:
        return self.text


@dataclass(frozen=True)
class StaleAnchor:
    """An anchor of edit number ``edit`` (1-based) that no longer matches:
    ``current`` is the anchor its line has now, None past the file's end."""

    edit: int
    anchor: str
    current: str | None


class StaleAnchorsError(RuntimeError):
    """``fs.edit`` rejected the whole patch and wrote nothing: ``stale`` lists
    the anchors that no longer match, ``lines`` the current lines around them
    with fresh anchors, and ``str(error)`` is the report."""

    def __init__(self, text: str, stale: list[StaleAnchor], lines: list[AnchoredLine]) -> None:
        super().__init__(text)
        self.stale = stale
        self.lines = lines


def _target(path: Any) -> dict[str, str]:
    if not isinstance(path, (str, os.PathLike)) or not os.fspath(path):
        raise TypeError("path must be a non-empty str or os.PathLike")
    return {"path": os.path.abspath(os.path.expanduser(os.fspath(path))), "cwd": os.getcwd()}


def _rows(items: Any) -> list[AnchoredLine]:
    return [
        AnchoredLine(line=int(item.get("line") or 0), anchor=str(item.get("anchor") or ""), text=str(item.get("text") or ""))
        for item in items or []
        if isinstance(item, dict)
    ]


async def read(path: str | os.PathLike[str], *, start: int | None = None, end: int | None = None) -> FileView:
    """Read a text file with every line tagged by an anchor.

    ``str(view)`` renders ``LINE:abc|text`` rows under a header (``path (lines
    1-40 of 120)``, plus ``crlf``/``no final newline``/encoding notes). Pass
    anchors from it to ``fs.edit``. ``start``/``end`` are 1-based and
    inclusive; at most 2,000 lines are returned unless ``end`` asks for more
    (``view.truncated`` says lines were left out). Binary files and files in
    no UTF-8/UTF-16 encoding raise ``RuntimeError``.
    """
    payload: dict[str, Any] = _target(path)
    if start is not None:
        payload["start"] = _count("start", start, minimum=1)
    if end is not None:
        payload["end"] = _count("end", end, minimum=1)
    reply = await host_request("fs.read", payload)
    if not isinstance(reply, dict):
        raise RuntimeError(f"fs.read returned a non-object result: {reply!r}")
    return FileView(
        path=str(reply.get("path") or ""),
        lines=_rows(reply.get("lines")),
        total_lines=int(reply.get("totalLines") or 0),
        start=int(reply.get("start") or 0),
        end=int(reply.get("end") or 0),
        truncated=bool(reply.get("truncated")),
        line_ends=str(reply.get("lineEnds") or ""),
        encoding=str(reply.get("encoding") or ""),
        final_newline=bool(reply.get("finalNewline")),
        text=str(reply.get("text") or ""),
    )


def _anchor(value: Any, number: int) -> str:
    if isinstance(value, AnchoredLine):
        return value.anchor
    if isinstance(value, str) and value:
        return value
    raise TypeError(f"edit {number}: an anchor is a str like '12:abc' (or an AnchoredLine)")


def _wire_edit(edit: Any, number: int) -> dict[str, Any]:
    if not isinstance(edit, dict):
        raise TypeError(f"edit {number} must be a dict like {{'replace': '12:abc', 'text': '...'}}")
    ops = [op for op in (*_ANCHORED_OPS, "append") if op in edit]
    if len(ops) != 1:
        raise ValueError(
            f"edit {number} needs exactly one of replace, delete, insert_after, insert_before, append"
        )
    op = ops[0]
    unknown = set(edit) - {op, "text"}
    if unknown:
        raise ValueError(f"edit {number}: unknown keys {sorted(unknown)}")
    wire: dict[str, Any] = {"op": op}
    if op == "append":
        wire["text"] = edit["append"]
        return wire
    where = edit[op]
    if op in ("replace", "delete") and isinstance(where, (list, tuple)):
        if len(where) != 2:
            raise ValueError(f"edit {number}: a range is [first_anchor, last_anchor]")
        wire["anchor"] = _anchor(where[0], number)
        wire["end"] = _anchor(where[1], number)
    else:
        wire["anchor"] = _anchor(where, number)
    if "text" in edit:
        text = edit["text"]
        if not isinstance(text, (str, list, tuple)) or (not isinstance(text, str) and not all(isinstance(line, str) for line in text)):
            raise TypeError(f"edit {number}: text must be a str or a list of str")
        wire["text"] = text if isinstance(text, str) else list(text)
    return wire


async def edit(path: str | os.PathLike[str], edits: dict[str, Any] | list[dict[str, Any]]) -> EditResult:
    """Apply anchored edits to a text file, all or nothing.

    Each edit is a dict naming one operation and its anchor(s) from
    ``fs.read`` (or a previous ``fs.edit`` result):

    - ``{"replace": "12:abc", "text": "new line"}``; a range:
      ``{"replace": ["12:abc", "15:xyz"], "text": "..."}`` (inclusive);
    - ``{"delete": "12:abc"}`` or ``{"delete": ["12:abc", "15:xyz"]}``;
    - ``{"insert_after": "12:abc", "text": "..."}``, ``{"insert_before": ...}``;
    - ``{"append": "text"}`` adds lines at the end (also fills an empty file).

    ``text`` is the new lines (a str, one trailing newline ignored, or a list
    of str); the file keeps its own line endings, final newline and encoding.
    All anchors address the file as read: line numbers never shift between
    the edits of one patch, and no two edits may touch the same lines. If any
    anchor no longer matches, raises ``StaleAnchorsError`` and writes nothing.
    The write is atomic (temp file + rename).
    """
    items = [edits] if isinstance(edits, dict) else edits
    if not isinstance(items, (list, tuple)) or not items:
        raise TypeError("edits must be a non-empty list of edit dicts (or one dict)")
    payload = {**_target(path), "edits": [_wire_edit(item, number) for number, item in enumerate(items, 1)]}
    reply = await host_request("fs.edit", payload)
    if not isinstance(reply, dict):
        raise RuntimeError(f"fs.edit returned a non-object result: {reply!r}")
    if not reply.get("applied"):
        raise StaleAnchorsError(
            str(reply.get("text") or "fs.edit rejected: stale anchors"),
            stale=[
                StaleAnchor(
                    edit=int(item.get("edit") or 0),
                    anchor=str(item.get("anchor") or ""),
                    current=None if item.get("current") is None else str(item["current"]),
                )
                for item in reply.get("stale") or []
                if isinstance(item, dict)
            ],
            lines=_rows(reply.get("lines")),
        )
    return EditResult(
        path=str(reply.get("path") or ""),
        edits=int(reply.get("edits") or 0),
        removed=int(reply.get("removed") or 0),
        added=int(reply.get("added") or 0),
        total_lines=int(reply.get("totalLines") or 0),
        hunks=[_rows(hunk.get("lines")) for hunk in reply.get("hunks") or [] if isinstance(hunk, dict)],
        text=str(reply.get("text") or ""),
    )
