#!/usr/bin/env python3
"""Lightweight guards on the check's validation and raw capture."""
import unittest
from pathlib import Path
import tempfile
from unittest.mock import Mock, patch
import check_client_write_cleanup as verifier
from lifecycle_lib import Wire2Error


class CheckTests(unittest.TestCase):
    def test_primary_failure_survives_a_shutdown_failure(self):
        provider = Mock(server_port=1)
        observer = Mock(raw_wire=[], wire=[])
        observer.call.side_effect = [{'activeSessionId': 'fixture-session'}, RuntimeError('cleanup failed')]
        attached = Mock(raw_wire=[], wire=[])
        attached.call.side_effect = [None, None, {'pauseId': 'fixture-pause'}]
        second = Mock(raw_wire=[], wire=[])
        summary = {'activeSessionId': 'fixture-session', 'sessionName': 'Write cleanup fixture', 'open': True}
        second.call.side_effect = [{'attachedClients': 1}, summary, {'attachedClients': 0}, RuntimeError('primary enqueue failure')]
        process = Mock(poll=Mock(return_value=None))
        observed = {}
        with tempfile.TemporaryDirectory() as folder:
            with patch.object(verifier, 'Provider', return_value=provider), patch.object(verifier.threading, 'Thread'), patch.object(verifier, 'WireClient', side_effect=[observer, attached, second]), patch.object(verifier.subprocess, 'Popen', return_value=process), patch.object(verifier, 'stop'), patch.object(verifier, 'settle_owned_processes'), patch.object(verifier, 'pause_released', side_effect=[False, True]):
                with self.assertRaisesRegex(RuntimeError, '^primary enqueue failure$'):
                    verifier.run_case(Path('/fixture/binary'), Path(folder), observed)
        self.assertEqual(observed['primary_error'], 'primary enqueue failure')
        self.assertEqual(observed['cleanup_error'], 'cleanup failed')
        self.assertFalse(observed['verified'])
        self.assertEqual(second.call.call_args.kwargs['streamingBehavior'], 'followUp')

    def test_rename_trigger_rejects_unexpected_data(self):
        summary = {'activeSessionId': 'session', 'sessionName': 'Write cleanup fixture', 'open': True}
        verifier.validate_rename_trigger(summary, 'session')
        for changed in (None, {**summary, 'activeSessionId': 'another'}, {**summary, 'sessionName': 'wrong'}, {**summary, 'open': False}):
            with self.assertRaises(AssertionError):
                verifier.validate_rename_trigger(changed, 'session')

    def test_nonowner_probe_does_not_hide_other_errors(self):
        reply = {'status': 'error', 'message': 'Session input pause is owned by another client: lease'}
        client = Mock()
        client.call.side_effect = Wire2Error('session.release_input', reply)
        self.assertFalse(verifier.pause_released(client, 'session', 'lease'))
        client.call.side_effect = Wire2Error('session.release_input', {**reply, 'message': 'Unexpected failure'})
        with self.assertRaises(AssertionError):
            verifier.pause_released(client, 'session', 'lease')
        client.call.side_effect = None
        self.assertTrue(verifier.pause_released(client, 'session', 'lease'))

if __name__ == '__main__':
    unittest.main()
