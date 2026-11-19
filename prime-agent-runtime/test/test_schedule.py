from __future__ import annotations

import asyncio
import datetime as dt
import unittest
from unittest.mock import AsyncMock, call, patch

from rlm import schedule

_JOB = {
    "id": "job-1",
    "prompt": "check CI",
    "label": None,
    "kind": "once",
    "schedule": "at 2030-01-02T03:04:05Z",
    "status": "active",
    "next_run_at": "2030-01-02T03:04:05.000Z",
    "last_run_at": None,
    "run_count": 0,
    "last_error": None,
    "created_at": "2029-12-31T00:00:00.000Z",
}

_PARSED = schedule.ScheduledJob(
    id="job-1",
    prompt="check CI",
    kind="once",
    schedule="at 2030-01-02T03:04:05Z",
    status="active",
    next_run_at="2030-01-02T03:04:05.000Z",
    created_at="2029-12-31T00:00:00.000Z",
)


class ScheduleClientTest(unittest.TestCase):
    def test_add_sends_one_schedule_and_parses_the_job(self) -> None:
        host = AsyncMock(return_value={"job": _JOB})
        moment = dt.datetime(2030, 1, 2, 4, 4, 5, tzinfo=dt.timezone(dt.timedelta(hours=1)))
        with patch.object(schedule, "host_request", host):
            first = asyncio.run(schedule.add("check CI", at=moment))
            asyncio.run(schedule.add("rerun", every=dt.timedelta(hours=2), label="nightly"))
            asyncio.run(schedule.add("rerun", cron="0 3 * * *"))
            asyncio.run(schedule.add("later", at="in 30m"))
        self.assertEqual(first, _PARSED)
        self.assertEqual(
            host.await_args_list,
            [
                call("schedule.add", {"prompt": "check CI", "at": "2030-01-02T03:04:05Z"}),
                call("schedule.add", {"prompt": "rerun", "every": "7200s", "label": "nightly"}),
                call("schedule.add", {"prompt": "rerun", "cron": "0 3 * * *"}),
                call("schedule.add", {"prompt": "later", "at": "in 30m"}),
            ],
        )

    def test_list_and_cancel_send_their_requests(self) -> None:
        host = AsyncMock(side_effect=[{"jobs": [_JOB]}, {"job": {**_JOB, "status": "cancelled"}}])
        with patch.object(schedule, "host_request", host):
            jobs = asyncio.run(schedule.list(include_inactive=True))
            cancelled = asyncio.run(schedule.cancel("job-1"))
        self.assertEqual(jobs, [_PARSED])
        self.assertEqual(cancelled.status, "cancelled")
        self.assertEqual(
            host.await_args_list,
            [
                call("schedule.list", {"include_inactive": True}),
                call("schedule.cancel", {"id": "job-1"}),
            ],
        )

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(schedule, "host_request", host):
            with self.assertRaisesRegex(ValueError, "exactly one"):
                asyncio.run(schedule.add("x"))
            with self.assertRaisesRegex(ValueError, "exactly one"):
                asyncio.run(schedule.add("x", at="in 5m", every="1h"))
            with self.assertRaisesRegex(ValueError, "positive"):
                asyncio.run(schedule.add("x", at=-5))
            with self.assertRaises(TypeError):
                asyncio.run(schedule.add("  ", at="in 5m"))
            with self.assertRaises(TypeError):
                asyncio.run(schedule.cancel(""))
        host.assert_not_awaited()

    def test_a_host_refusal_raises(self) -> None:
        host = AsyncMock(side_effect=RuntimeError("scheduled jobs need the prime-agent daemon"))
        with patch.object(schedule, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "daemon"):
                asyncio.run(schedule.add("x", at="in 5m"))


if __name__ == "__main__":
    unittest.main()
