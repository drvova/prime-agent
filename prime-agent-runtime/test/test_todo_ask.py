from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, patch

from rlm import ask as ask_module
from rlm import todo

_LIST = {
    "items": [
        {"id": "1", "text": "read", "status": "done"},
        {"id": "2", "text": "write", "status": "in_progress", "note": "half"},
    ]
}


class TodoClientTest(unittest.TestCase):
    def test_set_sends_items_and_parses_the_list(self) -> None:
        host = AsyncMock(return_value=_LIST)
        with patch.object(todo, "host_request", host):
            result = asyncio.run(todo.set(["read", {"text": "write", "status": "in_progress", "note": "half"}]))
        host.assert_awaited_once_with(
            "todo.set",
            {"items": [{"text": "read"}, {"text": "write", "status": "in_progress", "note": "half"}]},
        )
        self.assertEqual(
            result,
            todo.TodoList(
                items=[
                    todo.TodoItem("1", "read", "done"),
                    todo.TodoItem("2", "write", "in_progress", "half"),
                ]
            ),
        )
        self.assertEqual(str(result), "todo list: 1/2 done\n[x] 1. read\n[~] 2. write (half)")

    def test_update_and_get_send_their_requests(self) -> None:
        host = AsyncMock(return_value=_LIST)
        with patch.object(todo, "host_request", host):
            asyncio.run(todo.update("2", status="done", note=""))
            asyncio.run(todo.get())
        self.assertEqual(
            host.await_args_list,
            [
                unittest.mock.call("todo.update", {"id": "2", "status": "done", "note": ""}),
                unittest.mock.call("todo.get", {}),
            ],
        )

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(todo, "host_request", host):
            with self.assertRaisesRegex(ValueError, "status"):
                asyncio.run(todo.set([{"text": "x", "status": "finished"}]))
            with self.assertRaisesRegex(ValueError, "unknown todo field"):
                asyncio.run(todo.set([{"text": "x", "owner": "me"}]))
            with self.assertRaisesRegex(ValueError, "at least one"):
                asyncio.run(todo.update("1"))
            with self.assertRaises(TypeError):
                asyncio.run(todo.set("one string"))
        host.assert_not_awaited()


class AskClientTest(unittest.TestCase):
    def test_single_choice_answer_is_a_string(self) -> None:
        host = AsyncMock(return_value={"status": "answered", "answer": ["redis"]})
        with patch.object(ask_module, "host_request", host), patch.object(ask_module.os, "urandom", return_value=b"\x01" * 8):
            result = asyncio.run(ask_module.ask("Store?", ["sqlite", "redis"], default="sqlite", timeout=30))
        host.assert_awaited_once_with(
            "question.ask",
            {
                "id": "0101010101010101",
                "text": "Store?",
                "kind": "single",
                "choices": ["sqlite", "redis"],
                "default": ["sqlite"],
                "timeoutSeconds": 30,
            },
        )
        self.assertEqual(result, ask_module.AskResult(status="answered", answer="redis"))

    def test_no_user_returns_the_default_and_multi_answers_are_lists(self) -> None:
        host = AsyncMock(return_value={"status": "no_user", "answer": ["a", "b"]})
        with patch.object(ask_module, "host_request", host):
            result = asyncio.run(ask_module.ask("Which?", ["a", "b", "c"], multi=True, default=["a", "b"]))
        self.assertEqual(result, ask_module.AskResult(status="no_user", answer=["a", "b"]))
        declined = AsyncMock(return_value={"status": "declined", "answer": []})
        with patch.object(ask_module, "host_request", declined):
            self.assertEqual(
                asyncio.run(ask_module.ask("Name?")),
                ask_module.AskResult(status="declined", answer=None),
            )

    def test_an_interrupted_ask_withdraws_the_question(self) -> None:
        host = AsyncMock(side_effect=KeyboardInterrupt)
        sent: list[dict[str, object]] = []
        with (
            patch.object(ask_module, "host_request", host),
            patch.object(ask_module.os, "urandom", return_value=b"\x02" * 8),
            patch("rlm.repl.is_active", return_value=True),
            patch("rlm.repl.host_notify", side_effect=sent.append),
        ):
            with self.assertRaises(KeyboardInterrupt):
                asyncio.run(ask_module.ask("Proceed?", ["yes", "no"]))
        self.assertEqual(sent, [{"type": "question.cancel", "id": "0202020202020202"}])

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(ask_module, "host_request", host):
            with self.assertRaisesRegex(ValueError, "multi needs choices"):
                asyncio.run(ask_module.ask("x", multi=True))
            with self.assertRaisesRegex(TypeError, "default"):
                asyncio.run(ask_module.ask("x", ["a", "b"], default=["a"]))
            with self.assertRaisesRegex(ValueError, "timeout"):
                asyncio.run(ask_module.ask("x", timeout=0))
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
