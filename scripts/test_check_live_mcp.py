#!/usr/bin/env python3
"""Lightweight check tests; these never start a product binary."""
import copy
import http.client
import json
import threading
import unittest

import check_live_mcp as check


class VerifierTests(unittest.TestCase):
    def observation(self):
        return {'before': {'probe': 1, 'configs': {'fixture-before': check.BEFORE, 'fixture-after': {}}},
                'after': {'probe': 2, 'configs': {'fixture-before': {}, 'fixture-after': check.AFTER}}}

    def test_rejects_stale_and_rebuilt_handler(self):
        good = self.observation()
        check.validate_observation(good)
        stale = self.observation()
        stale['after']['configs'] = copy.deepcopy(stale['before']['configs'])
        with self.assertRaises(AssertionError):
            check.validate_observation(stale)
        rebuilt = self.observation()
        rebuilt['after']['probe'] = 1
        with self.assertRaises(AssertionError):
            check.validate_observation(rebuilt)

    def test_provider_selects_offered_tool_and_requires_actual_result(self):
        server = check.Server()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            connection = http.client.HTTPConnection(*server.server_address, timeout=3)
            first = {'tools': [{'type': 'function', 'function': {'name': 'eval'}}], 'messages': []}
            connection.request('POST', '/v1/chat/completions', json.dumps(first), {'Content-Type': 'application/json'})
            response = connection.getresponse()
            wire = response.read().decode()
            self.assertEqual(response.status, 200)
            event = json.loads(wire.splitlines()[0].removeprefix('data: '))
            function = event['choices'][0]['delta']['tool_calls'][0]['function']
            self.assertEqual(function, {'name': 'eval', 'arguments': json.dumps({'code': check.CELL})})
            with self.assertRaises(AssertionError):
                check.latest_probe(server.requests['before'])
            actual = self.observation()['before']
            second = {'messages': [{'role': 'tool', 'content': check.MARKER + json.dumps(actual)}]}
            connection.request('POST', '/v1/chat/completions', json.dumps(second), {'Content-Type': 'application/json'})
            connection.getresponse().read()
            self.assertEqual(check.latest_probe(server.requests['before']), actual)
            connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_after_probe_selects_latest_result_in_history(self):
        before, after = self.observation()['before'], self.observation()['after']
        old = {'role': 'tool', 'content': check.MARKER + json.dumps(before)}
        new = {'role': 'tool', 'content': [{'type': 'text', 'text': check.MARKER + json.dumps(after)}]}
        requests = [{'messages': [old]}, {'messages': [old, new]}]
        self.assertEqual(check.latest_probe(requests), after)


if __name__ == '__main__':
    unittest.main()
