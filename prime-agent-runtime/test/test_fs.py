from __future__ import annotations

import asyncio
import ntpath
import os
import unittest
from unittest.mock import AsyncMock, patch

from rlm import fs

_GREP_REPLY = {
    "matches": [{"path": "src/lib.rs", "line": 2, "column": 8, "text": "pub fn alpha() {}", "before": ["// lib"], "after": []}],
    "filesSearched": 2,
    "filesMatched": 1,
    "truncated": False,
    "unreadable": 0,
    "text": "src/lib.rs\n1-// lib\n2:pub fn alpha() {}\n[1 match in 1 file (2 searched)]",
}


class FsClientTest(unittest.TestCase):
    def test_grep_resolves_paths_in_the_kernel_and_parses_the_reply(self) -> None:
        host = AsyncMock(return_value=_GREP_REPLY)
        with patch.object(fs, "host_request", host):
            result = asyncio.run(fs.grep("alpha", "src", glob="*.rs", type=["rust"], case="smart", context=1, after=0))
        host.assert_awaited_once_with(
            "fs.grep",
            {
                "pattern": "alpha",
                "root": os.path.abspath("src"),
                "cwd": os.getcwd(),
                "case": "smart",
                "literal": False,
                "word": False,
                "multiline": False,
                "before": 1,
                "after": 0,
                "hidden": False,
                "gitignore": True,
                "maxResults": 100,
                "maxColumns": 300,
                "globs": ["*.rs"],
                "types": ["rust"],
            },
        )
        self.assertEqual(
            result,
            fs.GrepResult(
                matches=[fs.GrepMatch("src/lib.rs", 2, 8, "pub fn alpha() {}", ["// lib"], [])],
                files_searched=2,
                files_matched=1,
                truncated=False,
                text=_GREP_REPLY["text"],
            ),
        )
        self.assertEqual(str(result), _GREP_REPLY["text"])

    def test_find_sends_its_filters_and_lists_entries(self) -> None:
        reply = {"entries": [{"path": "Cargo.toml", "kind": "file", "size": 24}], "truncated": False, "text": "Cargo.toml"}
        host = AsyncMock(return_value=reply)
        with patch.object(fs, "host_request", host):
            listing = asyncio.run(fs.find("Cargo.toml", kind="file", max_depth=2))
        host.assert_awaited_once_with(
            "fs.find",
            {
                "root": os.getcwd(),
                "cwd": os.getcwd(),
                "ignoreCase": False,
                "hidden": False,
                "gitignore": True,
                "maxResults": 500,
                "name": "Cargo.toml",
                "kind": "file",
                "maxDepth": 2,
            },
        )
        self.assertEqual(listing.paths, ["Cargo.toml"])

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(fs, "host_request", host):
            with self.assertRaises(TypeError):
                asyncio.run(fs.grep(""))
            with self.assertRaisesRegex(ValueError, "case"):
                asyncio.run(fs.grep("x", case="loud"))
            with self.assertRaisesRegex(ValueError, "kind"):
                asyncio.run(fs.find(kind="socket"))
        host.assert_not_awaited()


_READ_REPLY = {
    "path": "src/app.py",
    "start": 1,
    "end": 2,
    "totalLines": 2,
    "lines": [
        {"line": 1, "anchor": "1:qwe", "text": "def run():"},
        {"line": 2, "anchor": "2:rty", "text": "    return 1"},
    ],
    "truncated": False,
    "lineEnds": "crlf",
    "encoding": "utf-8",
    "finalNewline": True,
    "text": "src/app.py (lines 1-2 of 2, crlf)\n1:qwe|def run():\n2:rty|    return 1",
}


class FsHashlineClientTest(unittest.TestCase):
    def test_read_resolves_the_path_and_parses_anchored_lines(self) -> None:
        host = AsyncMock(return_value=_READ_REPLY)
        with patch.object(fs, "host_request", host):
            view = asyncio.run(fs.read("src/app.py", start=1, end=2))
        host.assert_awaited_once_with(
            "fs.read", {"path": os.path.abspath("src/app.py"), "cwd": os.getcwd(), "start": 1, "end": 2}
        )
        self.assertEqual(
            view,
            fs.FileView(
                path="src/app.py",
                lines=[fs.AnchoredLine(1, "1:qwe", "def run():"), fs.AnchoredLine(2, "2:rty", "    return 1")],
                total_lines=2,
                start=1,
                end=2,
                truncated=False,
                line_ends="crlf",
                encoding="utf-8",
                final_newline=True,
                text=_READ_REPLY["text"],
            ),
        )
        self.assertEqual((str(view), view.anchor(2)), (_READ_REPLY["text"], "2:rty"))

    def test_edit_sends_wire_edits_and_returns_fresh_anchors(self) -> None:
        reply = {
            "applied": True,
            "path": "src/app.py",
            "edits": 3,
            "removed": 1,
            "added": 3,
            "totalLines": 4,
            "hunks": [{"start": 1, "end": 2, "lines": [{"line": 1, "anchor": "1:abc", "text": "x"}]}],
            "text": "src/app.py: 3 edits applied, -1 +3 lines, 4 total\n1:abc|x",
        }
        host = AsyncMock(return_value=reply)
        line = fs.AnchoredLine(2, "2:rty", "    return 1")
        with patch.object(fs, "host_request", host):
            result = asyncio.run(
                fs.edit(
                    "src/app.py",
                    [
                        {"replace": ["1:qwe", line], "text": ["def run():", "    return 2"]},
                        {"insert_before": "1:qwe", "text": "# app\n"},
                        {"append": "run()"},
                    ],
                )
            )
        host.assert_awaited_once_with(
            "fs.edit",
            {
                "path": os.path.abspath("src/app.py"),
                "cwd": os.getcwd(),
                "edits": [
                    {"op": "replace", "anchor": "1:qwe", "end": "2:rty", "text": ["def run():", "    return 2"]},
                    {"op": "insert_before", "anchor": "1:qwe", "text": "# app\n"},
                    {"op": "append", "text": "run()"},
                ],
            },
        )
        self.assertEqual(
            result,
            fs.EditResult(
                path="src/app.py",
                edits=3,
                removed=1,
                added=3,
                total_lines=4,
                hunks=[[fs.AnchoredLine(1, "1:abc", "x")]],
                text=reply["text"],
            ),
        )

    def test_a_stale_patch_raises_with_the_report(self) -> None:
        reply = {
            "applied": False,
            "path": "src/app.py",
            "stale": [{"edit": 1, "anchor": "2:rty", "current": "2:uio"}, {"edit": 2, "anchor": "9:zzz", "current": None}],
            "lines": [{"line": 2, "anchor": "2:uio", "text": "    return 3"}],
            "text": "fs.edit rejected, nothing was written: 2 of 2 anchors in src/app.py no longer match the file.",
        }
        with patch.object(fs, "host_request", AsyncMock(return_value=reply)):
            with self.assertRaises(fs.StaleAnchorsError) as caught:
                asyncio.run(fs.edit("src/app.py", [{"delete": "2:rty"}, {"insert_after": "9:zzz", "text": "x"}]))
        error = caught.exception
        self.assertEqual(
            (str(error), error.stale, error.lines),
            (
                reply["text"],
                [fs.StaleAnchor(1, "2:rty", "2:uio"), fs.StaleAnchor(2, "9:zzz", None)],
                [fs.AnchoredLine(2, "2:uio", "    return 3")],
            ),
        )
        self.assertIsInstance(error, RuntimeError)

    def test_a_windows_path_resolves_against_the_kernel_cwd(self) -> None:
        host = AsyncMock(return_value=_READ_REPLY)
        with (
            patch.object(fs, "host_request", host),
            patch.object(fs.os, "path", ntpath),
            patch.object(fs.os, "getcwd", return_value="C:\\proj"),
        ):
            asyncio.run(fs.read("src\\app.py"))
            asyncio.run(fs.read("D:\\other\\x.py"))
        self.assertEqual(
            [call.args for call in host.await_args_list],
            [
                ("fs.read", {"path": "C:\\proj\\src\\app.py", "cwd": "C:\\proj"}),
                ("fs.read", {"path": "D:\\other\\x.py", "cwd": "C:\\proj"}),
            ],
        )

    def test_malformed_edits_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(fs, "host_request", host):
            with self.assertRaisesRegex(ValueError, "exactly one"):
                asyncio.run(fs.edit("a.py", [{"replace": "1:abc", "delete": "2:def"}]))
            with self.assertRaisesRegex(ValueError, "unknown keys"):
                asyncio.run(fs.edit("a.py", {"replace": "1:abc", "txt": "x"}))
            with self.assertRaisesRegex(ValueError, "range"):
                asyncio.run(fs.edit("a.py", {"delete": ["1:abc"]}))
            with self.assertRaises(TypeError):
                asyncio.run(fs.edit("a.py", []))
            with self.assertRaisesRegex(ValueError, "start"):
                asyncio.run(fs.read("a.py", start=0))
        host.assert_not_awaited()


class FsSymbolClientTest(unittest.TestCase):
    def test_symbol_sends_the_name_and_parses_definitions_and_references(self) -> None:
        reply = {
            "definitions": [{"path": "src/store.rs", "line": 4, "kind": "fn", "text": "pub fn load() {", "handle": "out:00000000ab"}],
            "references": [{"path": "src/main.rs", "line": 9, "text": "store.load();"}],
            "truncated": True,
            "partial": False,
        }
        host = AsyncMock(return_value=reply)
        with patch.object(fs, "host_request", host):
            result = asyncio.run(fs.symbol("load", max_references=1))
        host.assert_awaited_once_with("fs.symbol", {"name": "load", "maxReferences": 1})
        self.assertEqual(
            (result, str(result)),
            (
                fs.SymbolResult(
                    name="load",
                    definitions=[fs.SymbolLocation("src/store.rs", 4, "pub fn load() {", "fn", "out:00000000ab")],
                    references=[fs.SymbolLocation("src/main.rs", 9, "store.load();")],
                    truncated=True,
                    partial=False,
                ),
                "load: 1 definitions, 1+ references\n"
                "def src/store.rs:4 [fn] pub fn load() { · out:00000000ab\n"
                "ref src/main.rs:9 store.load();",
            ),
        )


if __name__ == "__main__":
    unittest.main()
