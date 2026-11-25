"""Mocked ownership/cleanup guards; no product binaries or kernels are started."""
from pathlib import Path
import signal
import unittest
from unittest.mock import Mock, patch

import lifecycle_lib as lifecycle


class CleanupTests(unittest.TestCase):
    def test_ack_then_close_then_exit_then_owned_absence(self):
        order = []
        scope, client, daemon = Mock(), Mock(), Mock(returncode=0)
        client.call.side_effect = lambda *_args, **_kwargs: order.append('ack')
        client.close.side_effect = lambda: order.append('close')
        daemon.wait.side_effect = lambda **_kwargs: order.append('exit')
        scope.wait_gone.side_effect = lambda *_args: order.append('absent')
        scope.targets = {}
        observation = {}
        lifecycle.finish(scope, client, daemon, observation)
        self.assertEqual(order[:4], ['ack', 'close', 'exit', 'absent'])
        self.assertTrue(observation['cleanup_verified'])
        scope.send.assert_not_called()

    def test_successful_fallback_keeps_shutdown_failure_red(self):
        scope, client, daemon = Mock(), Mock(), Mock()
        client.call.side_effect = TimeoutError('shutdown ack missing')
        scope.live.side_effect = [[12], []]
        scope.send.return_value = True
        scope.targets, scope.errors = {12: 'proc:4'}, []
        observation = {}
        lifecycle.finish(scope, client, daemon, observation)
        self.assertFalse(observation['cleanup_verified'])
        self.assertEqual(observation['cleanup_error'], 'shutdown ack missing')
        self.assertEqual(observation['fallback_signals'], [{'pid': 12, 'signal': 'SIGTERM'}])
        self.assertEqual(observation['fallback_remaining'], [])

    def test_reused_pid_is_never_signaled(self):
        scope = lifecycle.OwnedProcesses(Path('/fixture/bin'), Path('/fixture'), Path('/fixture/d.sock'))
        scope.targets[12] = 'proc:4'
        with patch.object(lifecycle.os, 'pidfd_open', return_value=9, create=True), \
                patch.object(lifecycle.os, 'close') as close, \
                patch.object(lifecycle.signal, 'pidfd_send_signal', create=True) as send, \
                patch.object(scope, 'ownership', return_value=False):
            self.assertFalse(scope.send(12, signal.SIGKILL))
            send.assert_not_called()
            close.assert_called_once_with(9)

    def test_pidfd_pins_signal_after_validation(self):
        scope = lifecycle.OwnedProcesses(Path('/fixture/bin'), Path('/fixture'), Path('/fixture/d.sock'))
        scope.targets[12] = 'proc:4'
        with patch.object(lifecycle.os, 'pidfd_open', return_value=9, create=True), \
                patch.object(lifecycle.os, 'close'), \
                patch.object(lifecycle.signal, 'pidfd_send_signal', create=True) as send, \
                patch.object(scope, 'ownership', return_value=True) as ownership:
            self.assertTrue(scope.send(12, signal.SIGTERM))
            ownership.assert_called_once_with(12, 'proc:4')
            send.assert_called_once_with(9, signal.SIGTERM)

    def test_foreign_fixture_tag_never_owned(self):
        scope = lifecycle.OwnedProcesses(Path('/fixture/bin'), Path('/fixture'), Path('/fixture/d.sock'))
        scope.direct[12] = ('proc:4', [b'/fixture/bin'])
        with patch.object(lifecycle, 'process_identity', return_value=('proc:4', 'S')), \
                patch.object(Path, 'read_bytes', return_value=b'PA_GOAL_FIXTURE_ID=/foreign\0'):
            self.assertFalse(scope.ownership(12, 'proc:4'))

    def test_same_tag_different_direct_argv_never_owned(self):
        scope = lifecycle.OwnedProcesses(Path('/fixture/bin'), Path('/fixture'), Path('/fixture/d.sock'))
        scope.direct[12] = ('proc:4', [b'/fixture/bin', b'--mode', b'daemon'])
        with patch.object(lifecycle, 'process_identity', return_value=('proc:4', 'S')), \
                patch.object(Path, 'read_bytes', side_effect=[b'PA_GOAL_FIXTURE_ID=/fixture\0', b'/fixture/bin\0different\0']):
            self.assertFalse(scope.ownership(12, 'proc:4'))

    def test_a_session_of_another_daemon_or_a_worker_process_is_not_the_fixtures(self):
        row = {'rootActiveSessionId': 'fixture', 'supervisorSocketPath': '/fixture/d.sock',
               'socketPath': '/fixture/session.sock', 'pid': lifecycle.HOSTED_PID_BASE + 7,
               'processStartId': 'hosted:41:lifetime'}
        argv = b'/fixture/bin\0--mode\0daemon\0'
        check = lambda row, daemon: lifecycle.verify_session(row, 'fixture', Path('/fixture/bin'), Path('/fixture/d.sock'), daemon)
        with patch.object(Path, 'read_bytes', return_value=argv), \
                patch.object(lifecycle, 'session_serving', return_value=True):
            self.assertEqual(check(row, 41), ('/fixture/session.sock', 'hosted:41:lifetime'))
            with self.assertRaisesRegex(AssertionError, 'hosted by another daemon'):
                check(row, 42)
            with self.assertRaisesRegex(AssertionError, 'not hosted in the daemon'):
                check({**row, 'pid': 4242}, 41)

    def test_kernel_requires_verified_owner(self):
        scope = lifecycle.OwnedProcesses(Path('/fixture/bin'), Path('/fixture'), Path('/fixture/d.sock'))
        scope.targets = {12: 'proc:4'}
        for owner, expected in ((12, True), (99, False)):
            with patch.object(lifecycle, 'process_identity', return_value=('proc:8', 'S')), \
                    patch.object(Path, 'read_bytes', side_effect=[
                        ('PA_GOAL_FIXTURE_ID=/fixture\0PRIME_AGENT_KERNEL_OWNER_PID=%s\0' % owner).encode(), b'/usr/bin/python3\0kernel\0']):
                self.assertEqual(scope.ownership(20, 'proc:8'), expected)


class TranscriptTests(unittest.TestCase):
    def test_exported_transcripts_map_back_to_their_locators(self):
        """export-jsonl's layout (the agent dir's, `other/<absolute path>` outside it)
        maps back to locators; only session-headed transcripts under a dir count."""
        import tempfile
        with tempfile.TemporaryDirectory() as temp:
            agent = Path(temp).resolve() / 'agent'
            agent.mkdir()
            (agent / 'agent.db').write_text('')
            header = '{"type":"session","id":"s"}\n'

            def export(command, **_kwargs):
                out = Path(command[command.index('--out') + 1])
                (out / 'sessions').mkdir(parents=True)
                (out / 'sessions/a.jsonl').write_text(header)
                (out / 'sessions/journal.jsonl').write_text('{"op":1}\n')
                outside = out / 'other' / Path(temp).resolve().relative_to('/') / 'custom'
                outside.mkdir(parents=True)
                (outside / 'b.jsonl').write_text(header)

            with patch('lifecycle_lib.subprocess.run', side_effect=export):
                found = lifecycle.saved_transcripts(agent, binary='prime-agent')
                sessions = lifecycle.session_transcripts(agent, agent / 'sessions', 'prime-agent')
                custom = lifecycle.transcript_text(agent, Path(temp) / 'custom/b.jsonl', 'prime-agent')
            self.assertEqual(
                (sorted(found), [path for path, _ in sessions], custom),
                (sorted([agent / 'sessions/a.jsonl', agent / 'sessions/journal.jsonl',
                         Path(temp).resolve() / 'custom/b.jsonl']),
                 [agent / 'sessions/a.jsonl'], header))


if __name__ == '__main__':
    unittest.main()
