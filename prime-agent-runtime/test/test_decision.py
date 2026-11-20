from __future__ import annotations

import asyncio
import contextlib
import io
import unittest
from unittest.mock import AsyncMock, call, patch

import rlm
from rlm import decision


class DecideClientTest(unittest.TestCase):
    def test_a_scripted_loop_pays_one_host_call_per_decision_and_prints_each_row(self) -> None:
        replies = [{"index": 1, "row": f"[decide] attempt {n}? → retry"} for n in range(3)]
        replies.append({"index": 0, "row": "[decide] attempt 3? → stop"})
        host = AsyncMock(side_effect=replies)

        async def loop() -> list[str]:
            picked = []
            for attempt in range(10):
                # Kernel-speed work between decisions costs no model call.
                failing = attempt < 4
                if not failing:
                    break
                choice = await rlm.rlm.decide(f"attempt {attempt}?", ["stop", "retry"], context="1 failed")
                picked.append(choice)
                if choice == "stop":
                    break
            return picked

        out = io.StringIO()
        with patch.object(decision, "host_request", host), contextlib.redirect_stdout(out):
            picked = asyncio.run(loop())
        self.assertEqual(picked, ["retry", "retry", "retry", "stop"])
        self.assertEqual(
            host.await_args_list,
            [
                call(
                    "rlm.decide",
                    {"question": f"attempt {n}?", "options": ["stop", "retry"], "context": "1 failed", "cell": None},
                )
                for n in range(4)
            ],
        )
        self.assertEqual(
            out.getvalue().splitlines(),
            ["[decide] attempt 0? → retry", "[decide] attempt 1? → retry", "[decide] attempt 2? → retry", "[decide] attempt 3? → stop"],
        )

    def test_the_host_refusal_at_the_bound_raises_verbatim(self) -> None:
        host = AsyncMock(side_effect=RuntimeError("rlm.decide: this cell used its 8 decisions"))
        with patch.object(decision, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "used its 8 decisions"):
                asyncio.run(decision.decide("next?", ["a", "b"]))

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(decision, "host_request", host):
            for question, options, context in [
                ("  ", ["a", "b"], None),
                ("q", ["a"], None),
                ("q", "ab", None),
                ("q", ["a", "a"], None),
                ("q", ["a", ""], None),
                ("q", [f"o{n}" for n in range(17)], None),
                ("q", ["a", "b"], "x" * 8_001),
            ]:
                with self.assertRaises((ValueError, TypeError)):
                    asyncio.run(decision.decide(question, options, context=context))
        host.assert_not_awaited()

    def test_an_index_outside_the_options_is_an_error(self) -> None:
        host = AsyncMock(return_value={"index": 2, "row": "[decide] q → ?"})
        with patch.object(decision, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "unexpected result"):
                asyncio.run(decision.decide("q", ["a", "b"]))


if __name__ == "__main__":
    unittest.main()
