"""Isolated git worktrees for subagents.

``wt = await worktree.create("lane-a")`` checks the session's repository out
into a private worktree under the agent dir (keyed by a hash of the repository
path and by this session), copying the parent's uncommitted changes into it by
default. Spawn a child there with ``rlm.spawn(..., cwd=str(wt.path))``; its
edits stay out of the parent's checkout. ``await worktree.diff(wt)`` is the
child's delta since the worktree was made (the carried changes excluded), and
``await worktree.merge(wt)`` applies that delta to the parent's checkout only
when ``git apply --3way --check`` says it applies cleanly: a conflict is listed
and nothing is applied, never resolved automatically. ``strategy="branch"``
commits the delta to a branch instead of touching the checkout.
``await worktree.remove(wt)`` deletes it.
"""

from __future__ import annotations

import asyncio
import builtins
import hashlib
import json
import os
import re
import shutil
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .agent_dir import agent_dir

__all__ = ["MergeResult", "Worktree", "create", "diff", "list", "merge", "remove"]

_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
_STRATEGIES = ("patch", "branch")


@dataclass(frozen=True)
class Worktree:
    """One worktree: ``name``, its checkout ``path``, the parent ``repo``, the ``base`` commit
    it was made from, and whether it still ``exists``."""

    name: str
    path: Path
    repo: Path
    base: str
    exists: bool = True


@dataclass(frozen=True)
class MergeResult:
    """``status`` is ``applied`` (the delta is in the checkout), ``clean`` (a dry run that would
    apply), ``conflicts`` (nothing applied; ``conflicts`` lists the paths), ``empty`` (no delta),
    or ``branch`` (the delta was committed to ``branch``). ``files`` lists the delta's paths."""

    status: str
    files: builtins.list[str] = field(default_factory=builtins.list)
    conflicts: builtins.list[str] = field(default_factory=builtins.list)
    branch: str | None = None

    @property
    def ok(self) -> bool:
        return self.status in ("applied", "clean", "empty", "branch")


async def _git(cwd: Path, *args: str, stdin: bytes | None = None, env: dict[str, str] | None = None,
               check: bool = True) -> tuple[int, bytes, bytes]:
    process = await asyncio.create_subprocess_exec(
        "git", *args, cwd=str(cwd),
        stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        env={**os.environ, **(env or {})})
    out, err = await process.communicate(stdin)
    code = process.returncode if process.returncode is not None else -1
    if check and code != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {err.decode(errors='replace').strip()}")
    return code, out, err


async def _text(cwd: Path, *args: str) -> str:
    return (await _git(cwd, *args))[1].decode().strip()


def _session_key() -> str:
    return os.environ.get("RLM_SESSION_ID") or "no-session"


def _root(repo: Path) -> Path:
    digest = hashlib.sha256(str(repo).encode()).hexdigest()[:16]
    return agent_dir() / "worktrees" / digest / _session_key()


async def _repo() -> Path:
    try:
        return Path(await _text(Path.cwd(), "rev-parse", "--show-toplevel")).resolve()
    except RuntimeError as error:
        raise RuntimeError(f"worktree needs a git repository at the session cwd: {error}") from None


def _record_path(root: Path, name: str) -> Path:
    return root / f"{name}.json"


def _load(record: Path) -> tuple[Worktree, str]:
    data = json.loads(record.read_text())
    path = Path(data["path"])
    tree = Worktree(name=data["name"], path=path, repo=Path(data["repo"]), base=data["base"],
                    exists=path.is_dir())
    return tree, data["baseline"]


def _mark_merged(tree: Worktree, merged: str) -> None:
    """Record the content a merge brought home: removing the worktree at it loses nothing."""
    record = _record_path(_root(tree.repo), tree.name)
    data = json.loads(record.read_text())
    data["merged"] = merged
    record.write_text(json.dumps(data))


async def _resolve(target: Worktree | str) -> tuple[Worktree, str]:
    name = target.name if isinstance(target, Worktree) else target
    if not isinstance(name, str) or not _NAME.match(name):
        raise ValueError(f"unknown worktree: {target!r}")
    record = _record_path(_root(await _repo()), name)
    if not record.is_file():
        raise ValueError(f"no worktree named {name!r} in this session")
    return _load(record)


async def _snapshot_tree(path: Path) -> str:
    """The checkout's full content (tracked and untracked, ignores honored) as a tree object,
    written through a throwaway index so the checkout's own index never changes."""
    with tempfile.TemporaryDirectory() as scratch:
        env = {"GIT_INDEX_FILE": str(Path(scratch) / "index")}
        await _git(path, "read-tree", "HEAD", env=env)
        await _git(path, "add", "-A", ".", env=env)
        return (await _git(path, "write-tree", env=env))[1].decode().strip()


async def _record(op: str, outcome: str, tree: Worktree | None = None) -> None:
    """Report one operation's outcome to the host: its adoption counts, and (with ``tree``) the
    ledger behind the session-end notice of unremoved worktrees. A kernel without a host skips it."""
    payload: dict[str, Any] = {"op": op, "outcome": outcome}
    if tree is not None:
        payload.update(name=tree.name, path=str(tree.path))
    try:
        from . import host_request

        await host_request("worktree.record", payload)
    except Exception:  # noqa: BLE001 - adoption counts never fail the operation
        pass


async def create(name: str, *, base: str = "HEAD", carry_changes: bool = True) -> Worktree:
    """Check ``base`` out into a new worktree ``name``; ``carry_changes`` copies the parent's
    uncommitted changes (staged, unstaged, and untracked files) into it."""
    if not isinstance(name, str) or not _NAME.match(name):
        raise ValueError("name must be 1-64 letters, digits, '.', '_' or '-', starting alphanumeric")
    repo = await _repo()
    root = _root(repo)
    path = root / name
    if _record_path(root, name).exists() or path.exists():
        raise ValueError(f"a worktree named {name!r} already exists in this session")
    base_commit = await _text(repo, "rev-parse", "--verify", f"{base}^{{commit}}")
    root.mkdir(parents=True, exist_ok=True)
    await _git(repo, "worktree", "add", "--detach", str(path), base_commit)
    try:
        if carry_changes:
            patch = (await _git(repo, "diff", "--binary", "HEAD"))[1]
            if patch:
                await _git(path, "apply", "--binary", stdin=patch)
            untracked = (await _git(repo, "ls-files", "--others", "--exclude-standard", "-z"))[1]
            for relative in filter(None, untracked.decode().split("\0")):
                source, target = repo / relative, path / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
        baseline = await _snapshot_tree(path)
    except Exception:
        await _git(repo, "worktree", "remove", "--force", str(path), check=False)
        await _record("create", "failed")
        raise
    _record_path(root, name).write_text(json.dumps({
        "name": name, "path": str(path), "repo": str(repo), "base": base_commit,
        "baseline": baseline}))
    tree = Worktree(name=name, path=path, repo=repo, base=base_commit)
    await _record("create", "ok", tree)
    return tree


async def list() -> builtins.list[Worktree]:  # noqa: A001 - the API name
    """This session's worktrees of the current repository, by name."""
    root = _root(await _repo())
    if not root.is_dir():
        return []
    return [_load(record)[0] for record in sorted(root.glob("*.json"))]


async def _delta(tree: Worktree, baseline: str) -> bytes:
    if not tree.exists:
        raise RuntimeError(f"worktree {tree.name!r} no longer exists at {tree.path}")
    current = await _snapshot_tree(tree.path)
    return (await _git(tree.path, "diff", "--binary", baseline, current))[1]


async def diff(target: Worktree | str) -> str:
    """The worktree's changes since it was made (the carried changes excluded), as a patch."""
    tree, baseline = await _resolve(target)
    patch = await _delta(tree, baseline)
    await _record("diff", "ok")
    return patch.decode(errors="replace")


def _patch_files(patch: bytes) -> builtins.list[str]:
    files = re.findall(rb"^diff --git a/(.+?) b/", patch, flags=re.M)
    return [name.decode(errors="replace") for name in files]


def _conflicts(stderr: bytes) -> builtins.list[str]:
    found: builtins.list[str] = []
    for pattern in (rb"error: patch failed: (.+?):\d+", rb"error: (.+?): does not exist in index",
                    rb"error: (.+?): already exists in working directory",
                    rb"error: (.+?): patch does not apply", rb"conflicts? in (.+)"):
        for match in re.findall(pattern, stderr):
            name = match.decode(errors="replace").strip()
            if name not in found:
                found.append(name)
    return found or ["(git apply refused the patch)"]


async def merge(target: Worktree | str, *, strategy: str = "patch", dry_run: bool = False) -> MergeResult:
    """Bring the worktree's delta home. ``patch`` applies it to the parent's checkout after
    ``git apply --3way --check`` passes (a conflict is listed and nothing is applied); ``branch``
    commits it to ``prime-agent/worktree/<name>`` without touching the checkout. ``dry_run``
    only checks."""
    if strategy not in _STRATEGIES:
        raise ValueError('strategy must be "patch" or "branch"')
    tree, baseline = await _resolve(target)
    patch = await _delta(tree, baseline)
    files = _patch_files(patch)
    if not patch:
        await _record("merge", "empty")
        return MergeResult(status="empty")
    if strategy == "branch":
        branch = f"prime-agent/worktree/{tree.name}"
        if dry_run:
            return MergeResult(status="clean", files=files, branch=branch)
        # The commits carry the repository's own configured identity.
        start = (await _git(tree.path, "commit-tree", baseline, "-p", tree.base,
                            "-m", f"worktree {tree.name}: baseline"))[1].decode().strip()
        current = await _snapshot_tree(tree.path)
        commit = (await _git(tree.path, "commit-tree", current, "-p", start,
                             "-m", f"worktree {tree.name}: changes"))[1].decode().strip()
        await _git(tree.repo, "branch", "--force", branch, commit)
        _mark_merged(tree, current)
        await _record("merge", "branch")
        return MergeResult(status="branch", files=files, branch=branch)
    code, _, err = await _git(tree.repo, "apply", "--3way", "--check", stdin=patch, check=False)
    if code != 0:
        await _record("merge", "conflicts")
        return MergeResult(status="conflicts", files=files, conflicts=_conflicts(err))
    if dry_run:
        return MergeResult(status="clean", files=files)
    # A patch that applies as is lands in the working tree only; one that needs the three-way
    # fallback (it passed the check, so without a conflict) is staged by git.
    plain, _, _ = await _git(tree.repo, "apply", "--check", stdin=patch, check=False)
    await _git(tree.repo, "apply", *(() if plain == 0 else ("--3way",)), stdin=patch)
    _mark_merged(tree, await _snapshot_tree(tree.path))
    await _record("merge", "applied")
    return MergeResult(status="applied", files=files)


async def remove(target: Worktree | str, *, force: bool = False) -> None:
    """Delete the worktree. Unmerged changes refuse the removal unless ``force``."""
    tree, baseline = await _resolve(target)
    merged = json.loads(_record_path(_root(tree.repo), tree.name).read_text()).get("merged")
    unmerged = tree.exists and not force and await _delta(tree, baseline) \
        and await _snapshot_tree(tree.path) != merged
    if unmerged:
        raise RuntimeError(
            f"worktree {tree.name!r} has changes that were not merged; merge it first or pass force=True")
    if tree.exists:
        await _git(tree.repo, "worktree", "remove", "--force", str(tree.path))
    await _git(tree.repo, "worktree", "prune", check=False)
    _record_path(_root(tree.repo), tree.name).unlink(missing_ok=True)
    await _record("remove", "ok", tree)
