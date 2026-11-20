"""Edit-then-check: after an edit, the cheapest applicable check for the touched file runs, and
its diagnostics ride the edit's result, so a broken edit is seen without another round.

The check per file (`_choose`): Python gets `ruff check` limited to the fatal rules (syntax
errors, undefined names) when ruff is installed, else `python -m py_compile`; Rust gets
`cargo check -p <crate>` for the file's own crate; TypeScript gets `tsc --noEmit` on the file's
project; Go gets `go vet` on the file's package. Files outside the REPL's working directory, and
other languages, get none.

Each check runs in the background. The edit waits for it at most its learned budget: a check
that ends in time attaches its diagnostics (or `clean`) to the edit result; a slower one lets the
edit return at once and, if it finds anything, its diagnostics arrive as a `[bash-done]` notice.

What each check costs and finds is learned per project (decaying averages, kept under the agent
dir). A check whose cost exceeds its benefit (the model round its diagnostics save, weighted by
how often it finds any) is dropped, and re-probed now and then so a project that changes can win
it back.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shutil
import signal
import sys
import time
import tomllib
from dataclasses import asdict, dataclass

__all__ = ["after_edit"]

# Decay of the learned averages per observation (the newest weighs 1 - _DECAY).
_DECAY = 0.7
# Runs before a check's average decides anything.
_LEARNING_RUNS = 3
# What a found diagnostic is worth: about one model round saved, in milliseconds.
_ROUND_MS = 20_000.0
# A dropped check still runs every this-many skipped edits, so its averages keep learning.
_REPROBE_EVERY = 8
# The wait inside the edit result: the learned cost with headroom, within these bounds.
_COLD_BUDGET_MS = 1_500.0
_MIN_BUDGET_MS = 250.0
_MAX_BUDGET_MS = 3_000.0
_BUDGET_HEADROOM = 1.5
# A check still running after this is killed and counted as costly and fruitless.
_HARD_LIMIT_SECONDS = 120.0
# Bounds of the diagnostics shown.
_SHOWN_LINES = 40
_SHOWN_CHARS = 4_000
# How long reading the learned state may hold an edit, and how long a late notice may wait.
_STATE_IO_SECONDS = 0.2
_NOTICE_WAIT_SECONDS = 5.0
_SCHEMA = 1
_STATE_DIR = "edit-checks"
# The fatal ruff rules: syntax errors, undefined names, invalid comparisons and statements.
_RUFF_FATAL = "E9,F63,F7,F82"


@dataclass(frozen=True)
class _Check:
    kind: str
    argv: tuple[str, ...]
    cwd: str

    @property
    def label(self) -> str:
        return " ".join(self.argv)


@dataclass
class _Stats:
    """One check kind's learned averages in one project."""

    cost_ms: float = 0.0
    found: float = 0.0
    runs: int = 0
    skips: int = 0

    def observe(self, cost_ms: float, found: bool) -> None:
        if self.runs == 0:
            self.cost_ms, self.found = cost_ms, float(found)
        else:
            self.cost_ms = self.cost_ms * _DECAY + cost_ms * (1 - _DECAY)
            self.found = self.found * _DECAY + float(found) * (1 - _DECAY)
        self.runs += 1
        self.skips = 0

    def worth_running(self) -> bool:
        if self.runs < _LEARNING_RUNS or self.cost_ms <= self.found * _ROUND_MS:
            return True
        return self.skips + 1 >= _REPROBE_EVERY

    def budget_seconds(self) -> float:
        if self.runs == 0:
            return _COLD_BUDGET_MS / 1000
        return min(max(self.cost_ms * _BUDGET_HEADROOM, _MIN_BUDGET_MS), _MAX_BUDGET_MS) / 1000


@dataclass(frozen=True)
class _Outcome:
    """One check run; `pid` is 0 and `exit_code` -1 when it could not start or was killed."""

    pid: int
    exit_code: int
    output: str
    seconds: float

    @property
    def found(self) -> bool:
        return self.exit_code != 0 and bool(self.output.strip())


def _nearest(start: str, root: str, name: str) -> str | None:
    """The nearest directory from `start` up to `root` holding `name`."""
    directory = start
    while True:
        if os.path.isfile(os.path.join(directory, name)):
            return directory
        if directory == root or os.path.dirname(directory) == directory:
            return None
        directory = os.path.dirname(directory)


def _choose(path: str, root: str) -> _Check | None:
    """The cheapest applicable check for `path` (inside `root`), or None."""
    directory = os.path.dirname(path)
    suffix = os.path.splitext(path)[1]
    if suffix == ".py":
        ruff = shutil.which("ruff")
        if ruff:
            return _Check("ruff", (ruff, "check", "--isolated", "--quiet", "--output-format", "concise", "--select", _RUFF_FATAL, path), root)
        return _Check("py_compile", (sys.executable, "-m", "py_compile", path), root)
    if suffix == ".rs":
        crate = _nearest(directory, root, "Cargo.toml")
        cargo = shutil.which("cargo")
        if crate is None or cargo is None:
            return None
        try:
            with open(os.path.join(crate, "Cargo.toml"), "rb") as manifest:
                name = tomllib.load(manifest).get("package", {}).get("name")
        except (OSError, tomllib.TOMLDecodeError):
            return None
        if not isinstance(name, str):
            return None
        return _Check("cargo", (cargo, "check", "--quiet", "--message-format", "short", "-p", name), root)
    if suffix in (".ts", ".tsx", ".mts", ".cts"):
        project = _nearest(directory, root, "tsconfig.json")
        if project is None:
            return None
        local = [os.path.join(base, "node_modules", ".bin", "tsc") for base in (project, root)]
        tsc = next((candidate for candidate in local if os.path.isfile(candidate)), None) or shutil.which("tsc")
        if tsc is None:
            return None
        return _Check("tsc", (tsc, "--noEmit", "--pretty", "false", "-p", project), root)
    if suffix == ".go":
        module = _nearest(directory, root, "go.mod")
        go = shutil.which("go")
        if module is None or go is None:
            return None
        package = os.path.relpath(directory, module)
        return _Check("go_vet", (go, "vet", "./" + package.replace(os.sep, "/")), module)
    return None


class _Learned:
    """The project's learned stats, loaded on first use and saved after each change."""

    def __init__(self) -> None:
        self._stats: dict[str, _Stats] | None = None
        self._path: str | None = None

    def _load(self, path: str | None) -> dict[str, _Stats]:
        if path is None:
            return {}
        try:
            with open(path, encoding="utf-8") as file:
                stored = json.load(file)
            if stored.get("schema") != _SCHEMA:
                return {}
            return {kind: _Stats(**fields) for kind, fields in stored.get("checks", {}).items()}
        except (OSError, ValueError, TypeError):
            return {}

    async def stats(self, root: str, kind: str) -> _Stats:
        if self._stats is None:
            agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
            if agent_dir:
                digest = hashlib.sha256(root.encode()).hexdigest()[:16]
                self._path = os.path.join(agent_dir, _STATE_DIR, f"{digest}.json")
            try:
                self._stats = await asyncio.wait_for(asyncio.to_thread(self._load, self._path), _STATE_IO_SECONDS)
            except TimeoutError:
                self._stats = {}
        return self._stats.setdefault(kind, _Stats())

    def save(self) -> None:
        """Write the stats in the background; a failed write is dropped (the next one is newer)."""
        if self._path is None or self._stats is None:
            return
        body = json.dumps({"schema": _SCHEMA, "checks": {kind: asdict(stats) for kind, stats in self._stats.items()}})
        path = self._path

        def write() -> None:
            try:
                os.makedirs(os.path.dirname(path), exist_ok=True)
                temp = f"{path}.{os.getpid()}.tmp"
                with open(temp, "w", encoding="utf-8") as file:
                    file.write(body)
                os.replace(temp, path)
            except OSError:
                pass

        _keep(asyncio.ensure_future(asyncio.to_thread(write)))


_learned = _Learned()
# Background checks and saves, kept alive until they end.
_tasks: set[asyncio.Future[object]] = set()


def _keep(task: asyncio.Future[object]) -> None:
    _tasks.add(task)
    task.add_done_callback(_tasks.discard)


async def _run(check: _Check) -> _Outcome:
    started = time.monotonic()
    try:
        process = await asyncio.create_subprocess_exec(
            *check.argv,
            cwd=check.cwd,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
            start_new_session=os.name == "posix",
        )
    except OSError:
        return _Outcome(pid=0, exit_code=-1, output="", seconds=time.monotonic() - started)
    try:
        output, _ = await asyncio.wait_for(process.communicate(), _HARD_LIMIT_SECONDS)
    except TimeoutError:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        await process.wait()
        return _Outcome(pid=0, exit_code=-1, output="", seconds=time.monotonic() - started)
    return _Outcome(process.pid, process.returncode or 0, output.decode("utf-8", "replace"), time.monotonic() - started)


def _shown(output: str) -> str:
    lines = output.strip().splitlines()
    text = "\n".join(lines[:_SHOWN_LINES])[:_SHOWN_CHARS]
    hidden = len(lines) - len(text.splitlines())
    return text + (f"\n[{hidden} more lines]" if hidden > 0 else "")


def _note(outcome: str, kind: str) -> None:
    """Count one check outcome on the host (fire-and-forget; telemetry only)."""
    from . import repl

    try:
        repl.host_notify({"type": "edit_check.fact", "outcome": outcome, "kind": kind})
    except (OSError, RuntimeError, ValueError):
        pass


def _finish(check: _Check, stats: _Stats, outcome: _Outcome) -> None:
    stats.observe(outcome.seconds * 1000, outcome.found)
    _learned.save()
    if outcome.pid == 0:
        _note("failed", check.kind)
    else:
        _note("found" if outcome.found else "clean", check.kind)


async def _deliver_late(check: _Check, stats: _Stats, task: asyncio.Future[_Outcome], edited: str) -> None:
    """A check that outlived the edit's wait: learn from it, and send its diagnostics as a notice."""
    outcome = await task
    _finish(check, stats, outcome)
    if not outcome.found:
        return
    from . import repl
    from .bash import _completion_notices_wired

    if not _completion_notices_wired or not repl.is_active():
        return
    lines = outcome.output.strip().splitlines()
    try:
        await asyncio.wait_for(
            repl.host_request(
                {
                    "type": "bash.completed",
                    "pid": outcome.pid,
                    "command": f"{check.label}  # check after editing {edited}",
                    "exitCode": outcome.exit_code,
                    "output": _shown(outcome.output),
                    "outputTruncated": len(lines) > _SHOWN_LINES,
                }
            ),
            _NOTICE_WAIT_SECONDS,
        )
    except (TimeoutError, OSError, RuntimeError, ValueError):
        pass


async def after_edit(path: str) -> str | None:
    """Run the touched file's check, waiting at most its learned budget: the diagnostics line(s) to
    attach to the edit result, or None when the file gets no check or its check was dropped."""
    root = os.path.realpath(os.getcwd())
    target = os.path.realpath(path)
    if not os.path.isfile(target) or os.path.commonpath([root, target]) != root:
        return None
    check = _choose(target, root)
    if check is None:
        return None
    stats = await _learned.stats(root, check.kind)
    if not stats.worth_running():
        stats.skips += 1
        _note("skipped", check.kind)
        return None
    task = asyncio.ensure_future(_run(check))
    budget = stats.budget_seconds()
    done, _ = await asyncio.wait({task}, timeout=budget)
    if task not in done:
        _note("late", check.kind)
        _keep(asyncio.ensure_future(_deliver_late(check, stats, task, os.path.relpath(target, root))))
        return f"[check: {check.label} · still running after {budget:.1f} s; any diagnostics arrive as a [bash-done] notice]"
    outcome = task.result()
    _finish(check, stats, outcome)
    if outcome.pid == 0:
        return None
    if not outcome.found:
        return f"[check: {check.label} · clean · {outcome.seconds:.1f} s]"
    return f"[check: {check.label} · exit {outcome.exit_code} · {outcome.seconds:.1f} s]\n{_shown(outcome.output)}"
