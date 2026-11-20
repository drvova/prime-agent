from __future__ import annotations

import asyncio
import importlib
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import bash, fs

command_cache = importlib.import_module("rlm.command_cache")
repl = importlib.import_module("rlm.repl")


class AllowlistTest(unittest.TestCase):
    def test_only_simple_read_only_commands_are_cacheable(self):
        commands = {
            "git status": True,
            "git log --oneline -5": True,
            "git diff HEAD~1": False,  # `~` is shell expansion
            "git diff --stat": True,
            "ls -la src": True,
            "cat a.txt b.txt": True,
            "rg -n foo src": True,
            "grep -rn needle .": True,
            "git commit -m x": False,
            "git diff --output=patch.txt": False,
            "rg --pre ./decode.sh foo": False,
            "grep needle": False,  # reads stdin
            "ls > listing.txt": False,
            "cat a.txt | wc -l": False,
            "git status; rm -rf build": False,
            "cat $(ls)": False,
            "rg foo *.py": False,
            "echo hi": False,
            "cargo test": False,
        }
        self.assertEqual({command: command_cache.plan(command) is not None for command in commands}, commands)


class CacheTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(os.path.realpath(self._temp.name))
        (self.root / "a.txt").write_text("first\n", encoding="utf-8")
        cwd = os.getcwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, cwd)
        self.addCleanup(self._temp.cleanup)
        command_cache.invalidate()
        self.notes: list[dict] = []
        notify = mock.patch.object(repl, "host_notify", self.notes.append)
        notify.start()
        self.addCleanup(notify.stop)

    def _rewrite(self, text: str) -> None:
        path = self.root / "a.txt"
        before = path.stat().st_mtime_ns
        path.write_text(text, encoding="utf-8")
        # A write inside one mtime tick still changes the size; keep mtime moving too.
        os.utime(path, ns=(before + 1_000_000, before + 1_000_000))

    async def test_a_repeated_read_answers_cached_until_its_file_changes_or_an_edit_lands(self):
        first = await bash("cat a.txt")
        again = await bash("cat a.txt")
        self._rewrite("second line, longer\n")
        changed = await bash("cat a.txt")
        repeat = await bash("cat a.txt")
        command_cache.invalidate()  # what fs.edit and the edit skill do before writing
        after_edit = await bash("cat a.txt")
        self.assertEqual(
            [(result.output, result.cached) for result in (first, again, changed, repeat, after_edit)],
            [
                ("first\n", False),
                ("first\n", True),
                ("second line, longer\n", False),
                ("second line, longer\n", True),
                ("second line, longer\n", False),
            ],
        )
        self.assertEqual(
            [note["hit"] for note in self.notes if note["source"] == "bash"],
            [False, True, False, True, False],
        )

    async def test_a_command_with_side_effects_always_runs(self):
        for _ in range(2):
            result = await bash("echo x >> log.txt")
            self.assertFalse(result.cached)
        self.assertEqual((self.root / "log.txt").read_text(), "x\nx\n")

    async def test_an_unchanged_fs_range_read_answers_cached(self):
        reply = {
            "path": "a.txt",
            "lines": [{"line": 1, "anchor": "1:abc", "text": "first"}],
            "totalLines": 1,
            "start": 1,
            "end": 1,
            "truncated": False,
            "lineEnds": "lf",
            "encoding": "utf-8",
            "finalNewline": True,
            "text": "a.txt (lines 1-1 of 1)\n1:abc|first",
        }
        host = mock.AsyncMock(return_value=reply)
        with mock.patch.object(fs, "host_request", host):
            first = await fs.read("a.txt", start=1, end=1)
            second = await fs.read("a.txt", start=1, end=1)
            self._rewrite("first, changed\n")
            third = await fs.read("a.txt", start=1, end=1)
        self.assertEqual(
            (host.await_count, [view.cached for view in (first, second, third)], second.text),
            (2, [False, True, False], "a.txt (lines 1-1 of 1) · cached\n1:abc|first"),
        )

    async def test_a_whole_file_read_always_asks_the_host(self):
        # The host answers a re-read of a file this session has seen as "unchanged" or a diff,
        # and knows when a compaction dropped it; a cached full view would show it all again.
        reply = {
            "path": "a.txt",
            "lines": [{"line": 1, "anchor": "1:abc", "text": "first"}],
            "totalLines": 1,
            "start": 1,
            "end": 1,
            "truncated": False,
            "text": "a.txt (unchanged since turn 1, 1 lines; whole view: out:0123456789)",
        }
        host = mock.AsyncMock(return_value=reply)
        with mock.patch.object(fs, "host_request", host):
            views = [await fs.read("a.txt") for _ in range(2)]
        self.assertEqual(
            (host.await_count, [(view.cached, view.text) for view in views]),
            (2, [(False, reply["text"])] * 2),
        )


if __name__ == "__main__":
    unittest.main()
