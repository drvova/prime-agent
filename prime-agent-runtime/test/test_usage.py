from __future__ import annotations

import asyncio
import unittest
from datetime import datetime, timezone
from unittest.mock import AsyncMock, call, patch

from rlm import usage

_SESSION = {
    "cost": {"total": 0.875, "own": 0.375, "subagents": 0.5},
    "tokens": {"input": 1400, "output": 280, "cacheRead": 9000, "cacheWrite": 500, "total": 11180},
}
_QUOTAS = {
    "providers": [
        {
            "provider": "github-copilot",
            "plan": "business",
            "source": "provider",
            "fetchedAtMs": 1,
            "state": {
                "kind": "windows",
                "windows": [
                    {"label": "Premium requests", "used": 300.0, "limit": 300.0, "unit": "requests", "resetsAtMs": 1796083200000}
                ],
            },
        },
        {"provider": "anthropic", "source": "provider", "fetchedAtMs": 1, "state": {"kind": "failed", "error": "did not answer"}},
    ]
}


class UsageClientTest(unittest.TestCase):
    def test_get_reads_the_session_then_the_quotas_and_parses_both(self) -> None:
        host = AsyncMock(side_effect=[_SESSION, _QUOTAS])
        with patch.object(usage, "host_request", host):
            result = asyncio.run(usage.get(refresh=True))
        self.assertEqual(
            host.await_args_list,
            [call("usage.session", {}), call("usage.quotas", {"refresh": True})],
        )
        self.assertEqual(
            result,
            usage.Usage(
                cost=usage.Cost(total=0.875, own=0.375, subagents=0.5),
                tokens=usage.Tokens(input=1400, output=280, cache_read=9000, cache_write=500, total=11180),
                quotas=[
                    usage.ProviderQuota(
                        provider="github-copilot",
                        plan="business",
                        state="windows",
                        windows=[
                            usage.QuotaWindow(
                                label="Premium requests",
                                used=300.0,
                                limit=300.0,
                                unit="requests",
                                resets_at=datetime(2026, 12, 1, tzinfo=timezone.utc),
                            )
                        ],
                        resets_at=None,
                        error=None,
                    ),
                    usage.ProviderQuota(
                        provider="anthropic", plan=None, state="failed", windows=[], resets_at=None, error="did not answer"
                    ),
                ],
            ),
        )
        self.assertEqual(result.quotas[0].windows[0].used_fraction, 1.0)
        self.assertEqual(
            str(result),
            "cost: $0.8750 (own $0.3750, subagents $0.5000)\n"
            "tokens: 11180 (input 1400, output 280, cache read 9000, cache write 500)\n"
            "github-copilot (business):\n"
            "  Premium requests: 300/300 requests, resets 2026-12-01T00:00:00+00:00\n"
            "anthropic: unavailable: did not answer",
        )

    def test_a_session_without_a_daemon_says_why_there_are_no_quotas(self) -> None:
        reply = {"providers": [], "unavailable": "no daemon"}
        host = AsyncMock(side_effect=[_SESSION, reply])
        with patch.object(usage, "host_request", host):
            result = asyncio.run(usage.get())
        self.assertEqual((result.quotas, result.quotas_unavailable), ([], "no daemon"))
        self.assertIn("quotas: no daemon", str(result))

    def test_refresh_must_be_a_bool(self) -> None:
        host = AsyncMock()
        with patch.object(usage, "host_request", host):
            with self.assertRaises(TypeError):
                asyncio.run(usage.get(refresh="yes"))
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
