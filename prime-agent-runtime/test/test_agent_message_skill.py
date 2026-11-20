from __future__ import annotations

import asyncio
import importlib.util
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch


SKILL = Path(__file__).parents[2] / "skills/agent-message/src/agent_message/__init__.py"


class AgentMessageSkillTest(unittest.TestCase):
    def test_roled_parent_and_broadcast_forms(self) -> None:
        spec = importlib.util.spec_from_file_location("agent_message_test", SKILL)
        assert spec and spec.loader
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        host = AsyncMock(return_value={"deliveryStatus": "queued"})
        with patch.object(module, "host_request", host), patch.object(module, "_emit_sent_message"):
            asyncio.run(module.send("done", receiver_role="parent"))
            asyncio.run(module.send("all", "follow up"))
        self.assertEqual(host.await_args_list[0].args[1]["receiver_role"], "parent")
        self.assertEqual(host.await_args_list[1].args[1]["target"], "all")

    def test_roled_selector_validation(self) -> None:
        spec = importlib.util.spec_from_file_location("agent_message_validation", SKILL)
        assert spec and spec.loader
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with self.assertRaisesRegex(ValueError, "required"):
            asyncio.run(module.send("hello", receiver_role="child"))
        with self.assertRaisesRegex(ValueError, "omitted"):
            asyncio.run(module.send("hello", receiver_role="parent", receiver_name="x"))
        with self.assertRaisesRegex(TypeError, "unexpected keyword argument 'mode'"):
            asyncio.run(module.send("hello", receiver_role="parent", mode="follow_up"))

    def test_a_wake_refused_receipt_is_labeled_refused(self) -> None:
        spec = importlib.util.spec_from_file_location("agent_message_wake_refused", SKILL)
        assert spec and spec.loader
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        receipt = {"deliveryStatus": "wake_refused", "retryAfterMs": 1000}
        host = AsyncMock(return_value=receipt)
        with patch.object(module, "host_request", host), patch("rlm.emit") as emit:
            asyncio.run(module.send("again", receiver_role="parent"))
        emit.assert_called_once_with(
            {
                "application/vnd.prime-agent.agent-message+json": {
                    **receipt,
                    "receiverRole": "parent",
                },
                "text/plain": "Agent message refused: wake budget spent",
            }
        )


if __name__ == "__main__":
    unittest.main()
