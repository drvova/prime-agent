from __future__ import annotations

import asyncio
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import rlm
from rlm import hypothesis, worktree


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout


class FauxHost:
    """The host side of one session: settings at a given depth, and faux children that
    apply their approach's edit in their worktree the moment they are spawned."""

    def __init__(self, *, depth: int, max_depth: int, edits: dict[str, str]) -> None:
        self.depth, self.max_depth, self.edits = depth, max_depth, edits
        self.spawned: list[dict[str, Any]] = []
        self.records: list[dict[str, Any]] = []

    async def __call__(self, request: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
        payload = payload or {}
        if request == "settings.session":
            return {"rlmDepth": self.depth}
        if request == "settings.host":
            return {"rlmMaxDepth": {"maxDepth": self.max_depth, "source": "default"}}
        if request == "rlm.run":
            kwargs = payload["kwargs"]
            self.spawned.append(kwargs)
            for approach, content in self.edits.items():
                if f"Approach to take: {approach}\n" in payload["prompt"]:
                    (Path(kwargs["cwd"]) / "answer.txt").write_text(content)
            name = kwargs["name"]
            return {"rlm_child_id": f"id-{name}", "name": name, "session_dir": f"/sessions/{name}",
                    "model": "faux/model"}
        if request == "rlm.collect":
            return {"results": [
                {"rlm_child_id": target, "session_name": target[3:], "status": "done", "settled": True,
                 "answer_preview": "edited answer.txt"} for target in payload["targets"]]}
        if request == "hypotheses.record":
            self.records.append(payload)
        return {}


class HypothesesTest(unittest.TestCase):
    """Each test runs in a throwaway repository with its own agent dir and session id."""

    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name).resolve()
        self.repo = root / "repo"
        self.repo.mkdir()
        _git(self.repo, "init", "-q", "-b", "main")
        # A throwaway test repository's own identity (never this project's history).
        _git(self.repo, "config", "user.name", "Hypotheses Test")
        _git(self.repo, "config", "user.email", "hypotheses-test@example.invalid")
        (self.repo / "answer.txt").write_text("0\n")
        _git(self.repo, "add", ".")
        _git(self.repo, "commit", "-q", "-m", "base")
        env = patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent"),
                                      "RLM_SESSION_ID": "session-1"})
        env.start()
        self.addCleanup(env.stop)
        cwd = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, cwd)

    def run_with(self, host: FauxHost, coroutine_factory):
        with patch("rlm.host_request", host), patch("rlm.settings.host_request", host):
            return asyncio.run(coroutine_factory())

    def test_the_passing_approach_ranks_first_and_only_its_worktree_stays(self) -> None:
        host = FauxHost(depth=0, max_depth=2, edits={"off by one": "41\n", "the right fix": "42\n"})
        table = self.run_with(host, lambda: rlm.rlm.hypotheses(
            "make answer.txt hold the answer", ["off by one", "the right fix"], check="grep -qx 42 answer.txt"))
        winner = table.winner
        self.assertIsNotNone(winner)
        self.assertEqual(
            [(row.rank, row.approach, row.passed, row.check_exit, row.child_status, row.diff.files)
             for row in table.rows],
            [(1, "the right fix", True, 0, "done", ["answer.txt"]),
             (2, "off by one", False, 1, "done", ["answer.txt"])],
        )
        self.assertIn("+42", winner.diff.text)
        self.assertIsNone(table.rows[1].worktree)
        self.assertEqual(self.run_with(host, worktree.list), [winner.worktree])
        # One child per approach, each in its own checkout (never the parent's).
        cwds = {kwargs["cwd"] for kwargs in host.spawned}
        self.assertEqual((len(host.spawned), len(cwds), str(self.repo) in cwds), (2, 2, False))
        self.assertEqual(host.records, [{"approaches": 2, "passed": 1, "winner": True}])
        # The model brings the winner home with worktree.merge.
        merged = self.run_with(host, lambda: worktree.merge(winner.worktree))
        self.assertEqual(merged, worktree.MergeResult(status="applied", files=["answer.txt"]))
        self.assertEqual((self.repo / "answer.txt").read_text(), "42\n")

    def test_a_session_at_its_depth_bound_refuses_before_any_worktree(self) -> None:
        host = FauxHost(depth=2, max_depth=2, edits={})
        with self.assertRaisesRegex(RuntimeError, "rlm depth 2 of max 2"):
            self.run_with(host, lambda: hypothesis.hypotheses("t", ["a", "b"], check="true"))
        self.assertEqual((host.spawned, self.run_with(host, worktree.list)), ([], []))

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = FauxHost(depth=0, max_depth=2, edits={})
        for approaches, check, error in [
            (["only one"], "true", "2 to 6"),
            ([f"a{n}" for n in range(7)], "true", "2 to 6"),
            (["a", "a"], "true", "distinct"),
            (["a", "b"], 3, "check must be"),
        ]:
            with self.assertRaisesRegex((ValueError, TypeError), error):
                self.run_with(host, lambda: hypothesis.hypotheses("t", approaches, check=check))
        self.assertEqual(host.spawned, [])


if __name__ == "__main__":
    unittest.main()
