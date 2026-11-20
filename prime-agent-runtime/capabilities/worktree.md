## Worktrees

Give a subagent its own checkout so its edits stay out of yours: `wt = await worktree.create("lane-a")`, then `rlm.spawn(..., cwd=str(wt.path))`. Worktrees live under the agent dir, never inside the project, and belong to this session; remove each one when its work is merged.

- `worktree.create(name: str, *, base: str = "HEAD", carry_changes: bool = True) -> Worktree`: a new checkout of `base`; `carry_changes` copies your uncommitted changes (staged, unstaged, untracked) into it; `Worktree(name, path, repo, base, exists)`
- `worktree.list() -> list[Worktree]`: this session's worktrees of the current repository
- `worktree.diff(target: Worktree | str) -> str`: the worktree's changes since it was made (the carried changes excluded), as a patch
- `worktree.merge(target: Worktree | str, *, strategy: Literal["patch", "branch"] = "patch", dry_run: bool = False) -> MergeResult`: `patch` applies the delta to your checkout only when `git apply --3way --check` passes; on a conflict nothing is applied and `result.conflicts` lists the paths (resolve them yourself, never by retrying); `branch` commits the delta to `prime-agent/worktree/<name>` instead. `result.status` is `applied`, `clean` (a dry run that would apply), `conflicts`, `empty`, or `branch`
- `worktree.remove(target: Worktree | str, *, force: bool = False) -> None`: delete it; unmerged changes refuse the removal unless `force`
