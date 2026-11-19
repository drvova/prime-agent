from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, patch

from rlm import model
from rlm import notify as notify_module


class NotifyClientTest(unittest.TestCase):
    def test_notify_sends_the_ping_and_reports_delivery(self) -> None:
        host = AsyncMock(return_value={"delivered": True})
        with patch.object(notify_module, "host_request", host):
            delivered = asyncio.run(notify_module.notify("tests pass", level="success"))
        host.assert_awaited_once_with("notify.send", {"message": "tests pass", "level": "success"})
        self.assertIs(delivered, True)

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(notify_module, "host_request", host):
            with self.assertRaises(ValueError):
                asyncio.run(notify_module.notify("   "))
            with self.assertRaises(ValueError):
                asyncio.run(notify_module.notify("x", level="loud"))
        host.assert_not_awaited()


class SetEffortClientTest(unittest.TestCase):
    def test_set_effort_returns_the_effective_level(self) -> None:
        host = AsyncMock(return_value={"thinkingLevel": "medium"})
        with patch.object(model, "host_request", host):
            effective = asyncio.run(model.set_effort("max"))
        host.assert_awaited_once_with("model.set_effort", {"level": "max"})
        self.assertEqual(effective, "medium")

    def test_a_refused_switch_raises_the_host_reason_and_bad_levels_never_reach_it(self) -> None:
        host = AsyncMock(side_effect=RuntimeError("model.set_effort is off"))
        with patch.object(model, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "is off"):
                asyncio.run(model.set_effort("high"))
            with self.assertRaises(ValueError):
                asyncio.run(model.set_effort("turbo"))
        host.assert_awaited_once()


if __name__ == "__main__":
    unittest.main()
