"""The read-only command cache: an idempotent read-only `bash()` command (`git status`/`log`/`diff`,
`ls`, `rg`/`grep`, `cat`) or `fs.read` whose inputs have not changed answers from its last result at
once, marked `cached`, instead of running again.

Only a strict allowlist is cached: one simple command (no pipe, redirect, chain, substitution,
expansion, or glob), from the programs above, with no option that writes or runs another program.
A result is keyed by the working directory and the exact command, and is valid while the files it
read are unchanged: each entry keeps a fingerprint (size, mtime, inode) of the files it depends on
(the named files for `cat`; the listed directories for `ls`; the searched tree for `rg`/`grep`; the
work tree with `.git/HEAD` and `.git/index` for `git`), taken before the command ran and again when it
ended. A write through `fs.edit`/`edit`, or any write a later fingerprint sees through mtime, misses.
A tree too large to fingerprint cheaply is never cached.
"""

from __future__ import annotations

import os
import shlex
import threading
from collections import OrderedDict
from dataclasses import dataclass
from typing import Any

__all__ = [
    "CacheKey",
    "current_epoch",
    "fingerprint",
    "invalidate",
    "lookup",
    "note",
    "plan",
    "plan_files",
    "remember",
]

# The programs whose listed subcommands only read.
_GIT_READS = frozenset({"status", "log", "diff"})
_PROGRAMS = frozenset({"git", "ls", "cat", "rg", "grep"})
# Options that write a file or run another program.
_WRITING_OPTIONS = ("--output", "--ext-diff", "--pre", "--exec")
# Shell syntax that makes a command more than one read.
_SHELL_SYNTAX = frozenset("|;&<>$`()*?[]{}~\\\n!")
# Files a fingerprint may stat before the command counts as uncacheable.
_MAX_FINGERPRINT_ENTRIES = 5_000
# Entries kept, and the largest output kept per entry.
_MAX_ENTRIES = 64
_MAX_OUTPUT_CHARS = 256 * 1024
# Directories a tree fingerprint never walks into (their writes never change these reads).
_SKIPPED_DIRS = frozenset({".git", "node_modules", "target", "__pycache__", ".venv"})


@dataclass(frozen=True)
class CacheKey:
    cwd: str
    command: str


@dataclass(frozen=True)
class _Plan:
    """What a cacheable command depends on: named files, directories listed, trees walked."""

    files: tuple[str, ...]
    dirs: tuple[str, ...]
    trees: tuple[str, ...]
    git: bool


def plan(command: str) -> _Plan | None:
    """The dependencies of an allowlisted read-only `command`, or None when it is not cacheable."""
    if any(char in _SHELL_SYNTAX for char in command):
        return None
    try:
        words = shlex.split(command)
    except ValueError:
        return None
    if not words or words[0] not in _PROGRAMS:
        return None
    if any(word.startswith(_WRITING_OPTIONS) for word in words[1:]):
        return None
    program, args = words[0], words[1:]
    operands = [arg for arg in args if not arg.startswith("-")]
    if program == "git":
        if not args or args[0] not in _GIT_READS:
            return None
        return _Plan(files=(), dirs=(), trees=(".",), git=True)
    if program == "cat":
        return _Plan(files=tuple(operands), dirs=(), trees=(), git=False) if operands else None
    if program == "ls":
        recursive = any(arg.startswith("-") and not arg.startswith("--") and "R" in arg for arg in args)
        targets = tuple(operands) or (".",)
        return _Plan(files=(), dirs=() if recursive else targets, trees=targets if recursive else (), git=False)
    # rg/grep: the first operand is the pattern (unless -e/-f gave it); the rest are paths.
    pattern_given = any(arg in ("-e", "-f") or arg.startswith(("-e", "--regexp", "--file")) for arg in args)
    paths = operands if pattern_given else operands[1:]
    if program == "grep" and not paths:
        return None  # grep without a path reads stdin
    return _Plan(files=(), dirs=(), trees=tuple(paths) or (".",), git=False)


def plan_files(*paths: str) -> _Plan:
    """The dependencies of a read of exactly these files (a kernel `fs.read`)."""
    return _Plan(files=paths, dirs=(), trees=(), git=False)


def _stat(path: str) -> tuple[Any, ...]:
    try:
        info = os.stat(path)
    except OSError:
        return (path, None)
    return (path, info.st_size, info.st_mtime_ns, info.st_ino)


def fingerprint(cwd: str, deps: _Plan) -> tuple[Any, ...] | None:
    """The stamps of everything `deps` reads, or None when there are too many to take cheaply."""
    stamps: list[Any] = []
    budget = _MAX_FINGERPRINT_ENTRIES
    for name in deps.files:
        stamps.append(_stat(os.path.join(cwd, name)))
    for name in deps.dirs:
        directory = os.path.join(cwd, name)
        stamps.append(_stat(directory))
        try:
            entries = sorted(os.listdir(directory)) if os.path.isdir(directory) else []
        except OSError:
            entries = []
        budget -= len(entries)
        if budget < 0:
            return None
        stamps.extend(_stat(os.path.join(directory, entry)) for entry in entries)
    roots = list(deps.trees)
    if deps.git:
        stamps.extend(_stat(os.path.join(cwd, ".git", name)) for name in ("HEAD", "index", "packed-refs"))
    for name in roots:
        root = os.path.join(cwd, name)
        stamps.append(_stat(root))
        for directory, subdirs, files in os.walk(root):
            subdirs[:] = sorted(sub for sub in subdirs if sub not in _SKIPPED_DIRS)
            budget -= len(files) + 1
            if budget < 0:
                return None
            stamps.append(_stat(directory))
            stamps.extend(_stat(os.path.join(directory, file)) for file in sorted(files))
    return tuple(stamps)


@dataclass(frozen=True)
class _Entry:
    stamps: tuple[Any, ...]
    epoch: int
    value: Any


_lock = threading.Lock()
_entries: OrderedDict[Any, _Entry] = OrderedDict()
_epoch = 0


def invalidate() -> None:
    """Forget every entry: a write through `fs.edit` or the `edit` skill."""
    global _epoch
    with _lock:
        _epoch += 1
        _entries.clear()


def current_epoch() -> int:
    with _lock:
        return _epoch


def lookup(key: Any, stamps: tuple[Any, ...]) -> Any | None:
    """The cached value for `key` while its stamps still hold, else None."""
    with _lock:
        entry = _entries.get(key)
        if entry is None or entry.epoch != _epoch or entry.stamps != stamps:
            return None
        _entries.move_to_end(key)
        return entry.value


def remember(key: Any, stamps: tuple[Any, ...], epoch: int, value: Any, *, chars: int) -> None:
    """Keep `value` for `key` under the stamps taken before it was produced; an edit since then
    (a newer epoch) or an output too large to keep leaves the cache as it was."""
    if chars > _MAX_OUTPUT_CHARS:
        return
    with _lock:
        if epoch != _epoch:
            return
        _entries[key] = _Entry(stamps, epoch, value)
        _entries.move_to_end(key)
        while len(_entries) > _MAX_ENTRIES:
            _entries.popitem(last=False)


def note(source: str, hit: bool) -> None:
    """Count one lookup on the host (fire-and-forget; telemetry only)."""
    from . import repl

    try:
        repl.host_notify({"type": "command_cache.fact", "source": source, "hit": hit})
    except (OSError, RuntimeError, ValueError):
        pass
