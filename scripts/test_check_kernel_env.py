#!/usr/bin/env python3
"""Prove stale tool text cannot satisfy a current kernel probe."""
import unittest
import json
from pathlib import Path
import socket
import tempfile
from unittest.mock import patch, MagicMock
from check_kernel_env import current_probe, MARKER, finish_daemon, KernelClient, wait_gone


def requests(content=None):
    messages = [{'role': 'tool', 'tool_call_id': 'kernel_old', 'content': MARKER + '{"old":true}'}]
    if content is not None:
        messages.append({'role': 'tool', 'tool_call_id': 'kernel_new', 'content': content})
    return [{}, {'messages': messages}]


class ProbeTests(unittest.TestCase):
    def test_only_current_result_counts(self):
        self.assertEqual(current_probe(requests(MARKER + '{"new":true}'), 'new'), {'new': True})

    def test_old_probe_cannot_hide_failed_cell(self):
        with self.assertRaisesRegex(AssertionError, 'current kernel probe'):
            current_probe(requests('Traceback: failed current cell'), 'new')

    def test_old_probe_cannot_hide_missing_result(self):
        with self.assertRaisesRegex(AssertionError, 'current tool result'):
            current_probe(requests(), 'new')

    def test_duplicate_probe_is_failure(self):
        with self.assertRaisesRegex(AssertionError, 'current kernel probe'):
            current_probe(requests(MARKER + '{}\n' + MARKER + '{}'), 'new')

    def test_shutdown_ack_then_control_close_then_process_exit(self):
        events = []
        client, process = MagicMock(), MagicMock(returncode=0)
        client.call.side_effect = lambda *a, **k: events.append('ack')
        client.close.side_effect = lambda: events.append('client_closed')
        process.wait.side_effect = lambda **k: events.append('process_exit')
        finish_daemon(client, process)
        self.assertEqual(events, ['ack', 'client_closed', 'process_exit'])

    def test_refused_shutdown_is_not_cleanup_success(self):
        client, process = MagicMock(), MagicMock(returncode=0)
        client.call.side_effect = AssertionError('refused')
        with self.assertRaisesRegex(AssertionError, 'refused'):
            finish_daemon(client, process)
        process.wait.assert_not_called()

    def test_only_a_session_nothing_serves_is_gone(self):
        with tempfile.TemporaryDirectory() as folder:
            served = Path(folder) / 'served.sock'
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(served))
            listener.listen(64)
            try:
                wait_gone({(str(Path(folder) / 'never-bound.sock'), 'hosted:1:a')})
                with patch('check_kernel_env.LIMIT', 0.1):
                    with self.assertRaisesRegex(AssertionError, 'still served after shutdown'):
                        wait_gone({(str(served), 'hosted:1:b')})
            finally:
                listener.close()

    def test_cold_kernel_response_after_35_seconds_uses_request_budget(self):
        client = KernelClient.__new__(KernelClient)
        client.counter, client.wire = 0, []
        client.sock = MagicMock()
        client.sock.gettimeout.return_value = 35
        client.reader = MagicMock()
        client.reader.readline.return_value = json.dumps({'type': 'response', 'id': '1', 'success': True, 'data': {'done': True}}).encode()
        with patch('check_kernel_env.time.monotonic', side_effect=[0, 36, 36]):
            self.assertEqual(client.call('prompt_and_wait', timeout_seconds=90), {'done': True})
        self.assertEqual([call.args[0] for call in client.sock.settimeout.call_args_list], [90, 54, 35])


if __name__ == '__main__':
    unittest.main()
