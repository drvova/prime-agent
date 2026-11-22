#!/usr/bin/env python3
"""Reject stale result success and crashing a session the fixture does not own."""
import json
from pathlib import Path
import unittest
import tempfile
from unittest.mock import patch, MagicMock
from check_launch_env import current_probe, restart, worker_descriptor, finish_daemon, MARKER


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

    def test_descriptor_with_wrong_identity_never_crashes_a_session(self):
        client = MagicMock()
        with patch('check_launch_env.worker_descriptor', return_value=(Path('/fixture'),
                {'pid': 0x40000001, 'rootActiveSessionId': 'someone-else'})):
            with self.assertRaisesRegex(AssertionError, 'descriptor identity'):
                restart(client, MagicMock(), 'crash', 'fixture', Path('/fixture'), Path('/fixture/binary'), {}, False, {}, set(), {})
        client.call.assert_not_called()

    def test_descriptor_filename_need_not_equal_active_id(self):
        with tempfile.TemporaryDirectory() as folder:
            agent = Path(folder)
            descriptors = agent / 'daemon-workers' / 'fixture'
            descriptors.mkdir(parents=True)
            row = {'pid': 123, 'rootActiveSessionId': 'active-id', 'supervisorSocketPath': '/fixture.sock'}
            path = descriptors / 'different-worker-id.json'
            path.write_text(json.dumps(row))
            self.assertEqual(worker_descriptor(agent, 'active-id', Path('/fixture.sock')), (path, row))

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
