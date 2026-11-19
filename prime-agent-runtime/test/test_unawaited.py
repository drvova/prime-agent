from __future__ import annotations

import unittest

from test_repl import ReplProcess, one, stream_text


class UnawaitedKernelCallTest(unittest.TestCase):
    """A kernel coroutine the cell never awaits did not run: the cell says so
    and fails, instead of finishing ok with a coroutine repr."""

    def setUp(self) -> None:
        self.repl = ReplProcess()
        self.addCleanup(self.repl.close)
        self.repl.ready()

    def test_trailing_unawaited_call_fails_the_cell_with_the_fix(self) -> None:
        events = self.repl.execute("c1", "from rlm import todo\ntodo.set(['alpha', 'beta'])")

        self.assertEqual(
            [(e["event"], e.get("status")) for e in events if e["event"] in ("result", "host_request", "done")],
            [("done", "error")],
        )
        error = one(events, "error")
        assert error is not None
        self.assertIn("todo.set(...) was never awaited", error["evalue"])
        self.assertIn("await todo.set(...)", error["evalue"])

    def test_dropped_unawaited_call_mid_cell_fails_the_cell(self) -> None:
        events = self.repl.execute("c1", "from rlm import web\nweb.read('https://example.com')\nprint('after')")

        self.assertEqual(stream_text(events, "stdout"), "after\n")
        self.assertIsNone(one(events, "host_request"))
        self.assertEqual(events[-1]["status"], "error")
        error = one(events, "error")
        assert error is not None
        self.assertIn("web.read(...) was never awaited", error["evalue"])
        self.assertIn("cell line 2", error["evalue"])

    def test_cell_defined_coroutines_keep_their_behavior(self) -> None:
        events = self.repl.execute("c1", "async def f():\n    return 1\nc = f()\nc.close()\nf()")

        self.assertEqual(events[-1]["status"], "ok")
        self.assertIsNone(one(events, "error"))


if __name__ == "__main__":
    unittest.main()
