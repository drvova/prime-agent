"""Mocked ownership/cleanup guards; no product binaries or kernels are started."""
import json
from pathlib import Path
import signal
import socket
import struct
import tempfile
import threading
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


class FakeRuntime:
    """A wire2 server on a Unix socket: welcomes one client, records its
    frames and answers each with `answer(frame)`, a list of frames to send."""
    def __init__(self, folder, answer):
        self.path = Path(folder) / 'daemon2.sock'
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.path))
        self.listener.listen(1)
        self.answer, self.received = answer, []
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        sock, _ = self.listener.accept()
        with sock:
            buffer = b''
            while True:
                data = sock.recv(65536)
                if not data:
                    return
                buffer += data
                while len(buffer) >= 8:
                    header, payload = struct.unpack('>II', buffer[:8])
                    if len(buffer) < 8 + header + payload:
                        break
                    frame = json.loads(buffer[8:8 + header])
                    buffer = buffer[8 + header + payload:]
                    self.received.append(frame)
                    replies = ([{'type': 'welcome', 'version': 1, 'runtimeVersion': 'fixture', 'lastSeq': 0}]
                               if frame['type'] == 'hello' else self.answer(frame))
                    for reply in replies:
                        # Split inside the prefix: the client reads whole frames only.
                        data = lifecycle.wire2_frame(reply)
                        sock.sendall(data[:5])
                        sock.sendall(data[5:])

    def close(self):
        self.listener.close()
        self.thread.join(timeout=5)


def ok(frame, result):
    return {'type': 'reply', 'id': frame['id'], 'status': 'ok', 'result': result}


class Wire2ClientTests(unittest.TestCase):
    def run_client(self, answer, exercise):
        with tempfile.TemporaryDirectory(dir='/tmp') as folder:
            runtime = FakeRuntime(folder, answer)
            client = lifecycle.Wire2Client(Path(folder) / 'd.sock', None, timeout=5)
            try:
                return exercise(client), [frame for frame in runtime.received if frame['type'] != 'hello']
            finally:
                client.close()
                runtime.close()

    def test_a_named_create_is_the_create_then_its_rename(self):
        def answer(frame):
            if frame.get('method') == 'session.create':
                return [ok(frame, {'session': 's1', 'path': '/s/s1.jsonl', 'cwd': '/w'})]
            return [ok(frame, None)]
        created, sent = self.run_client(answer, lambda client: client.call(
            'create', name='target', config={'cwd': '/w', 'provider': 'p', 'model': 'm',
                                             'noTools': True, 'telemetryDisabled': True}))
        self.assertEqual(
            (created, [(frame['type'], frame.get('method'), frame['args']) for frame in sent]),
            ({'session': 's1', 'path': '/s/s1.jsonl', 'cwd': '/w', 'activeSessionId': 's1',
              'sessionId': 's1', 'sessionFile': '/s/s1.jsonl'},
             [('call', 'session.create', {'options': {'provider': 'p', 'model': 'm', 'telemetryDisabled': True},
                                          'cwd': '/w'}),
              ('call', 'session.rename', {'session': 's1', 'name': 'target'})]))

    def test_prompt_and_wait_answers_its_own_tasks_done(self):
        def answer(frame):
            if frame['type'] == 'subscribe':
                return [{'type': 'backfilled', 'subId': frame['subId'], 'throughSeq': 0}]
            done = lambda task, outcome: {'type': 'event', 'subId': 'agent:s1', 'event': {
                'id': 'e', 'seq': 1, 'ts': 1, 'topic': 'agent/s1/done', 'source': 'runtime',
                'payload': {'task': task, 'outcome': outcome}, 'trust': 'owner'}}
            # Another task's `done` comes first; the reply names the task after its `done`.
            return [done('t0', 'failed'), done('t1', 'completed'), ok(frame, {'task': 't1'})]
        done, sent = self.run_client(answer, lambda client: client.call(
            'prompt_and_wait', timeout_seconds=90, activeSessionId='s1', message='hi'))
        self.assertEqual(
            (done, [(frame['type'], frame.get('filter') or frame.get('topic'), frame.get('payload'))
                    for frame in sent]),
            ({'task': 't1', 'outcome': 'completed'},
             [('subscribe', 'agent/s1/#', None),
              ('publish', 'agent/s1/input', {'text': 'hi', 'priority': 'follow_up'})]))

    def test_a_stream_no_longer_followed_is_dropped(self):
        def answer(frame):
            if frame['type'] == 'subscribe':
                return [{'type': 'backfilled', 'subId': frame['subId'], 'throughSeq': 0}]
            row = lambda: {'type': 'event', 'subId': 'list:list-1', 'event': {
                'id': 'live', 'seq': 0, 'ts': 1, 'topic': 'runtime/session_list/list-1',
                'source': 'runtime', 'payload': {'id': 'list-1', 'loaded': 1, 'total': 1},
                'trust': 'owner'}}
            if frame['type'] == 'query':
                return [row(), ok(frame, {'sessions': []})]
            # A row the daemon sent before it read the unsubscribe.
            return [row()]
        def exercise(client):
            listed = client.call('list_saved_sessions', cwd='/w')
            client.wait('the late row', lambda: client.dropped)
            return listed, [event['payload'] for event in client.events], client.dropped
        (listed, events, dropped), sent = self.run_client(answer, exercise)
        self.assertEqual(
            (listed, len(events), [frame['subId'] for frame in dropped], [frame['type'] for frame in sent]),
            ({'sessions': []}, 1, ['list:list-1'], ['subscribe', 'query', 'unsubscribe']))

    def test_a_refusal_raises_with_its_reply_and_an_unmapped_command_names_itself(self):
        refusal = lambda frame: [{'type': 'reply', 'id': frame['id'], 'status': 'error',
                                  'message': 'no such session', 'info': {'kind': 'x'}}]
        def exercise(client):
            with self.assertRaises(lifecycle.Wire2Error) as refused:
                client.call('kill', activeSessionId='gone')
            with self.assertRaisesRegex(AssertionError, 'no wire2 form of `refine`'):
                client.call('refine', activeSessionId='s1')
            return refused.exception.reply
        reply, sent = self.run_client(refusal, exercise)
        self.assertEqual((reply['message'], reply['info'], [frame.get('method') for frame in sent]),
                         ('no such session', {'kind': 'x'}, ['session.stop']))

    def test_a_subagent_delete_rides_the_stop(self):
        _, sent = self.run_client(lambda frame: [ok(frame, None)], lambda client: client.call(
            'kill', activeSessionId='c1', rlmLedgerDelete='user', rlmChildId='h1'))
        self.assertEqual([(frame['method'], frame['args']) for frame in sent],
                         [('session.stop', {'session': 'c1', 'rlmLedgerDelete': 'user', 'rlmChildId': 'h1'})])

    def test_a_call_waits_for_its_own_request_budget(self):
        client = lifecycle.Wire2Client.__new__(lifecycle.Wire2Client)
        client.lock, client.counter, client.replies, client.sent, client.raw_sent = threading.Condition(), 0, {}, [], []
        client.sock = Mock()
        with patch.object(client, 'wait', return_value={'status': 'ok', 'result': {'done': True}}) as wait:
            self.assertEqual(client.call('get_commands', timeout_seconds=90, activeSessionId='s1'), {'done': True})
        self.assertEqual(wait.call_args.args[2], 90)


if __name__ == '__main__':
    unittest.main()
