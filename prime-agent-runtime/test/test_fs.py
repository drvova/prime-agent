from __future__ import annotations

import asyncio
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


if __name__ == "__main__":
    unittest.main()
