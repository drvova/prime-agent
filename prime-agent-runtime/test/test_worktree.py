from __future__ import annotations

import asyncio
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, call, patch

from rlm import worktree


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout


class WorktreeTest(unittest.TestCase):
    """Each test runs in a throwaway repository with its own agent dir and session id."""

    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name).resolve()
        self.repo = root / "repo"
        self.repo.mkdir()
        _git(self.repo, "init", "-q", "-b", "main")
        # A throwaway test repository's own identity (never this project's history).
        _git(self.repo, "config", "user.name", "Worktree Test")
        _git(self.repo, "config", "user.email", "worktree-test@example.invalid")
        (self.repo / "app.py").write_text("one\ntwo\nthree\n")
        (self.repo / "notes.txt").write_text("keep\n")
        _git(self.repo, "add", ".")
        _git(self.repo, "commit", "-q", "-m", "base")
        env = patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent"),
                                      "RLM_SESSION_ID": "session-1"})
        env.start()
        self.addCleanup(env.stop)
        cwd = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, cwd)
        self.host = AsyncMock(return_value={})
        recorder = patch("rlm.host_request", self.host)
        recorder.start()
        self.addCleanup(recorder.stop)

    def run_async(self, coroutine):
        return asyncio.run(coroutine)

    def test_create_carries_dirty_state_and_diff_is_only_the_childs_delta(self) -> None:
        (self.repo / "app.py").write_text("one\nTWO\nthree\n")
        (self.repo / "draft.md").write_text("untracked draft\n")
        tree = self.run_async(worktree.create("lane-a"))
        self.assertEqual(tree.path.parent.parent.parent.name, "worktrees")
        self.assertEqual(tree.path.parent.name, "session-1")
        self.assertEqual((tree.path / "app.py").read_text(), "one\nTWO\nthree\n")
        self.assertEqual((tree.path / "draft.md").read_text(), "untracked draft\n")
        self.assertEqual(self.run_async(worktree.diff(tree)), "")
        (tree.path / "notes.txt").write_text("keep\nchild line\n")
        patch_text = self.run_async(worktree.diff("lane-a"))
        self.assertIn("+child line", patch_text)
        self.assertNotIn("TWO", patch_text)
        self.assertEqual(self.run_async(worktree.list()), [tree])

    def test_merge_checks_first_and_never_applies_a_conflict(self) -> None:
        tree = self.run_async(worktree.create("lane-b"))
        (tree.path / "app.py").write_text("one\nchild\nthree\n")
        (self.repo / "app.py").write_text("one\nparent\nthree\n")
        result = self.run_async(worktree.merge(tree))
        self.assertEqual(result, worktree.MergeResult(status="conflicts", files=["app.py"],
                                                      conflicts=result.conflicts))
        self.assertTrue(result.conflicts)
        self.assertEqual((self.repo / "app.py").read_text(), "one\nparent\nthree\n")

    def test_merge_applies_a_clean_delta_and_remove_needs_force_only_before_it(self) -> None:
        tree = self.run_async(worktree.create("lane-c"))
        (tree.path / "notes.txt").write_text("keep\nmerged\n")
        with self.assertRaisesRegex(RuntimeError, "not merged"):
            self.run_async(worktree.remove(tree))
        dry = self.run_async(worktree.merge(tree, dry_run=True))
        self.assertEqual(dry, worktree.MergeResult(status="clean", files=["notes.txt"]))
        self.assertEqual((self.repo / "notes.txt").read_text(), "keep\n")
        merged = self.run_async(worktree.merge(tree))
        self.assertEqual(merged, worktree.MergeResult(status="applied", files=["notes.txt"]))
        self.assertEqual((self.repo / "notes.txt").read_text(), "keep\nmerged\n")
        self.run_async(worktree.remove(tree))
        self.assertFalse(tree.path.exists())
        self.assertEqual(self.run_async(worktree.list()), [])
        self.assertEqual(
            self.host.await_args_list,
            [call("worktree.record", {"op": "create", "outcome": "ok", "name": "lane-c",
                                      "path": str(tree.path)}),
             call("worktree.record", {"op": "merge", "outcome": "applied"}),
             call("worktree.record", {"op": "remove", "outcome": "ok", "name": "lane-c",
                                      "path": str(tree.path)})],
        )

    def test_branch_strategy_commits_the_delta_without_touching_the_checkout(self) -> None:
        tree = self.run_async(worktree.create("lane-d"))
        (tree.path / "new.py").write_text("print('hi')\n")
        result = self.run_async(worktree.merge(tree, strategy="branch"))
        self.assertEqual(result, worktree.MergeResult(status="branch", files=["new.py"],
                                                      branch="prime-agent/worktree/lane-d"))
        self.assertFalse((self.repo / "new.py").exists())
        shown = _git(self.repo, "show", "--stat", "--format=%s", "prime-agent/worktree/lane-d")
        self.assertIn("worktree lane-d: changes", shown)
        self.assertIn("new.py", shown)
        self.run_async(worktree.remove(tree))

    def test_names_are_validated_and_unique(self) -> None:
        with self.assertRaisesRegex(ValueError, "name must be"):
            self.run_async(worktree.create("../escape"))
        self.run_async(worktree.create("lane-e", carry_changes=False))
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.run_async(worktree.create("lane-e"))
        with self.assertRaisesRegex(ValueError, "strategy"):
            self.run_async(worktree.merge("lane-e", strategy="squash"))
        self.run_async(worktree.remove("lane-e", force=True))


if __name__ == "__main__":
    unittest.main()
