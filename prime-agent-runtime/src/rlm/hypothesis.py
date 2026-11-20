"""Parallel hypotheses: ``rlm.hypotheses(task, approaches=[...], check=...)``.

Each approach gets its own ``worktree`` checkout and one child spawned in it
(``rlm.spawn(..., cwd=str(wt.path))``). Once the children settle, the
acceptance ``check`` runs in every checkout, and the reply is a ranked table:
passing approaches first, then the ones whose child finished, then the smaller
diff. Each row's diff is a handle (``row.diff.text`` is the patch), so the
table itself stays a few lines. The losing worktrees are removed; the winner's
stays for the model to bring home with ``worktree.merge`` (then
``worktree.remove``).

The fan-out is bounded twice: by the session's RLM depth (a session at its
depth bound refuses before any worktree is made) and by the host's
session-boot admission, which paces every child's boot (``rlm.spawn``
returns once the boot was admitted).
"""

from __future__ import annotations

import asyncio
import inspect
import secrets
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import worktree as _worktree

__all__ = ["DiffHandle", "Hypotheses", "HypothesisRow", "hypotheses"]

APPROACHES_MIN = 2
APPROACHES_MAX = 6
APPROACH_CHARS_MAX = 2_000
TASK_CHARS_MAX = 20_000
DEFAULT_TIMEOUT_MS = 1_800_000
CHECK_TIMEOUT_S = 900
CHECK_TAIL_CHARS = 2_000

Check = str | Callable[[Path], Any]


@dataclass(frozen=True)
class DiffHandle:
    """One approach's patch, kept out of the table: ``str(handle)`` names it,
    ``handle.text`` is the whole patch, ``handle.files`` its paths."""

    name: str
    files: list[str]
    lines: int
    text: str = field(repr=False)

    def __str__(self) -> str:
        if not self.text:
            return f"[diff:{self.name} · empty]"
        return f"[diff:{self.name} · {len(self.files)} files · {self.lines} lines]"


@dataclass(frozen=True)
class HypothesisRow:
    """One approach's outcome. ``worktree`` is ``None`` once its checkout was removed (every
    row but the winner's); ``check_exit`` is the check's exit code (``None``: it did not run
    or timed out), ``check_tail`` the end of its output."""

    rank: int
    approach: str
    name: str
    passed: bool
    child_status: str
    check_exit: int | None
    check_tail: str = field(repr=False)
    answer_preview: str | None
    diff: DiffHandle
    worktree: _worktree.Worktree | None


@dataclass(frozen=True)
class Hypotheses:
    """The ranked table. ``winner`` is the first row when it passed, else ``None``."""

    task: str
    rows: list[HypothesisRow]

    @property
    def winner(self) -> HypothesisRow | None:
        return self.rows[0] if self.rows and self.rows[0].passed else None

    def __str__(self) -> str:
        passed = sum(row.passed for row in self.rows)
        winner = self.winner
        head = f"[hypotheses] {len(self.rows)} approaches · {passed} passed · winner: "
        head += f"#{winner.rank} {winner.name}" if winner else "none"
        lines = [head]
        for row in self.rows:
            check = "pass" if row.passed else f"fail({row.check_exit})"
            approach = row.approach if len(row.approach) <= 48 else row.approach[:47] + "…"
            lines.append(f"{row.rank}. {row.name}  {check}  child:{row.child_status}  {row.diff}  {approach}")
        return "\n".join(lines)


def _validate(task: Any, approaches: Any, check: Any, timeout_ms: Any) -> list[str]:
    if not isinstance(task, str) or not task.strip():
        raise ValueError("task must be a non-empty string")
    if len(task) > TASK_CHARS_MAX:
        raise ValueError(f"task is at most {TASK_CHARS_MAX} characters (got {len(task)})")
    if isinstance(approaches, str) or not isinstance(approaches, Sequence):
        raise TypeError("approaches must be a list of strings")
    choices = list(approaches)
    if not APPROACHES_MIN <= len(choices) <= APPROACHES_MAX:
        raise ValueError(
            f"approaches must hold {APPROACHES_MIN} to {APPROACHES_MAX} entries (got {len(choices)})")
    for choice in choices:
        if not isinstance(choice, str) or not choice.strip():
            raise ValueError("every approach must be a non-empty string")
        if len(choice) > APPROACH_CHARS_MAX:
            raise ValueError(f"an approach is at most {APPROACH_CHARS_MAX} characters (got {len(choice)})")
    if len(set(choices)) != len(choices):
        raise ValueError("approaches must be distinct")
    if not (isinstance(check, str) and check.strip()) and not callable(check):
        raise TypeError("check must be a shell command string or a callable taking the checkout path")
    if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool) or timeout_ms <= 0:
        raise ValueError("timeout_ms must be a positive int")
    return choices


def _brief(task: str, approach: str, path: Path, check: Check) -> str:
    shown = f"`{check}`" if isinstance(check, str) else "a check the parent runs"
    return (
        f"{task}\n\nApproach to take: {approach}\n\n"
        f"Work only in this checkout: {path} (an isolated git worktree; the parent merges it if "
        f"this approach wins). The acceptance check, run from the checkout root, is {shown}. "
        "Make it pass with this approach and no other; do not commit. "
        "Answer with one line on what you changed."
    )


async def _run_check(check: Check, path: Path) -> tuple[bool, int | None, str]:
    """``(passed, exit code, output tail)``; a callable passes on a truthy result (an int is an
    exit code: 0 passes)."""
    if callable(check):
        try:
            outcome = check(path)
            if inspect.isawaitable(outcome):
                outcome = await outcome
        except Exception as error:  # noqa: BLE001 - a raising check fails its approach, verbatim
            return False, None, f"{type(error).__name__}: {error}"
        if isinstance(outcome, int) and not isinstance(outcome, bool):
            return outcome == 0, outcome, ""
        return bool(outcome), 0 if outcome else 1, ""
    process = await asyncio.create_subprocess_shell(
        check, cwd=str(path), stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
    try:
        out, _ = await asyncio.wait_for(process.communicate(), CHECK_TIMEOUT_S)
    except asyncio.TimeoutError:
        process.kill()
        await process.wait()
        return False, None, f"check timed out after {CHECK_TIMEOUT_S}s"
    tail = out.decode(errors="replace")[-CHECK_TAIL_CHARS:]
    return process.returncode == 0, process.returncode, tail


async def _record(approaches: int, passed: int, winner: bool) -> None:
    """Report one run's counts to the host (adoption telemetry); a kernel without a host skips it."""
    try:
        from . import host_request

        await host_request("hypotheses.record", {"approaches": approaches, "passed": passed, "winner": winner})
    except Exception:  # noqa: BLE001 - adoption counts never fail the run
        pass


async def _depth_allows_children() -> None:
    from . import settings as _settings

    current = await _settings.get()
    if current.rlm_max_depth and current.rlm_depth + 1 > current.rlm_max_depth:
        raise RuntimeError(
            f"rlm.hypotheses needs one level of children, but this session is at rlm depth "
            f"{current.rlm_depth} of max {current.rlm_max_depth}")


async def hypotheses(
    task: str,
    approaches: Sequence[str],
    *,
    check: Check,
    timeout_ms: int = DEFAULT_TIMEOUT_MS,
    better: bool = False,
) -> Hypotheses:
    """Try each approach in its own worktree and child; the ranked table (see the module docs).

    Args:
        task: What every child does (at most 20,000 characters).
        approaches: 2 to 6 distinct approaches, one child each.
        check: The acceptance check: a shell command run from each checkout's root (exit 0
            passes), or a callable taking the checkout path (truthy, or an exit code of 0, passes).
        timeout_ms: How long the children may run; a child still running then is aborted and
            its approach fails.
        better: Check each child's brief with ``better`` first (``rlm.spawn(better=True)``).
    """
    choices = _validate(task, approaches, check, timeout_ms)
    await _depth_allows_children()
    from . import abort, collect, spawn

    run = secrets.token_hex(2)
    names = [f"hyp-{run}-{index + 1}" for index in range(len(choices))]
    trees: list[_worktree.Worktree] = []
    handles: list[Any] = []
    keep: str | None = None
    try:
        for name in names:
            trees.append(await _worktree.create(name))
        for name, approach, tree in zip(names, choices, trees):
            handles.append(await spawn(_brief(task, approach, tree.path, check), name=name,
                                       cwd=str(tree.path), better=better))
        settled = {result.rlm_child_id: result for result in await collect(handles, timeout_ms=timeout_ms)}
        outcomes: list[tuple[tuple[bool, bool, int, int], dict[str, Any]]] = []
        for index, (name, approach, tree, handle) in enumerate(zip(names, choices, trees, handles)):
            child = settled.get(handle.rlm_child_id)
            status = child.status if child is not None else "missing"
            if child is not None and not child.settled:
                await abort(handle)
                status = "timeout"
            patch = await _worktree.diff(tree)
            diff = DiffHandle(name=name, files=_worktree._patch_files(patch.encode()),
                              lines=patch.count("\n"), text=patch)
            passed, exit_code, tail = (False, None, "") if status == "timeout" else await _run_check(check, tree.path)
            # Passing first, then a child that finished, then the smaller diff, then the given order.
            key = (not passed, status != "done", diff.lines, index)
            outcomes.append((key, {
                "approach": approach, "name": name, "passed": passed, "child_status": status,
                "check_exit": exit_code, "check_tail": tail, "diff": diff, "tree": tree,
                "answer_preview": child.answer_preview if child is not None else None}))
        outcomes.sort(key=lambda outcome: outcome[0])
        if outcomes and outcomes[0][1]["passed"]:
            keep = outcomes[0][1]["name"]
        rows = []
        for rank, (_, row) in enumerate(outcomes, start=1):
            tree = row.pop("tree")
            rows.append(HypothesisRow(rank=rank, worktree=tree if row["name"] == keep else None, **row))
    except BaseException:
        # A run that fails midway stops the children it started (their checkouts go below).
        for handle in handles:
            try:
                await abort(handle)
            except Exception:  # noqa: BLE001 - the original error is the one to surface
                pass
        raise
    finally:
        # Every checkout but the winner's goes, whether the run finished or failed midway.
        for tree in trees:
            if tree.name != keep:
                await _worktree.remove(tree, force=True)
    table = Hypotheses(task=task, rows=rows)
    await _record(len(rows), sum(row.passed for row in rows), table.winner is not None)
    return table
