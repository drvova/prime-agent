import copy
import json
from pathlib import Path
import tempfile
import unittest

from check_saved_catalog import COUNT, PAGE, TARGET, normalize, seed, validate_query
from lifecycle_lib import wire2_frame


class SavedCatalogGuards(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.expected = seed(self.root)

    def call(self, rows=None, remaining=0, cursor=None):
        rows = copy.deepcopy(self.expected if rows is None else rows)
        for row in rows:
            row['path'] = str(self.root / 'agent/sessions' / row['path'])
        data = {'sessions': rows, 'remaining': remaining}
        if cursor is not None:
            data['nextCursor'] = cursor
        topic = 'runtime/session_list/list-1'
        event = lambda payload: {'type': 'event', 'subId': 'list:list-1', 'event': {
            'id': 'live', 'seq': 0, 'ts': 1, 'topic': topic, 'source': 'runtime',
            'payload': dict(payload, id='list-1', command='list_saved_sessions'), 'trust': 'owner'}}
        frames = [{'type': 'backfilled', 'subId': 'list:list-1', 'throughSeq': 0}]
        frames += [event({'session': row}) for row in rows]
        frames.append(event({'loaded': len(rows), 'total': len(rows)}))
        frames.append({'type': 'reply', 'id': 'check-2', 'status': 'ok', 'result': data})
        sent = [{'type': 'subscribe', 'subId': 'list:list-1', 'filter': topic, 'since': {'from': 'now'}},
                {'type': 'query', 'id': 'check-2', 'name': 'sessions.list', 'args': {
                    'shape': 'saved', 'stream': 'list-1', 'cwd': '/tmp/catalog-paging-fixture',
                    'scope': 'all'}},
                {'type': 'unsubscribe', 'subId': 'list:list-1'}]
        return {'paging': {}, 'frames': frames, 'final_data': data, 'after_reply': [],
            'raw_received_hex': [wire2_frame(frame).hex() for frame in frames],
            'raw_sent_hex': [wire2_frame(frame).hex() for frame in sent]}

    def refresh_raw(self, call):
        call['raw_received_hex'] = [wire2_frame(frame).hex() for frame in call['frames']]

    def check(self, call, rows=None, **kwargs):
        return validate_query(call, self.root, self.expected if rows is None else rows,
                              ordered=True, remaining=0, **kwargs)

    def test_fixture_is_valid_ordered_and_old_target_is_beyond_first_page(self):
        files = sorted((self.root / 'agent/sessions').glob('*.jsonl'))
        self.assertGreater(len(files), PAGE)
        self.assertEqual(len(files), COUNT)
        mtimes = [path.stat().st_mtime_ns for path in files]
        self.assertEqual(len(set(mtimes)), COUNT)
        self.assertEqual(mtimes, sorted(mtimes))
        for path in files:
            entries = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(entries[0]['type'], 'session')
            self.assertEqual(entries[0]['id'], path.stem)
            self.assertEqual(entries[-1]['message']['role'], 'assistant')
        self.assertNotIn(TARGET, json.dumps(self.expected[:PAGE]))
        self.assertEqual(self.expected[-1]['name'], TARGET)
        self.assertEqual(self.check(self.call()), self.expected)

    def test_rejects_missing_duplicate_or_wrong_rows(self):
        for mutate in (
            lambda rows: rows.pop(),
            lambda rows: rows.append(copy.deepcopy(rows[0])),
            lambda rows: rows[0].update(name='wrong'),
            lambda rows: rows[-1].update(firstMessage='wrong'),
        ):
            call = self.call()
            mutate(call['final_data']['sessions'])
            self.refresh_raw(call)
            with self.assertRaises(AssertionError):
                self.check(call)
        escaped = self.call()
        escaped['final_data']['sessions'][0]['path'] = '/ambient/session.jsonl'
        with self.assertRaises(AssertionError):
            self.check(escaped)

    def test_pages_cover_full_catalog_once_and_wrong_boundary_is_rejected(self):
        first = self.call(self.expected[:PAGE], COUNT - PAGE, 'boundary')
        validate_query(first, self.root, self.expected[:PAGE], ordered=True,
                       remaining=COUNT - PAGE, cursor=True)
        rest = self.call(self.expected[PAGE:])
        self.check(rest, self.expected[PAGE:])
        joined = normalize(first['final_data']['sessions'] + rest['final_data']['sessions'], self.root)
        self.assertEqual(joined, self.expected)
        for key, value in (('remaining', 0), ('nextCursor', '')):
            broken = copy.deepcopy(first)
            broken['final_data'][key] = value
            self.refresh_raw(broken)
            with self.assertRaises(AssertionError):
                validate_query(broken, self.root, self.expected[:PAGE], ordered=True,
                               remaining=COUNT - PAGE, cursor=True)
        with self.assertRaises(AssertionError):
            self.check(self.call(self.expected[PAGE - 1:]), self.expected[PAGE:])
        with self.assertRaises(AssertionError):
            self.check(self.call(self.expected[PAGE:], cursor='not-finished'), self.expected[PAGE:])

    def test_deleted_cursor_boundary_still_requires_the_complete_older_tail(self):
        boundary = self.root / 'agent/sessions' / self.expected[PAGE - 1]['path']
        boundary.unlink()
        self.assertFalse(boundary.exists())
        tail = self.expected[PAGE:]
        self.assertEqual(self.check(self.call(tail), tail), tail)
        self.assertEqual(tail[-1]['name'], TARGET)
        with self.assertRaises(AssertionError):
            self.check(self.call(tail[1:]), tail)

    def test_raw_terminal_and_stream_disagreement_fail_closed(self):
        for mode in ('missing_raw', 'bad_raw', 'wrong_request', 'wrong_stream',
                     'duplicate_stream', 'wrong_order', 'no_stream', 'after_final',
                     'foreign_after_reply'):
            call = self.call()
            if mode == 'missing_raw':
                call['raw_received_hex'] = []
            elif mode == 'bad_raw':
                call['raw_received_hex'][0] = wire2_frame({}).hex()
            elif mode == 'wrong_request':
                call['raw_sent_hex'][1] = wire2_frame({'type': 'query', 'id': 'elsewhere',
                                                       'name': 'sessions.list', 'args': {}}).hex()
            elif mode == 'wrong_stream':
                call['frames'][1] = copy.deepcopy(call['frames'][1])
                call['frames'][1]['event']['payload']['session']['firstMessage'] = 'wrong'
            elif mode == 'duplicate_stream':
                call['frames'].insert(1, copy.deepcopy(call['frames'][1]))
            elif mode == 'wrong_order':
                call['frames'][1], call['frames'][2] = call['frames'][2], call['frames'][1]
            elif mode == 'no_stream':
                call['frames'] = call['frames'][:1] + call['frames'][-1:]
            elif mode == 'foreign_after_reply':
                call['after_reply'] = [{'type': 'event', 'subId': 'elsewhere', 'event': {}}]
            else:
                call['frames'].append(copy.deepcopy(call['frames'][1]))
            if mode not in ('missing_raw', 'bad_raw', 'wrong_request', 'foreign_after_reply'):
                self.refresh_raw(call)
            with self.subTest(mode=mode), self.assertRaises(AssertionError):
                self.check(call)
        # Dropped streamed items are legal under real Rust backpressure, and the
        # stream's rows after the reply are dropped by the client.
        call = self.call()
        del call['frames'][2:PAGE]
        call['after_reply'] = [copy.deepcopy(call['frames'][1])]
        self.refresh_raw(call)
        self.check(call)


if __name__ == '__main__':
    unittest.main()
