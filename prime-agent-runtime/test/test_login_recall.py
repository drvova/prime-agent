from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, call, patch

import rlm as rlm_module
from rlm import mcp


class McpLoginTest(unittest.TestCase):
    def test_a_completed_login_reloads_the_server(self) -> None:
        host = AsyncMock(return_value={"status": "logged_in"})
        reload = AsyncMock()
        with patch.object(mcp, "host_request", host), patch.object(mcp, "reload", reload):
            result = asyncio.run(mcp.login("linear"))
        host.assert_awaited_once_with("mcp.begin_login", {"server": "linear"})
        reload.assert_awaited_once_with("linear")
        self.assertEqual(result, {"server": "linear", "status": "logged_in", "message": "logged in to linear"})

    def test_unavailable_and_failed_logins_report_a_status_without_host_text(self) -> None:
        unavailable = AsyncMock(return_value={"status": "unavailable", "message": "ask the user to run /mcp login linear"})
        with patch.object(mcp, "host_request", unavailable):
            self.assertEqual(
                asyncio.run(mcp.login("linear")),
                {"server": "linear", "status": "unavailable", "message": "ask the user to run /mcp login linear"},
            )
        failing = AsyncMock(side_effect=RuntimeError("token endpoint https://x/?code=SECRET said no"))
        with patch.object(mcp, "host_request", failing):
            result = asyncio.run(mcp.login("linear"))
        self.assertEqual(result["status"], "failed")
        self.assertNotIn("SECRET", result["message"])
        with self.assertRaises(TypeError):
            asyncio.run(mcp.login(""))


_HIT = {
    "session_id": "s-old",
    "title": "Why is the parser test flaky?",
    "created_at": "2026-01-02T03:04:05.000Z",
    "updated_at": "2026-01-03T00:00:00.000Z",
    "match_count": 2,
    "message_count": 3,
    "snippets": [{"index": 0, "role": "user", "text": "Why is the parser test flaky?"}],
}


class SessionSearchTest(unittest.TestCase):
    def test_search_parses_hits_and_a_hit_reads_through_messages(self) -> None:
        row = {"index": 0, "id": "a", "message": {"role": "user", "content": "Why is the parser test flaky?"}}
        host = AsyncMock(side_effect=[{"hits": [_HIT]}, {"messages": [row], "total": 1, "agent": _HIT["title"]}])
        emitted = []
        with patch.object(rlm_module, "host_request", host), patch.object(rlm_module, "emit", emitted.append):
            hits = asyncio.run(rlm_module.rlm.session.search("flaky parser", limit=5))
            messages = asyncio.run(rlm_module.rlm.session.messages(start=0, limit=1, session=hits[0]))
        self.assertEqual(
            hits,
            [
                rlm_module.RLMSessionHit(
                    session_id="s-old",
                    title="Why is the parser test flaky?",
                    created_at="2026-01-02T03:04:05.000Z",
                    updated_at="2026-01-03T00:00:00.000Z",
                    match_count=2,
                    message_count=3,
                    snippets=[rlm_module.RLMSessionSnippet(index=0, role="user", text="Why is the parser test flaky?")],
                )
            ],
        )
        self.assertEqual([message.text for message in messages], ["Why is the parser test flaky?"])
        self.assertEqual(
            host.await_args_list,
            [
                call("rlm.session.search", {"query": "flaky parser", "limit": 5}),
                call("rlm.session.saved_messages", {"start": 0, "limit": 1, "roles": ["*"], "session": "s-old"}),
            ],
        )

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(rlm_module, "host_request", host):
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.rlm.session.search("  "))
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.rlm.session.search("x", limit=0))
            with self.assertRaisesRegex(ValueError, "not both"):
                asyncio.run(rlm_module.rlm.session.messages(agent="child", session="s-old"))
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
