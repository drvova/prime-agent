#!/usr/bin/env python3
"""Prove stale tool text cannot satisfy a current kernel probe."""
import unittest
import json
from pathlib import Path
import signal
from unittest.mock import patch, MagicMock
from check_kernel_env import current_probe, MARKER, finish_daemon, KernelClient, signal_owned, wait_gone, live_identities


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

    def test_process_exiting_during_proc_read_is_gone(self):
        with patch('check_kernel_env.process_identity', side_effect=ProcessLookupError(3, 'No such process')):
            wait_gone({(789, 'proc:456')})
            self.assertEqual(live_identities({(789, 'proc:456')}), set())

    def test_other_proc_read_errors_still_fail_cleanup(self):
        with patch('check_kernel_env.process_identity', side_effect=PermissionError('denied')):
            with self.assertRaises(PermissionError):
                wait_gone({(789, 'proc:456')})
            with self.assertRaises(PermissionError):
                live_identities({(789, 'proc:456')})

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

    def test_recycled_pid_never_receives_fallback_signal(self):
        with patch('os.pidfd_open', return_value=42, create=True), patch('os.close'), \
                patch('check_kernel_env.process_identity', return_value=('proc:new', 'S')), \
                patch('signal.pidfd_send_signal', create=True) as sent:
            self.assertFalse(signal_owned(789, 'proc:old', lambda: True, signal.SIGTERM))
            sent.assert_not_called()

    def test_failed_ownership_check_never_receives_fallback_signal(self):
        with patch('os.pidfd_open', return_value=42, create=True), patch('os.close'), \
                patch('check_kernel_env.process_identity', return_value=('proc:456', 'S')), \
                patch('signal.pidfd_send_signal', create=True) as sent:
            self.assertFalse(signal_owned(789, 'proc:456', lambda: False, signal.SIGTERM))
            sent.assert_not_called()


if __name__ == '__main__':
    unittest.main()
