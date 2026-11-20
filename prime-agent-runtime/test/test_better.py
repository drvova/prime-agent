from __future__ import annotations

import asyncio
import unittest
from typing import Any
from unittest.mock import patch

import rlm
from rlm import better as better_module

CHECKED = "Fix `parse_expr` in `src/parser.rs`; done when `cargo test -p parser` passes."


class HostRecorder:
    """Answers `better.run` with the checked brief and `rlm.run` with a spawn handle."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []

    async def __call__(self, request_type: str, payload: dict[str, Any]) -> dict[str, Any]:
        self.calls.append((request_type, payload))
        if request_type == "better.run":
            return {"rewrite": CHECKED, "unchanged": False}
        return {"rlm_child_id": "c1", "name": "parser", "session_dir": "/tmp/c1", "model": "test/m"}


class BetterClientTest(unittest.TestCase):
    def test_spawn_with_better_spawns_the_checked_brief(self) -> None:
        host = HostRecorder()
        with patch.object(better_module, "host_request", host), patch.object(rlm, "host_request", host):
            handle = asyncio.run(rlm.rlm.spawn("fix the parsr", name="parser", cwd="crates/parser", better=True))
        self.assertEqual(
            host.calls,
            [
                ("better.run", {"task": "fix the parsr", "mode": "brief", "cwd": "crates/parser"}),
                ("rlm.run", {"prompt": CHECKED, "kwargs": {"name": "parser", "cwd": "crates/parser"}}),
            ],
        )
        self.assertEqual(handle.rlm_child_id, "c1")

    def test_checks_and_evidence_ride_the_better_callable(self) -> None:
        calls: list[tuple[str, dict[str, Any]]] = []
        evidence = {"handle": "out:7f3a9c1e04", "round": 1, "passed": 1, "failed": 0, "checks": []}

        async def host(request_type: str, payload: dict[str, Any]) -> dict[str, Any]:
            calls.append((request_type, payload))
            if request_type == "better.checks":
                return {"status": "passed", "checks": [], "rounds": 0, "max_rounds": 3, "running": False, "last": evidence}
            return evidence

        with patch.object(better_module, "host_request", host):
            status = asyncio.run(better_module.better.checks())
            record = asyncio.run(better_module.better.evidence("out:7f3a9c1e04"))
        self.assertEqual((status["last"], record), (evidence, evidence))
        self.assertEqual(calls, [("better.checks", {}), ("better.evidence", {"handle": "out:7f3a9c1e04"})])

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = HostRecorder()
        with patch.object(better_module, "host_request", host), patch.object(rlm, "host_request", host):
            for call in (
                better_module.better("  "),
                better_module.better("x", mode="shorter"),
                better_module.evidence(""),
                rlm.spawn("x", name="n", better="yes"),  # type: ignore[arg-type]
            ):
                with self.assertRaises((ValueError, TypeError)):
                    asyncio.run(call)
        self.assertEqual(host.calls, [])


if __name__ == "__main__":
    unittest.main()
