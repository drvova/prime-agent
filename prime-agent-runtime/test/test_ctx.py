from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, call, patch

from rlm import ctx

_STORED = {"handle": "out:7f3a9c1e04", "state": "stored", "totalChars": 48211, "lines": 1204, "storedLines": 1204}


class CtxClientTest(unittest.TestCase):
    def run_with(self, replies, coro_factory):
        host = AsyncMock(side_effect=replies)
        with patch.object(ctx, "host_request", host):
            return host, asyncio.run(coro_factory())

    def test_page_sends_its_window_and_parses_the_slice(self) -> None:
        reply = {**_STORED, "start": 600, "end": 602, "text": "a\nb\n", "truncated": False}
        host, page = self.run_with([reply], lambda: ctx.page("out:7f3a9c1e04", start=600, lines=2))
        self.assertEqual(host.await_args_list, [call("ctx.page", {"handle": "out:7f3a9c1e04", "start": 600, "lines": 2})])
        self.assertEqual(
            page, ctx.Page(handle="out:7f3a9c1e04", start=600, end=602, lines=1204, text="a\nb\n", truncated=False)
        )
        self.assertEqual(str(page), "[out:7f3a9c1e04 lines 600..602 of 1204]\na\nb\n")

    def test_tail_and_grep_use_their_requests(self) -> None:
        tail_reply = {**_STORED, "start": 1203, "end": 1204, "text": "done\n", "truncated": False}
        grep_reply = {**_STORED, "pattern": "err", "matches": 3, "shown": 1, "text": "4:err\n", "truncated": True}
        host = AsyncMock(side_effect=[tail_reply, grep_reply])
        with patch.object(ctx, "host_request", host):
            tail = asyncio.run(ctx.tail("out:7f3a9c1e04", 1))
            matches = asyncio.run(ctx.grep("out:7f3a9c1e04", "err", context=0, max_matches=1))
        self.assertEqual(
            host.await_args_list,
            [
                call("ctx.tail", {"handle": "out:7f3a9c1e04", "n": 1}),
                call("ctx.grep", {"handle": "out:7f3a9c1e04", "pattern": "err", "context": 0, "max_matches": 1}),
            ],
        )
        self.assertEqual(
            (tail, matches),
            (
                ctx.Page(handle="out:7f3a9c1e04", start=1203, end=1204, lines=1204, text="done\n", truncated=False),
                ctx.Matches(handle="out:7f3a9c1e04", pattern="err", count=3, shown=1, text="4:err\n", truncated=True),
            ),
        )

    def test_an_evicted_handle_raises_a_clear_error_and_info_reports_it(self) -> None:
        evicted = {"handle": "out:7f3a9c1e04", "state": "evicted"}
        host = AsyncMock(side_effect=[evicted, evicted])
        with patch.object(ctx, "host_request", host):
            with self.assertRaises(ctx.PagedOutUnavailable) as raised:
                asyncio.run(ctx.page("out:7f3a9c1e04"))
            info = asyncio.run(ctx.info("out:7f3a9c1e04"))
        self.assertEqual(
            str(raised.exception),
            "out:7f3a9c1e04 was evicted from this session's spill store (it keeps the newest 64 MiB); "
            "re-run the command to see it again",
        )
        self.assertEqual(info, ctx.Info(handle="out:7f3a9c1e04", state="evicted"))

    def test_a_not_stored_handle_names_its_reason(self) -> None:
        reply = {"handle": "out:7f3a9c1e04", "state": "not_stored", "reason": "the spill writer is busy"}
        _, info = self.run_with([reply], lambda: ctx.info("out:7f3a9c1e04"))
        self.assertEqual(
            str(info), "out:7f3a9c1e04 was not stored (the spill writer is busy); re-run the command to see it"
        )

    def test_bad_arguments_fail_before_any_request(self) -> None:
        host = AsyncMock()
        with patch.object(ctx, "host_request", host):
            for bad in (
                lambda: ctx.page(7),
                lambda: ctx.page("out:7f3a9c1e04", start=-1),
                lambda: ctx.tail("out:7f3a9c1e04", True),
                lambda: ctx.grep("out:7f3a9c1e04", None),
            ):
                with self.assertRaises((TypeError, ValueError)):
                    asyncio.run(bad())
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
