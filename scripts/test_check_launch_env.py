#!/usr/bin/env python3
"""Reject stale result success and crashing a session the fixture does not own."""
from pathlib import Path
import unittest
import tempfile
from unittest.mock import patch, MagicMock
from check_launch_env import current_probe, durable_files_with, restart, finish_daemon, MARKER


def requests(content=None):
    messages = [{'role': 'tool', 'tool_call_id': 'launch_old', 'content': MARKER + '{"old":true}'}]
    if content is not None:
        messages.append({'role': 'tool', 'tool_call_id': 'launch_new', 'content': content})
    return [{}, {'messages': messages}]


class VerifierTests(unittest.TestCase):
    def test_only_current_result_counts(self):
        self.assertEqual(current_probe(requests(MARKER + '{"new":true}'), 'new'), {'new': True})

    def test_old_result_cannot_hide_failed_cell(self):
        with self.assertRaisesRegex(AssertionError, 'current probe'):
            current_probe(requests('Traceback: failed current cell'), 'new')

    def test_old_result_cannot_hide_missing_result(self):
        with self.assertRaisesRegex(AssertionError, 'current tool result'):
            current_probe(requests(), 'new')

    def test_duplicate_current_probe_is_failure(self):
        content = MARKER + '{}\n' + MARKER + '{}'
        with self.assertRaisesRegex(AssertionError, 'current probe'):
            current_probe(requests(content), 'new')

    def test_a_session_the_daemon_does_not_hold_is_never_crashed(self):
        client = MagicMock()
        with patch('check_launch_env.verify_runtime_session',
                   side_effect=AssertionError('session is not open in the fixture daemon')):
            with self.assertRaisesRegex(AssertionError, 'not open in the fixture daemon'):
                restart(client, MagicMock(), 'crash', 'fixture', Path('/fixture/agent'),
                        Path('/fixture/binary'), MagicMock(), {})
        client.call.assert_not_called()

    def test_the_secret_is_found_in_any_durable_file(self):
        with tempfile.TemporaryDirectory() as folder:
            agent = Path(folder)
            (agent / 'nested').mkdir()
            (agent / 'agent.db-wal').write_bytes(b'\0\0fixture-launch-secret\0')
            (agent / 'nested' / 'clean.json').write_text('{}')
            self.assertEqual(durable_files_with(agent, 'fixture-launch-secret'),
                             [str(agent / 'agent.db-wal')])

    def test_shutdown_ack_then_close_then_process_exit(self):
        events = []
        client, process = MagicMock(), MagicMock(returncode=0)
        client.call.side_effect = lambda *a, **k: events.append('ack')
        client.close.side_effect = lambda: events.append('client_closed')
        process.wait.side_effect = lambda **k: events.append('process_exit')
        finish_daemon(client, process, 'shutdown', force=True)
        self.assertEqual(events, ['ack', 'client_closed', 'process_exit'])

    def test_refused_shutdown_cannot_be_cleanup_success(self):
        client, process = MagicMock(), MagicMock(returncode=0)
        client.call.side_effect = AssertionError('refused')
        with self.assertRaisesRegex(AssertionError, 'refused'):
            finish_daemon(client, process, 'shutdown', force=True)
        process.wait.assert_not_called()


if __name__ == '__main__':
    unittest.main()
