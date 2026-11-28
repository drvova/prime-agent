#!/usr/bin/env python3
"""Check the restart fixture's durable identity and observable resync gate."""
from pathlib import Path
import socket
import threading
import tempfile
import unittest

from check_reconnect_version import RestartDaemon
from lifecycle_lib import Wire2Client, wire2_decode


class Connection:
    def __init__(self, descriptor=4):
        self.descriptor = descriptor
        self.frames = []
        self.shutdowns = []

    def fileno(self):
        return self.descriptor

    def sendall(self, data):
        self.frames.append(wire2_decode(data.hex()))

    def shutdown(self, how):
        self.shutdowns.append(how)


def fixture():
    server = object.__new__(RestartDaemon)
    server.root = Path('/tmp/reconnect-fixture')
    server.closed = False
    server.hello = {'appVersion': '0.9.8', 'protocol': {'version': 7}}
    server.lock = threading.Lock()
    server.sockets = []
    server.connections = {}
    server.stored, server.seq = [], 0
    server.release_catalog = threading.Event()
    server.catalog_held = threading.Event()
    return server


class RestartFixtureTests(unittest.TestCase):
    def test_snapshot_reads_preserve_the_settled_fixture_turn(self):
        with tempfile.TemporaryDirectory(prefix='reconnect-read-', dir='/tmp') as folder:
            root = Path(folder)
            server = RestartDaemon(root / 'd.sock',
                {'appVersion': '0.9.8', 'protocol': {'name': 'prime-agent.daemon', 'version': 7}},
                'restart', root)
            client = None
            try:
                server.prompts.append({'message': 'persisted fixture prompt'})
                server.settle(Connection(), 's1', ('fixture', 'turn'), 1, emit=False)
                # The real link's handshake and read, over the fixture's wire2 socket.
                client = Wire2Client(root / 'd.sock', None)
                self.assertEqual(client.query('daemon.identity')['appVersion'], '0.9.8')
                self.assertEqual(client.call('get_messages', activeSessionId='s1'),
                                 {'messages': server.messages})
                self.assertEqual(len(server.messages), 2)
                self.assertEqual(server.errors, [])
            finally:
                if client is not None:
                    client.close()
                server.close()

    def test_attach_and_list_share_the_durable_session_identity(self):
        server, connection = fixture(), Connection()
        server.send(connection, {'type': 'response', 'command': 'attach',
            'data': {'snapshot': {'summary': {'id': 's1'}, 'state': {'sessionId': 'fixture-durable'}}}})
        server.send(connection, {'type': 'response', 'command': 'list', 'data': {'sessions': []}})
        attached = connection.frames[0]['result']['snapshot']['summary']
        listed = connection.frames[1]['result']['sessions'][0]
        self.assertEqual(attached, listed)
        self.assertEqual(attached['sessionId'], 'fixture-durable')
        self.assertEqual(attached['activeSessionId'], 's1')
        self.assertTrue(attached['sessionFile'].endswith('/sessions/fixture.jsonl'))

    def test_restart_announces_shutdown_before_closing_only_live_connections(self):
        server = fixture()
        live, closed = Connection(), Connection(-1)
        server.sockets = [live, closed]
        # What the TUI's link follows: the closing feed and the session's topics.
        server.connections = {live: {'compat:closing': 'system/closing', 'compat:agent:s1': 'agent/s1/#'}}
        server.restart('0.10.0')
        self.assertEqual([(frame['type'], frame.get('subId'), frame.get('event', {}).get('topic'),
                           frame.get('event', {}).get('payload')) for frame in live.frames], [
            ('event', 'compat:closing', 'system/closing', {'reason': 'shutdown'}),
            ('event', 'compat:agent:s1', 'agent/s1/closed', {'reason': 'shutdown'}),
            ('closing', None, None, None)])
        self.assertEqual(live.frames[1]['event']['seq'], 1)
        self.assertEqual(live.shutdowns, [socket.SHUT_RDWR])
        self.assertEqual(closed.frames, [])
        self.assertEqual(server.hello['appVersion'], '0.10.0')
        self.assertEqual(server.hello['protocol'], {'version': 7})
        self.assertTrue(server.hello['supervisorGeneration'])

    def test_restarted_catalog_response_waits_for_capture_release(self):
        server, connection = fixture(), Connection()
        server.closed = True
        response = {'type': 'response', 'id': 'catalog', 'command': 'get_commands', 'data': {'commands': []}}
        thread = threading.Thread(target=server.send, args=(connection, response), daemon=True)
        thread.start()
        try:
            self.assertTrue(server.catalog_held.wait(2))
            self.assertEqual(connection.frames, [])
        finally:
            server.release_catalog.set()
            thread.join(2)
        self.assertFalse(thread.is_alive())
        self.assertEqual(connection.frames, [{'type': 'reply', 'id': 'catalog', 'status': 'ok',
                                              'result': {'commands': []}}])


if __name__ == '__main__':
    unittest.main()
