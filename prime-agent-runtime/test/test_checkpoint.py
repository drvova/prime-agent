from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, call, patch

from rlm import checkpoint

_CHECKPOINT = {"id": "cp-1", "label": "before refactor", "created_at": "2030-01-01T00:00:00.000Z"}


class CheckpointClientTest(unittest.TestCase):
    def test_create_and_list_parse_checkpoints(self) -> None:
        host = AsyncMock(side_effect=[{"checkpoint": _CHECKPOINT}, {"checkpoints": [_CHECKPOINT]}])
        with patch.object(checkpoint, "host_request", host):
            taken = asyncio.run(checkpoint.create("before refactor"))
            listed = asyncio.run(checkpoint.list())
        expected = checkpoint.Checkpoint("cp-1", "before refactor", "2030-01-01T00:00:00.000Z")
        self.assertEqual((taken, listed), (expected, [expected]))
        self.assertEqual(
            host.await_args_list,
            [call("checkpoint.create", {"label": "before refactor"}), call("checkpoint.list", {})],
        )

    def test_restore_sends_a_question_id_and_parses_the_outcome(self) -> None:
        host = AsyncMock(return_value={"status": "declined", "restored": 0})
        with patch.object(checkpoint, "host_request", host), patch.object(checkpoint.os, "urandom", return_value=b"\x02" * 8):
            result = asyncio.run(checkpoint.restore("cp-1"))
        host.assert_awaited_once_with("checkpoint.restore", {"id": "cp-1", "question_id": "0202020202020202"})
        self.assertEqual(result, checkpoint.RestoreResult(status="declined", restored=0))
        self.assertFalse(result.ok)

    def test_a_disabled_restore_raises_permission_error(self) -> None:
        host = AsyncMock(return_value={"status": "disabled", "message": "checkpoint.restore is off: ..."})
        with patch.object(checkpoint, "host_request", host):
            with self.assertRaisesRegex(PermissionError, "is off"):
                asyncio.run(checkpoint.restore("cp-1"))

    def test_an_interrupted_restore_withdraws_its_question(self) -> None:
        from rlm import repl

        host = AsyncMock(side_effect=asyncio.CancelledError())
        notify = unittest.mock.Mock()
        with (
            patch.object(checkpoint, "host_request", host),
            patch.object(checkpoint.os, "urandom", return_value=b"\x03" * 8),
            patch.object(repl, "is_active", return_value=True),
            patch.object(repl, "host_notify", notify),
        ):
            with self.assertRaises(asyncio.CancelledError):
                asyncio.run(checkpoint.restore("cp-1"))
        notify.assert_called_once_with({"type": "question.cancel", "id": "0303030303030303"})

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(checkpoint, "host_request", host):
            with self.assertRaises(TypeError):
                asyncio.run(checkpoint.restore(""))
            with self.assertRaises(TypeError):
                asyncio.run(checkpoint.create(3))  # type: ignore[arg-type]
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
