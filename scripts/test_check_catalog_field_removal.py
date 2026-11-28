import copy
import json
from pathlib import Path
import tempfile
import unittest

from check_catalog_field_removal import COUNT, CWD, REMOVED, seed, validate
from lifecycle_lib import wire2_frame


class CatalogFieldRemovalGuards(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.expected = seed(self.root)

    def observation(self):
        rows = copy.deepcopy(self.expected)
        for row in rows:
            row['path'] = str(self.root / 'agent/sessions' / row['path'])
        topic = 'runtime/session_list/list-1'
        event = lambda payload: {'type': 'event', 'subId': 'list:list-1', 'event': {
            'id': 'live', 'seq': 0, 'ts': 1, 'topic': topic, 'source': 'runtime',
            'payload': dict(payload, id='list-1', command='list_saved_sessions'), 'trust': 'owner'}}
        frames = [{'type': 'backfilled', 'subId': 'list:list-1', 'throughSeq': 0}]
        frames += [event({'session': copy.deepcopy(row)}) for row in rows]
        frames.append(event({'loaded': COUNT, 'total': COUNT}))
        data = {'sessions': rows}
        frames.append({'type': 'reply', 'id': 'check-2', 'status': 'ok', 'result': data})
        sent = [{'type': 'subscribe', 'subId': 'list:list-1', 'filter': topic, 'since': {'from': 'now'}},
                {'type': 'query', 'id': 'check-2', 'name': 'sessions.list', 'args': {
                    'shape': 'saved', 'stream': 'list-1', 'cwd': CWD, 'scope': 'all'}},
                {'type': 'unsubscribe', 'subId': 'list:list-1'}]
        observed = {'frames': frames, 'final_data': data, 'after_reply': [],
                    'raw_sent_hex': [wire2_frame(frame).hex() for frame in sent]}
        self.raw(observed)
        return observed

    def raw(self, observed):
        observed['raw_received_hex'] = [wire2_frame(frame).hex() for frame in observed['frames']]

    def check(self, observed):
        return validate(observed, self.root, self.expected)

    def test_omission_keeps_seeded_stable_fields(self):
        self.assertEqual(self.check(self.observation()),
                         sorted(self.expected, key=lambda row: row['id']))
        for path in (self.root / 'agent/sessions').glob('*.jsonl'):
            entries = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(entries[0]['type'], 'session')
            self.assertEqual(entries[0]['id'], path.stem)
            self.assertIn('café 界', entries[-1]['message']['content'][0]['text'])

    def test_null_empty_or_text_key_fails_in_both_stream_and_final(self):
        for location in ('stream', 'final'):
            for value in (None, '', 'stale text'):
                observed = self.observation()
                row = observed['frames'][1]['event']['payload']['session'] if location == 'stream' else observed['final_data']['sessions'][0]
                row[REMOVED] = value
                self.raw(observed)
                with self.subTest(location=location, value=value), self.assertRaises(AssertionError):
                    self.check(observed)

    def test_missing_duplicate_escaped_or_changed_stable_rows_fail(self):
        for location in ('stream', 'final'):
            for mode in ('missing', 'duplicate', 'escaped', 'changed'):
                observed = self.observation()
                rows = [frame['event']['payload']['session'] for frame in observed['frames']
                        if frame['type'] == 'event' and 'session' in frame['event']['payload']] if location == 'stream' else observed['final_data']['sessions']
                if mode == 'missing':
                    if location == 'stream':
                        observed['frames'].pop(1)
                    else:
                        rows.pop()
                elif mode == 'duplicate':
                    if location == 'stream':
                        observed['frames'].insert(1, copy.deepcopy(observed['frames'][1]))
                    else:
                        rows.append(copy.deepcopy(rows[0]))
                else:
                    rows[0]['path' if mode == 'escaped' else 'name'] = '/ambient/escaped.jsonl' if mode == 'escaped' else 'wrong'
                self.raw(observed)
                with self.subTest(location=location, mode=mode), self.assertRaises(AssertionError):
                    self.check(observed)

    def test_missing_raw_wrong_request_and_postterminal_frames_fail_closed(self):
        for mode in ('missing_raw', 'wrong_raw', 'wrong_request', 'after_final', 'failure'):
            observed = self.observation()
            if mode == 'missing_raw':
                observed['raw_received_hex'] = []
            elif mode == 'wrong_raw':
                observed['raw_received_hex'][0] = wire2_frame({}).hex()
            elif mode == 'wrong_request':
                observed['raw_sent_hex'][1] = wire2_frame({'type': 'query', 'id': 'wrong',
                                                           'name': 'sessions.list', 'args': {}}).hex()
            else:
                if mode == 'after_final':
                    observed['frames'].append(copy.deepcopy(observed['frames'][1]))
                else:
                    observed['frames'][-1]['status'] = 'error'
                self.raw(observed)
            with self.subTest(mode=mode), self.assertRaises(AssertionError):
                self.check(observed)


if __name__ == '__main__':
    unittest.main()
