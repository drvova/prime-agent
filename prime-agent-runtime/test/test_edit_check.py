from __future__ import annotations

import asyncio
import importlib
import importlib.util
import json
import os
import re
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

edit_check = importlib.import_module("rlm.edit_check")
repl = importlib.import_module("rlm.repl")
bash_module = importlib.import_module("rlm.bash")


def _seconds_free(text: str | None) -> str | None:
    """The check line with its run time blanked (`· 0.1 s` -> `· N s`)."""
    return None if text is None else re.sub(r"\d+\.\d s", "N s", text)


class EditCheckTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(os.path.realpath(self._temp.name))
        self.agent_dir = self.root / "agent"
        self.project = self.root / "project"
        self.project.mkdir()
        self.edited = self.project / "app.py"
        self.edited.write_text("x = 1\n", encoding="utf-8")
        cwd = os.getcwd()
        os.chdir(self.project)
        self.addCleanup(os.chdir, cwd)
        self.addCleanup(self._temp.cleanup)
        env = mock.patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": str(self.agent_dir)})
        env.start()
        self.addCleanup(env.stop)
        # Every test starts from a cold project.
        learned = mock.patch.object(edit_check, "_learned", edit_check._Learned())
        learned.start()
        self.addCleanup(learned.stop)
        self.notes: list[dict] = []
        notify = mock.patch.object(repl, "host_notify", self.notes.append)
        notify.start()
        self.addCleanup(notify.stop)

    def _check(self, script: str) -> mock._patch:
        return mock.patch.object(
            edit_check,
            "_choose",
            lambda path, root: edit_check._Check("fake", ("sh", "-c", script), root),
        )

    async def _settled(self) -> None:
        while edit_check._tasks:
            await asyncio.gather(*list(edit_check._tasks))

    async def asyncTearDown(self) -> None:
        # The learned state saves in the background: let it land before the
        # temp dirs are removed (a save racing the cleanup failed it).
        await self._settled()

    async def test_a_check_that_ends_in_time_attaches_its_diagnostics_and_is_learned(self):
        with self._check("echo 'app.py:1:1: F821 undefined name'; exit 1"):
            attached = await edit_check.after_edit(str(self.edited))
        await self._settled()
        state = json.loads(next((self.agent_dir / "edit-checks").iterdir()).read_text())
        state["checks"]["fake"]["cost_ms"] = "learned"
        self.assertEqual(
            (_seconds_free(attached), state, self.notes),
            (
                "[check: sh -c echo 'app.py:1:1: F821 undefined name'; exit 1 · exit 1 · N s]\n"
                "app.py:1:1: F821 undefined name",
                {"schema": 1, "checks": {"fake": {"cost_ms": "learned", "found": 1.0, "runs": 1, "skips": 0}}},
                [{"type": "edit_check.fact", "outcome": "found", "kind": "fake"}],
            ),
        )

    async def test_a_slow_check_never_holds_the_edit_and_its_diagnostics_arrive_as_a_notice(self):
        requests: list[dict] = []

        async def host_request(data):
            requests.append(data)
            return {"status": "ok"}

        with (
            self._check("sleep 0.6; echo 'app.py:2: broken'; exit 2"),
            mock.patch.object(edit_check, "_COLD_BUDGET_MS", 50.0),
            mock.patch.object(repl, "host_request", host_request),
            mock.patch.object(repl, "is_active", lambda: True),
            mock.patch.object(bash_module, "_completion_notices_wired", True),
        ):
            started = time.monotonic()
            attached = await edit_check.after_edit(str(self.edited))
            waited = time.monotonic() - started
            await self._settled()
        for request in requests:
            request["pid"] = "the check's pid"
        self.assertEqual(
            (attached, waited < 0.5, requests, [note["outcome"] for note in self.notes]),
            (
                "[check: sh -c sleep 0.6; echo 'app.py:2: broken'; exit 2 · still running after 0.1 s; "
                "any diagnostics arrive as a [bash-done] notice]",
                True,
                [
                    {
                        "type": "bash.completed",
                        "pid": "the check's pid",
                        "command": "sh -c sleep 0.6; echo 'app.py:2: broken'; exit 2  # check after editing app.py",
                        "exitCode": 2,
                        "output": "app.py:2: broken",
                        "outputTruncated": False,
                    }
                ],
                ["late", "found"],
            ),
        )

    async def test_a_late_notice_the_host_never_answers_does_not_linger(self):
        never = asyncio.Event()

        async def host_request(data):
            await never.wait()

        with (
            self._check("sleep 0.2; echo diag; exit 1"),
            mock.patch.object(edit_check, "_COLD_BUDGET_MS", 10.0),
            mock.patch.object(edit_check, "_NOTICE_WAIT_SECONDS", 0.05),
            mock.patch.object(repl, "host_request", host_request),
            mock.patch.object(repl, "is_active", lambda: True),
            mock.patch.object(bash_module, "_completion_notices_wired", True),
        ):
            await edit_check.after_edit(str(self.edited))
            await asyncio.wait_for(self._settled(), 5)
        self.assertEqual([note["outcome"] for note in self.notes], ["late", "found"])

    async def test_both_edit_paths_carry_the_check(self):
        # `fs.edit` (anchored) and the `edit` skill (old_str/new_str) both return the touched
        # file's check with their own result.
        fs = importlib.import_module("rlm.fs")
        skill_spec = importlib.util.spec_from_file_location(
            "edit_skill_under_check", Path(__file__).parents[2] / "skills/edit/src/edit/__init__.py"
        )
        skill = importlib.util.module_from_spec(skill_spec)
        skill_spec.loader.exec_module(skill)
        reply = {"applied": True, "path": "app.py", "edits": 1, "removed": 1, "added": 1, "totalLines": 1, "hunks": [], "text": "app.py: 1 edit applied"}
        with self._check("echo 'app.py:1: E999'; exit 1"), mock.patch.object(fs, "host_request", mock.AsyncMock(return_value=reply)):
            anchored = await fs.edit("app.py", {"replace": "1:abc", "text": "x = 2"})
            retyped = await skill.run(str(self.edited), "x = 1", "x = 3")
        check = "[check: sh -c echo 'app.py:1: E999'; exit 1 · exit 1 · N s]\napp.py:1: E999"
        self.assertEqual(
            (_seconds_free(anchored.check), _seconds_free(anchored.text), _seconds_free(retyped)),
            (check, f"app.py: 1 edit applied\n{check}", f"Edited {self.edited}\n{check}"),
        )

    async def test_a_file_outside_the_project_gets_no_check(self):
        outside = self.root / "outside.py"
        outside.write_text("x = (\n", encoding="utf-8")
        with self._check("exit 1"):
            self.assertEqual(await edit_check.after_edit(str(outside)), None)


class LearnedStatsTest(unittest.TestCase):
    def test_a_costly_fruitless_check_is_dropped_then_reprobed(self):
        # Three 30 s runs that never find anything cost more than the round they could save: the
        # check is dropped, except every eighth edit, which runs it again to keep learning.
        stats = edit_check._Stats()
        for _ in range(3):
            stats.observe(30_000.0, found=False)
        decisions = []
        for _ in range(9):
            run = stats.worth_running()
            decisions.append(run)
            if run:
                stats.observe(30_000.0, found=False)
            else:
                stats.skips += 1
        self.assertEqual(decisions, [False] * 7 + [True, False])

    def test_a_cheap_check_that_finds_problems_keeps_running(self):
        stats = edit_check._Stats()
        for found in (True, False, True, False, False):
            stats.observe(800.0, found=found)
        self.assertEqual((stats.worth_running(), round(stats.budget_seconds(), 2)), (True, 1.2))


class ChooseTest(unittest.TestCase):
    def test_each_language_gets_its_cheapest_check(self):
        with tempfile.TemporaryDirectory() as temp:
            root = os.path.realpath(temp)
            Path(root, "crates/core/src").mkdir(parents=True)
            Path(root, "crates/core/Cargo.toml").write_text('[package]\nname = "core"\n')
            Path(root, "web/src").mkdir(parents=True)
            Path(root, "web/tsconfig.json").write_text("{}")
            Path(root, "svc/pkg").mkdir(parents=True)
            Path(root, "svc/go.mod").write_text("module example.com/svc\n")
            tools = {"ruff": "/bin/ruff", "cargo": "/bin/cargo", "tsc": "/bin/tsc", "go": "/bin/go"}
            with mock.patch.object(edit_check.shutil, "which", tools.get):
                chosen = {
                    name: edit_check._choose(os.path.join(root, name), root)
                    for name in ("a.py", "crates/core/src/lib.rs", "web/src/x.ts", "svc/pkg/y.go", "notes.md")
                }
            self.assertEqual(
                chosen,
                {
                    "a.py": edit_check._Check(
                        "ruff",
                        ("/bin/ruff", "check", "--isolated", "--quiet", "--output-format", "concise", "--select", "E9,F63,F7,F82", os.path.join(root, "a.py")),
                        root,
                    ),
                    "crates/core/src/lib.rs": edit_check._Check(
                        "cargo", ("/bin/cargo", "check", "--quiet", "--message-format", "short", "-p", "core"), root
                    ),
                    "web/src/x.ts": edit_check._Check(
                        "tsc", ("/bin/tsc", "--noEmit", "--pretty", "false", "-p", os.path.join(root, "web")), root
                    ),
                    "svc/pkg/y.go": edit_check._Check("go_vet", ("/bin/go", "vet", "./pkg"), os.path.join(root, "svc")),
                    "notes.md": None,
                },
            )


if __name__ == "__main__":
    unittest.main()
