## Workspace checkpoints

Snapshot the workspace before a risky multi-file change, so it can be undone. Checkpoints live in prime-agent's own shadow store (the one `/undo` uses), never in the project's git history, and belong to this session. They need the prime-agent daemon; headless runs raise `RuntimeError`.

- `checkpoint.create(label: str = "") -> Checkpoint`: snapshot the files now; `Checkpoint(id, label, created_at)`
- `checkpoint.list() -> list[Checkpoint]`: this session's checkpoints, oldest first
- `checkpoint.restore(id: str) -> RestoreResult`: put the files back to a checkpoint of this session. Off unless the user set `replCanRestoreCheckpoints` (raises `PermissionError` otherwise); the user is always asked to confirm the listed changes first. `result.status` is `restored`, `declined` (keep the files; do not retry), or `no_user` (nobody could confirm; nothing changed); `result.restored` counts the paths written or removed
