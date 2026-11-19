"""Kernel client for Prime Agent's native, in-process file search.

``await fs.grep(pattern)``, ``await fs.glob(pattern)`` and ``await fs.find()``
send the ``fs.grep``/``fs.glob``/``fs.find`` host requests. The host runs the
ripgrep library crates in process (regex search, ``.gitignore``-aware walks,
globs, file types), so they behave the same on Linux, macOS and Windows and
need no ``rg``, ``grep`` or ``find`` installed. Paths resolve here, against the
REPL's working directory, and results print them relative to it with ``/``.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any, Literal

from . import host_request

__all__ = ["FsEntry", "FsListing", "GrepMatch", "GrepResult", "find", "glob", "grep"]

_CASES = ("sensitive", "insensitive", "smart")
_KINDS = ("file", "dir", "symlink")


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
