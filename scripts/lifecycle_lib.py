"""Shared real-binary lifecycle check infrastructure: owned daemons, PTY terminals,
protocol fixture supervisors, terminal screen capture and owned-process cleanup."""
import codecs
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import shlex
import select
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import fcntl
import termios
import unicodedata

LIMIT = 35


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def stop(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)


def env_for(root):
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TZ")
           if key in os.environ}
    for name in ("home", "tmp", "xdg", "agent/sessions"):
        (root / name).mkdir(parents=True, exist_ok=True)
    env.update(HOME=str(root / "home"), TMPDIR=str(root / "tmp"),
               XDG_CONFIG_HOME=str(root / "xdg"),
               PRIME_AGENT_CODING_AGENT_DIR=str(root / "agent"),
               PI_CODING_AGENT_DIR=str(root / "agent"), TERM="xterm-256color",
               PRIME_AGENT_MODEL_PROVIDER="parity-local", PRIME_AGENT_MODEL="parity-1",
               PI_OFFLINE="1", DO_NOT_TRACK="1",
               # These checks speak the old supervisor's wire on its socket
               # (`daemon.sock`); the runtime daemon (wire2, `daemon2.sock`) is
               # the default, so the supervisor is selected by name.
               PRIME_AGENT_RUNTIME_DAEMON="0")
    (root / "agent/settings.json").write_text(json.dumps({
        "onboardingCompleted": True, "onboardingShown": True,
        "telemetry": {"noticeShown": True}}))
    (root / "agent/models.json").write_text(json.dumps({"providers": {
        "parity-local": {"api": "openai-completions", "baseUrl": "http://127.0.0.1:1/v1",
                         "apiKey": "fixture-only", "models": [{"id": "parity-1",
                         "name": "Parity 1", "contextWindow": 128000, "maxTokens": 4096}]}}}))
    return env


def actual_hello(binary, root, env):
    path = root / "hello.sock"
    with open(root / "hello-daemon.log", "wb") as log:
        process = subprocess.Popen([str(binary), "--mode", "daemon", "--daemon-socket", str(path)],
            cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = time.monotonic() + LIMIT
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError("hello-discovery daemon exited before greeting")
                try:
                    with socket.socket(socket.AF_UNIX) as sock:
                        sock.settimeout(2)
                        sock.connect(str(path))
                        with sock.makefile("rb") as stream:
                            hello = json.loads(stream.readline())
                        if hello.get("type") != "daemon_hello" or not hello.get("schemaId"):
                            raise RuntimeError(f"invalid actual hello: {hello}")
                        return hello
                except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
                    # Poll actual socket readiness, bounded by the discovery deadline.
                    threading.Event().wait(0.05)
            raise TimeoutError("hello-discovery socket readiness")
        finally:
            stop(process)



WIRE2_VERSION = 1
# The old commands the runtime daemon answers with one call or query on the
# command's session, its other members as the arguments (`pa-tui`
# `wire2_compat.rs`, the authority on the mapping, `session_call`/`command_query`).
WIRE2_SESSION_CALLS = {
    'cycle_model': 'session.cycle_model', 'set_scoped_models': 'session.set_scoped_models',
    'cycle_thinking_level': 'session.cycle_effort', 'set_service_tier': 'session.set_service_tier',
    'set_transport': 'session.set_transport', 'set_steering_mode': 'session.set_steering_mode',
    'set_follow_up_mode': 'session.set_follow_up_mode',
    'set_auto_compaction': 'session.set_auto_compaction', 'set_auto_retry': 'session.set_auto_retry',
    'reload': 'session.reload', 'mark_anthropic_warning_shown': 'session.mark_anthropic_warning_shown',
    'navigate_tree': 'session.navigate_tree', 'abort_branch_summary': 'session.abort_branch_summary',
    'export_html': 'session.export_html', 'fork': 'session.fork_entry',
    'import_jsonl': 'session.import_jsonl', 'mutate_queued_message': 'session.mutate_queued_message',
    'abort_compaction': 'session.abort_compaction', 'abort_retry': 'session.abort_retry',
    'answer_question': 'session.answer_question', 'kill_kernel_bash': 'session.kill_kernel_bash',
    'factory_activity': 'session.factory_activity', 'agent_messages_clear': 'agent_messages.clear',
    'set_rlm_max_depth': 'session.set_rlm_max_depth', 'heartbeat_set': 'session.set_heartbeat',
    'heartbeat_update': 'session.update_heartbeat', 'heartbeat_manage': 'session.manage_heartbeat',
}
WIRE2_SESSION_QUERIES = {
    'get_commands': 'session.commands', 'get_mcp_connections': 'session.mcp_connections',
    'get_model_catalog': 'session.model_catalog', 'get_user_messages_for_forking': 'session.fork_messages',
    'get_last_assistant_text': 'session.last_assistant_text', 'list_kernel_bash': 'session.kernel_bash',
    'tail_kernel_bash': 'session.kernel_bash_tail', 'get_rlm_max_depth_status': 'session.rlm_max_depth',
    'heartbeat_get': 'session.heartbeat', 'get_connection_state': 'session.model',
    'get_rlm_children': 'session.children', 'get_session_stats': 'session.usage',
    'get_context_tree': 'session.context_tree', 'get_context_breakdown': 'session.breakdown',
    'get_session_tree': 'session.tree', 'get_system_prompt': 'session.system_prompt',
}
# The create config's members `session.create` takes as `options` (`CREATE_OPTIONS`).
WIRE2_CREATE_OPTIONS = ('provider', 'model', 'thinking', 'systemPrompt', 'appendSystemPrompt',
                        'skills', 'promptTemplates', 'autonomous', 'executionMode', 'apiKey',
                        'credential', 'profile', 'models')


def wire2_socket(path):
    """The runtime daemon's socket for a launch's `--daemon-socket path`: wire2
    is served on `daemon2.sock` beside it."""
    path = Path(path)
    return path if path.name == 'daemon2.sock' else path.with_name('daemon2.sock')


def wire2_frame(header, payload=b''):
    """One private frame (`pa_types::daemon::framing`): the u32 BE header and
    payload lengths, the JSON header, the payload."""
    data = json.dumps(header).encode()
    return struct.pack('>II', len(data), len(payload)) + data + payload



def wire2_decode(line):
    """The JSON header of one private frame, given as hex."""
    data = bytes.fromhex(line)
    header, _payload = struct.unpack('>II', data[:8])
    return json.loads(data[8:8 + header])


def saved_list_capture(client, call, **args):
    """`list_saved_sessions` on `client` with `args`, and the traffic it took,
    kept in `call` (also when it fails):
    `final_data`, the frames received up to its reply (`frames`, and as hex
    `raw_received_hex`), those after it (`after_reply`) and the frames sent
    (`raw_sent_hex`). The reply is the catalog: the client stops following
    the stream at it, and a row sent after it is dropped (the old daemon's
    full queue skipped it)."""
    start, raw_start, sent_start = len(client.wire), len(client.raw_received), len(client.raw_sent)
    try:
        call['final_data'] = client.call('list_saved_sessions', **args)
        return call
    finally:
        frames, raw = client.wire[start:], client.raw_received[raw_start:]
        end = next((index + 1 for index, frame in enumerate(frames)
                    if frame.get('type') == 'reply'), len(frames))
        call['frames'], call['raw_received_hex'] = frames[:end], raw[:end]
        call['after_reply'] = frames[end:]
        call['raw_sent_hex'] = client.raw_sent[sent_start:]


def saved_list_stream(call, args):
    """The rows and progress a captured `list_saved_sessions` (see
    `saved_list_capture`) streamed, once its traffic is proven to be that one
    query's: subscribe to its own live-only stream, the query with `args`,
    the replay's end, only its stream's events, the reply that ends it and
    is `final_data`, the unsubscribe; raw bytes and decoded frames agree."""
    assert call['raw_received_hex'] and call['raw_sent_hex'], 'missing raw traffic'
    subscribe, request, unsubscribe = [wire2_decode(line) for line in call['raw_sent_hex']]
    stream = request['args'].get('stream')
    assert request == {'type': 'query', 'id': request['id'], 'name': 'sessions.list',
                       'args': {'shape': 'saved', 'stream': stream, **args}}, request
    sub_id = subscribe['subId']
    assert (subscribe, unsubscribe) == (
        {'type': 'subscribe', 'subId': sub_id, 'filter': 'runtime/session_list/' + stream,
         'since': {'from': 'now'}},
        {'type': 'unsubscribe', 'subId': sub_id}), (subscribe, unsubscribe)
    frames = call['frames']
    finals = [frame for frame in frames if frame.get('type') == 'reply']
    assert len(finals) == 1, 'missing/duplicate terminal reply'
    final = finals[0]
    assert final.get('status') == 'ok' and final.get('id') == request['id'], 'request/reply ID mismatch'
    assert final.get('result') == call['final_data'] and frames[-1] == final, 'final is not the stream terminator'
    assert [wire2_decode(line) for line in call['raw_received_hex']] == frames, 'raw/decoded receipt mismatch'
    assert all(bytes.fromhex(line) == wire2_frame(wire2_decode(line))
               for line in call['raw_sent_hex']), 'raw/decoded request mismatch'
    assert frames[0] == {'type': 'backfilled', 'subId': sub_id,
                         'throughSeq': frames[0].get('throughSeq')}, frames[0]
    for frame in call.get('after_reply', []):
        assert frame.get('type') == 'event' and frame.get('subId') == sub_id, frame
    items, progress = [], []
    for frame in frames[1:-1]:
        assert frame.get('type') == 'event' and frame.get('subId') == sub_id, frame
        event = frame['event']
        assert event['topic'] == 'runtime/session_list/' + stream and event['seq'] == 0, event
        row = event['payload']
        assert row.get('id') == stream and row.get('command') == 'list_saved_sessions', row
        if 'session' in row:
            items.append(row['session'])
        else:
            assert 0 <= row['loaded'] <= row['total'], row
            progress.append(row)
    return items, progress


class Wire2Error(AssertionError):
    """The runtime daemon refused a request: its reply's `message` and `info`."""
    def __init__(self, request, reply):
        super().__init__(f"{request}: {reply.get('message')}")
        self.reply = reply


class TaskFailed(AssertionError):
    """A `prompt_and_wait` whose task's `done` says it failed: `done` is its payload."""
    def __init__(self, done):
        super().__init__(f"the prompt failed: {done.get('error')}")
        self.done = done


class Wire2Client:
    """A lifecycle check's client of the runtime daemon (wire2 on `daemon2.sock`).

    `call(kind, **members)` keeps the old wire's command API the checks speak:
    each old command becomes the wire2 frames that do the same (`pa-tui`
    `wire2_compat.rs`, `pa-runtime` `acp/wire2_link.rs`, `pa-cli`
    `daemon_command.rs`) and answers the old response's `data`; a refusal
    raises `Wire2Error`. `wire` keeps every frame the daemon sent, `events`
    every event of the client's subscriptions, in order. A command with no
    wire2 form raises, naming it."""
    def __init__(self, path, process, timeout=LIMIT, name='lifecycle-check'):
        self.path = wire2_socket(path)
        deadline = time.monotonic() + timeout
        while True:
            self.sock = socket.socket(socket.AF_UNIX)
            try:
                self.sock.connect(str(self.path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                self.sock.close()
                assert process is None or process.poll() is None, 'daemon exited before socket readiness'
                assert time.monotonic() < deadline, 'daemon socket never became ready'
                threading.Event().wait(0.025)
        self.lock = threading.Condition()
        self.wire, self.events, self.sent = [], [], []
        # Every frame as its bytes (hex), as sent and as received.
        self.raw_sent, self.raw_received = [], []
        self.replies, self.backfilled = {}, set()
        self.subscriptions, self.dropped = set(), []
        self.counter = 0
        self.closed = None
        self.send({'type': 'hello', 'version': WIRE2_VERSION, 'client': name})
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.hello = self.wait('welcome', lambda: next(
            (frame for frame in self.wire if frame.get('type') in ('welcome', 'closing')), None), timeout)
        assert self.hello['type'] == 'welcome', self.hello

    def send(self, frame):
        data = wire2_frame(frame)
        self.sent.append(frame)
        self.raw_sent.append(data.hex())
        self.sock.sendall(data)

    def read(self):
        buffer = b''
        try:
            while True:
                while len(buffer) >= 8:
                    header, payload = struct.unpack('>II', buffer[:8])
                    if len(buffer) < 8 + header + payload:
                        break
                    frame = json.loads(buffer[8:8 + header])
                    raw, buffer = buffer[:8 + header + payload], buffer[8 + header + payload:]
                    with self.lock:
                        kind = frame.get('type')
                        if kind == 'event' and frame.get('subId') not in self.subscriptions:
                            # Sent before the daemon read the unsubscribe: no
                            # longer followed, so dropped (kept apart).
                            self.dropped.append(frame)
                            continue
                        self.raw_received.append(raw.hex())
                        self.wire.append(frame)
                        if kind == 'reply':
                            self.replies[frame['id']] = frame
                        elif kind == 'backfilled':
                            self.backfilled.add(frame['subId'])
                        elif kind == 'event':
                            self.events.append(frame['event'])
                        self.lock.notify_all()
                data = self.sock.recv(65536)
                if not data:
                    break
                buffer += data
        except OSError as error:
            reason = str(error)
        else:
            reason = 'the daemon closed the connection'
        with self.lock:
            self.closed = reason
            self.lock.notify_all()

    def wait(self, label, found, timeout=LIMIT):
        """`found()`'s first truthy value, checked under the client's lock as
        frames arrive; fails once the connection ends or `timeout` passes."""
        deadline = time.monotonic() + timeout
        with self.lock:
            while True:
                value = found()
                if value:
                    return value
                if self.closed is not None:
                    raise AssertionError(f'{label}: {self.closed}')
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimeoutError('never answered: ' + label)
                self.lock.wait(left)

    def request(self, frame, timeout=LIMIT):
        with self.lock:
            self.counter += 1
            frame = dict(frame, id=f'check-{self.counter}')
        self.send(frame)
        label = frame.get('method') or frame.get('name') or frame.get('topic')
        reply = self.wait(label, lambda: self.replies.pop(frame['id'], None), timeout)
        if reply['status'] != 'ok':
            raise Wire2Error(label, reply)
        return reply['result']

    def query(self, name, timeout=LIMIT, **args):
        return self.request({'type': 'query', 'name': name, 'args': args}, timeout)

    def invoke(self, method, timeout=LIMIT, **args):
        return self.request({'type': 'call', 'method': method, 'args': args}, timeout)

    def publish(self, topic, payload, timeout=LIMIT):
        return self.request({'type': 'publish', 'topic': topic, 'payload': payload}, timeout)

    def subscribe(self, sub_id, filter, since=None, timeout=LIMIT):
        """Follows `filter` (live from now by default) once its replay is done."""
        if sub_id in self.subscriptions:
            return
        self.subscriptions.add(sub_id)
        self.send({'type': 'subscribe', 'subId': sub_id, 'filter': filter,
                   'since': since or {'from': 'now'}})
        self.wait('subscription ' + sub_id, lambda: sub_id in self.backfilled, timeout)

    def unsubscribe(self, sub_id):
        with self.lock:
            self.subscriptions.discard(sub_id)
        self.send({'type': 'unsubscribe', 'subId': sub_id})

    def follow(self, session, timeout=LIMIT):
        """Follows every topic of `session` (`agent/<id>/#`), as an attach does."""
        self.subscribe('agent:' + session, f'agent/{session}/#', timeout=timeout)

    def event(self, topic, predicate=lambda payload: True, timeout=LIMIT):
        """The first event on `topic` (of a followed subscription) whose payload matches."""
        return self.wait('event on ' + topic, lambda: next(
            (event for event in self.events if event['topic'] == topic and predicate(event['payload'])),
            None), timeout)

    def input(self, session, payload, timeout=LIMIT):
        return self.publish(f'agent/{session}/input', payload, timeout)

    def prompt_and_wait(self, session, payload, timeout):
        """Publishes the input and answers its task's `done` (a failed task fails)."""
        self.follow(session)
        accepted = self.input(session, payload)
        task = accepted.get('task')
        assert task, 'the input was stored but no task was queued: ' + json.dumps(accepted)
        done = self.event(f'agent/{session}/done', lambda payload: payload.get('task') == task, timeout)
        if done['payload'].get('outcome') == 'failed':
            raise TaskFailed(done['payload'])
        return done['payload']

    def call(self, kind, timeout_seconds=LIMIT, **payload):
        """Old command `kind` with its members: its old response's `data`."""
        session = payload.pop('activeSessionId', None)
        timeout = timeout_seconds
        if kind in ('prompt', 'steer', 'follow_up', 'prompt_and_wait'):
            assert not {'content', 'customMessage', 'prefixMessages'} & payload.keys(), \
                'the runtime input carries a prompt\'s text and images only'
            priority = {'steer': 'steer', 'follow_up': 'follow_up', 'prompt_and_wait': 'follow_up'}.get(kind)
            if kind == 'prompt' and payload.get('streamingBehavior'):
                priority = {'steer': 'steer', 'followUp': 'follow_up'}[payload['streamingBehavior']]
            message = {'text': payload['message']}
            if payload.get('images'):
                message['images'] = payload['images']
            if priority:
                message['priority'] = priority
            if kind == 'prompt_and_wait':
                return self.prompt_and_wait(session, message, timeout)
            return self.input(session, message, timeout)
        if kind == 'create':
            config = payload.get('config') or {}
            options = {key: config[key] for key in WIRE2_CREATE_OPTIONS if key in config}
            if payload.get('telemetryDisabled') or config.get('telemetryDisabled'):
                options['telemetryDisabled'] = True
            if config.get('parentSessionPath'):
                options['parent'] = {'path': config['parentSessionPath'],
                                     'rlmDepth': config.get('rlmDepth') or 0}
            args = {'options': options}
            for key, value in (('path', payload.get('sessionPath')), ('cwd', config.get('cwd')),
                               ('sessionDir', config.get('sessionDir')),
                               ('launchEnv', payload.get('launchEnv'))):
                if value is not None:
                    args[key] = value
            if payload.get('noSession'):
                args['ephemeral'] = True
            created = self.invoke('session.create', timeout, **args)
            created.update(activeSessionId=created['session'], sessionId=created['session'],
                           sessionFile=created.get('path'))
            if payload.get('name'):
                # The old create named the session; on wire2 a rename does.
                self.invoke('session.rename', timeout, session=created['session'], name=payload['name'])
            return created
        if kind == 'attach':
            self.follow(session, timeout)
            return self.invoke('session.attach', timeout, session=session, **payload)
        if kind == 'detach':
            if not session:
                return None
            detached = self.invoke('session.detach', timeout, session=session)
            self.unsubscribe('agent:' + session)
            return detached
        if kind in WIRE2_SESSION_CALLS:
            return self.invoke(WIRE2_SESSION_CALLS[kind], timeout, session=session, **payload)
        if kind in WIRE2_SESSION_QUERIES:
            return self.query(WIRE2_SESSION_QUERIES[kind], timeout, session=session, **payload)
        if kind == 'get_state':
            return self.query('session.snapshot', timeout, session=session, shape='state')
        if kind == 'get_messages':
            return self.query('transcript.page', timeout, session=session, shape='messages', **payload)
        if kind == 'get_queue':
            # The queue view (`sessionActions`): the lane's tasks and the rows
            # waiting on the agent, as the old queue's two lists.
            actions = self.call('get_state', timeout, activeSessionId=session)['sessionActions']
            return {'steering': actions['steering'], 'followUp': actions['followUps']}
        if kind == 'list':
            return self.query('sessions.list', timeout, open=not payload.get('all'))
        if kind == 'list_saved_sessions':
            # The catalog streams its rows on the request's live-only topic
            # while the daemon walks the store; the reply is the catalog.
            with self.lock:
                self.counter += 1
                stream = f'list-{self.counter}'
            self.subscribe('list:' + stream, 'runtime/session_list/' + stream, timeout=timeout)
            try:
                return self.query('sessions.list', timeout, shape='saved', stream=stream,
                                  **({'activeSessionId': session} if session else {}), **payload)
            finally:
                self.unsubscribe('list:' + stream)
        if kind in ('rename', 'set_session_name'):
            return self.invoke('session.rename', timeout, session=session, name=payload['name'])
        if kind == 'kill':
            return self.invoke('session.stop', timeout, session=session)
        if kind == 'abort':
            return self.invoke('session.abort', timeout, session=session)
        if kind == 'abort_and_send_queued':
            return self.invoke('session.abort_and_send_queued', timeout, session=session)
        if kind == 'set_model':
            return self.invoke('session.set_model', timeout, session=session,
                               provider=payload['provider'], modelId=payload['modelId'])
        if kind == 'set_thinking_level':
            return self.invoke('session.set_effort', timeout, session=session, level=payload['level'])
        if kind == 'wait_for_idle':
            idle = self.invoke('session.wait_idle', timeout, session=session,
                               subagents=payload.get('waitForRlmQuiescence') is True)
            return dict(idle['autonomous'], outstandingBackgroundJobs=idle['outstandingBackgroundJobs'])
        if kind == 'send_message':
            args = {'session': payload['targetActiveSessionId'], 'message': payload['message']}
            if payload.get('fromActiveSessionId'):
                args['from'] = payload['fromActiveSessionId']
            if payload.get('deliveryMode'):
                args['delivery'] = payload['deliveryMode']
            return self.invoke('session.send', timeout, **args)
        if kind == 'acquire_session_input_pause':
            return self.invoke('session.pause_input', timeout, session=session, key=payload.get('leaseKey'))
        if kind == 'release_session_input_pause':
            return self.invoke('session.release_input', timeout, session=session, pauseId=payload.get('pauseId'))
        if kind == 'get_available_models':
            return self.query('models', timeout)
        if kind == 'roster_subscribe':
            self.subscribe('roster', 'runtime/roster', timeout=timeout)
            return self.query('runtime.roster', timeout)
        if kind == 'heartbeats_list':
            jobs = self.query('schedules', timeout, **({'session': session} if session else {}))['jobs']
            return {'heartbeats': [{'job': job} for job in jobs
                                   if job.get('source') in ('heartbeat', 'rlm_heartbeat')]}
        if kind in ('shutdown', 'restart'):
            return self.invoke('daemon.' + kind, timeout, **payload)
        if kind in ('prepare_update_restart', 'commit_update_restart'):
            return self.invoke('daemon.' + kind, timeout, **payload)
        raise AssertionError(f'the runtime daemon has no wire2 form of `{kind}`')

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()
        self.reader.join(timeout=5)


class Screen:
    """Minimal xterm cell capture; raw ANSI is retained separately for audit."""
    def __init__(self, width=120, height=40):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.row = self.col = 0
        self.saved = (0, 0)
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        replies = []
        while self.pending:
            if self.pending[0] == "\x1b":
                if len(self.pending) < 2:
                    break
                if self.pending[1] in "]P_^":
                    end = re.search(r"\x07|\x1b\\", self.pending[2:])
                    if end is None:
                        break
                    self.pending = self.pending[2 + end.end():]
                    continue
                if self.pending[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                    if match is None:
                        break
                    params, _, command = match.groups()
                    self.pending = self.pending[match.end():]
                    nums = [int(x) if x.isdigit() else 0 for x in params.lstrip("?<>").split(";")]
                    n = nums[0] or 1
                    if command in "Hf":
                        self.row = min(self.height - 1, max(0, (nums[0] or 1) - 1))
                        self.col = min(self.width - 1, max(0, ((nums[1] if len(nums) > 1 else 1) or 1) - 1))
                    elif command == "A": self.row = max(0, self.row - n)
                    elif command == "B": self.row = min(self.height - 1, self.row + n)
                    elif command == "C": self.col = min(self.width - 1, self.col + n)
                    elif command == "D": self.col = max(0, self.col - n)
                    elif command == "G": self.col = min(self.width - 1, n - 1)
                    elif command == "J":
                        if nums[0] in (2, 3): self.cells = [[" "] * self.width for _ in range(self.height)]
                        elif nums[0] == 0:
                            self.cells[self.row][self.col:] = [" "] * (self.width - self.col)
                            for row in range(self.row + 1, self.height): self.cells[row] = [" "] * self.width
                    elif command == "K":
                        start, end = (0, self.width) if nums[0] == 2 else ((0, self.col + 1) if nums[0] == 1 else (self.col, self.width))
                        self.cells[self.row][start:end] = [" "] * (end - start)
                    elif command == "P":
                        line = self.cells[self.row]
                        line[self.col:] = (line[self.col + n:] + [" "] * n)[:self.width - self.col]
                    elif command == "X":
                        end = min(self.width, self.col + n)
                        self.cells[self.row][self.col:end] = [" "] * (end - self.col)
                    elif command == "n" and nums[0] == 6:
                        replies.append(f"\x1b[{self.row + 1};{self.col + 1}R".encode())
                    elif command == "c": replies.append(b"\x1b[?1;2c")
                    continue
                command = self.pending[1]
                self.pending = self.pending[2:]
                if command == "7": self.saved = (self.row, self.col)
                elif command == "8": self.row, self.col = self.saved
                elif command in "()" and self.pending: self.pending = self.pending[1:]
                continue
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r": self.col = 0
            elif char == "\n":
                self.row += 1
                if self.row >= self.height:
                    self.cells.pop(0); self.cells.append([" "] * self.width); self.row = self.height - 1
            elif char == "\b": self.col = max(0, self.col - 1)
            elif char >= " " and not unicodedata.combining(char):
                if self.col >= self.width: self.col = 0; self.row = min(self.height - 1, self.row + 1)
                self.cells[self.row][self.col] = char
                self.col += 2 if unicodedata.east_asian_width(char) in "WF" else 1
        return replies


def fixture_state(root, active):
    # AgentConnectionState wire shape.
    # No model selected: model and contextUsage are optional on the wire.
    return {"activeSessionId": active, "cwd": str(root), "sessionId": "fixture-durable",
            "sessionName": "Draft fixture", "thinkingLevel": "off", "serviceTier": "default",
            "availableThinkingLevels": ["off"], "isStreaming": False,
            "isCompacting": False, "isBashRunning": False, "retryAttempt": 0,
            "steeringMode": "all", "followUpMode": "all", "leafId": None,
            "autoCompactionEnabled": False, "messageCount": 0, "compactionCount": 0,
            "sessionActions": {"queuedCount": 0, "steering": [], "followUps": []},
            "goal": {"active": False, "status": "idle", "tokensUsed": 0,
                     "timeUsedSeconds": 0, "continuationsUsed": 0},
            "scopedModels": [], "activeToolNames": []}


class Supervisor:
    def __init__(self, path, hello, scenario, root):
        self.path, self.hello, self.scenario, self.root = path, hello, scenario, root
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path)); self.listener.listen(); self.listener.settimeout(0.2)
        self.done = threading.Event()
        self.lock = threading.Lock()
        self.sockets, self.threads, self.errors = [], [], []
        self.commands, self.prompts, self.prompt_envelopes = [], [], []
        self.accepted_prompts, self.result_acks = [], []
        self.journal, self.settled = {}, set()
        self.messages = []
        self.attaches = 0
        self.closed = False
        self.post_close_attached = False
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def extra_command(self, command, response):
        """Subclass-owned protocol additions, handled before unknown-command errors."""
        return False

    def send(self, sock, data):
        sock.sendall((json.dumps(data) + "\n").encode())

    def settle(self, sock, active, key, index, emit=True):
        # Each admitted synthetic turn finishes once; cached result replay
        # does not execute another turn. No real provider is called.
        marker = f"Fixture settled {index}"
        message = {"role": "assistant", "stopReason": "stop",
                   "api": "openai-completions", "provider": "parity-local", "model": "parity-1",
                   "timestamp": 1750000000000, "usage": {"input": 0, "output": 0,
                   "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                   "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
                   "content": [{"type": "text", "text": marker}]}
        with self.lock:
            if key in self.settled:
                return
            self.settled.add(key)
            prompt = self.prompts[index - 1]
            self.messages.extend([{"role": "user", "content": [{"type": "text", "text": prompt["message"]}],
                                   "timestamp": 1750000000000}, message])
        if not emit:
            return  # Completed history survives lost events and is delivered by attach.
        for event in ({"type": "turn_start"},
                      {"type": "message_start", "message": message},
                      {"type": "message_end", "message": message},
                      {"type": "turn_end", "message": message, "toolResults": []}):
            self.send(sock, {"type": "session_event", "activeSessionId": active, "event": event})

    def accept(self):
        while not self.done.is_set():
            try: sock, _ = self.listener.accept()
            except socket.timeout: continue
            except OSError: return
            sock.settimeout(0.2)
            self.sockets.append(sock)
            thread = threading.Thread(target=self.serve, args=(sock,), daemon=True)
            self.threads.append(thread); thread.start()

    def serve(self, sock):
        # The protocol requires this for prompt dispatch. Advertise only
        # the admission path implemented here; leave direct-peer routing disabled.
        hello = dict(self.hello, serverCapabilities=["session_input_admission"], clientId="draft-fixture")
        buffer = b""
        try:
            self.send(sock, hello)
            while not self.done.is_set():
                try: data = sock.recv(65536)
                except socket.timeout: continue
                if not data: return
                buffer += data
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    if not line.strip(): continue
                    envelope = json.loads(line)
                    command = envelope.get("command", envelope)
                    name = command.get("type")
                    request_id = envelope.get("id")
                    client_id = envelope.get("clientId")
                    key = (client_id, request_id)
                    with self.lock:
                        self.commands.append(command)
                    active = "s1"
                    response = {"type": "response", "id": request_id, "command": name,
                                "success": True, "data": {}}
                    if name == "ack_result":
                        # The supervisor acknowledges its journal and returns nothing;
                        # this one-way command must not receive a synthetic response.
                        with self.lock:
                            ack_key = (client_id, command.get("commandId"))
                            self.result_acks.append({"clientId": client_id, "commandId": command.get("commandId")})
                            self.journal.pop(ack_key, None)
                        continue
                    if name == "create":
                        response["data"] = {"activeSessionId": active, "id": active,
                            "sessionId": "fixture-durable", "sessionFile": str(self.root / "agent/sessions/fixture.jsonl")}
                    elif name == "attach":
                        state = fixture_state(self.root, active)
                        with self.lock:
                            messages = list(self.messages)
                        state["messageCount"] = len(messages)
                        response["data"] = {"protocol": hello["protocol"], "activeSessionId": active,
                            "snapshot": {"activeSessionId": active, "summary": {"id": active, "cwd": str(self.root)},
                                         "state": state, "messages": messages, "lastEventSequence": 0, "lastEventCursor": None},
                            "client": {"id": "draft-fixture", "capabilities": []},
                            "lastEventSequence": 0, "lastEventCursor": None}
                        with self.lock:
                            self.attaches += 1
                            if self.closed: self.post_close_attached = True
                    elif name == "prompt":
                        # daemon-mode's accepted prompt success has no data payload.
                        response.pop("data", None)
                        if not client_id or not request_id:
                            raise RuntimeError("prompt envelope lacks clientId/id for recovery identity")
                        with self.lock:
                            self.prompt_envelopes.append({"id": request_id, "clientId": client_id, "command": command})
                            cached = self.journal.get(key)
                        if cached is not None:
                            # Supervisor semantics: (clientId, commandId) completion
                            # returns the cached result without dispatching the prompt again.
                            cached_response, index = cached
                            self.send(sock, cached_response)
                            if cached_response["success"]:
                                self.settle(sock, active, key, index)
                            continue
                        with self.lock:
                            self.prompts.append(command)
                            index = len(self.prompts)
                            self.journal[key] = (dict(response), index)
                            if response["success"]:
                                self.accepted_prompts.append({"id": request_id, "clientId": client_id,
                                                              "command": command})
                    elif name == "list": response["data"] = {"sessions": []}
                    elif name == "heartbeats_list": response["data"] = {"heartbeats": []}
                    elif name == "roster_subscribe": response["data"] = {"changed": [], "removed": [], "resync": True}
                    elif name == "get_available_models": response["data"] = {"models": []}
                    elif name == "get_resource_snapshot":
                        # AgentConnectionResourceSnapshot wire shape.
                        response["data"] = {"contextFiles": [], "skills": [], "prompts": [],
                                            "extensions": [], "themes": [], "diagnostics": {
                                            "skills": [], "prompts": [], "extensions": [], "themes": []}}
                    elif name == "get_session_stats":
                        # AgentSession.getSessionStats's empty-session return shape.
                        response["data"] = {"sessionId": "fixture-durable", "userMessages": 0,
                                            "assistantMessages": 0, "toolCalls": 0, "toolResults": 0,
                                            "totalMessages": 0, "cost": 0, "tokens": {
                                            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}
                        with self.lock:
                            response["data"].update(userMessages=len(self.messages) // 2,
                                                    assistantMessages=len(self.messages) // 2,
                                                    totalMessages=len(self.messages))
                    elif name == "get_context_tree":
                        # AgentSession.getContextTree / ContextTreeNode and
                        # usage.emptyUsage: an empty session is a single active root.
                        usage = {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                                 "totalTokens": 0, "cost": {"input": 0, "output": 0,
                                 "cacheRead": 0, "cacheWrite": 0, "total": 0}}
                        response["data"] = {"id": "root", "label": "Draft fixture", "status": "active",
                                            "ownUsage": usage, "totalUsage": usage, "children": []}
                    elif name == "get_model_catalog": response["data"] = {"models": [], "configuredProviders": []}
                    elif name == "get_commands": response["data"] = {"commands": []}
                    elif name == "list_kernel_bash": response["data"] = {"commands": []}
                    elif name == "factory_activity": response["data"] = {"activity": [], "jobs": []}
                    elif name in ("get_state", "get_connection_state"):
                        response["data"] = fixture_state(self.root, active)
                        with self.lock:
                            response["data"]["messageCount"] = len(self.messages)
                    elif name == "get_messages":
                        # pa-daemon worker::commands::handle_get_messages returns
                        # the actual transcript.
                        # Fixture turns only append; no branches/pagination are used.
                        with self.lock:
                            response["data"] = {"messages": list(self.messages)}
                    elif name == "get_session_context":
                        # pa-daemon state_getters::handle_get_session_context's
                        # context envelope. This fixture has no compaction or
                        # model/thinking/tier change rows: build_session_context
                        # preserves its turns and uses off/None/None defaults.
                        with self.lock:
                            response["data"] = {"context": {
                                "messages": list(self.messages), "thinkingLevel": "off",
                                "serviceTier": None, "model": None}}
                    elif name == "get_mcp_connections": response["data"] = {"connections": []}
                    elif name == "detach": response["data"] = {}
                    elif name == "set_auto_compaction":
                        if not isinstance(command.get("enabled"), bool):
                            raise RuntimeError("set_auto_compaction requires a boolean")
                        response.pop("data", None)
                    elif self.extra_command(command, response):
                        pass
                    else:
                        self.errors.append(f"unexpected command: {name}")
                        response.update(success=False, error=f"Unsupported fixture command: {name}")
                    self.send(sock, response)
                    if name == "prompt" and response["success"]:
                        self.settle(sock, active, key, index)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            if not self.done.is_set(): self.errors.append(f"{type(error).__name__}: {error}")
        finally:
            sock.close()

    def close(self):
        self.done.set(); self.listener.close()
        for sock in self.sockets:
            try: sock.shutdown(socket.SHUT_RDWR)
            except OSError: pass
        self.thread.join(timeout=2)
        for thread in self.threads: thread.join(timeout=2)
        if self.thread.is_alive() or any(thread.is_alive() for thread in self.threads):
            raise RuntimeError("owned supervisor threads did not terminate")


class Terminal:
    def __init__(self, command, root, env):
        self.master, slave = pty.openpty()
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~(termios.ECHO | termios.ICANON)
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.process = subprocess.Popen(command, cwd=root, env=env, stdin=slave,
            stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)
        self.screen, self.raw = Screen(), bytearray()

    def pump(self):
        if select.select([self.master], [], [], 0.05)[0]:
            try: data = os.read(self.master, 65536)
            except OSError: data = b""
            self.raw.extend(data)
            for reply in self.screen.feed(data): os.write(self.master, reply)

    def wait(self, label, predicate, server):
        deadline = time.monotonic() + LIMIT
        while time.monotonic() < deadline:
            self.pump()
            if server.errors: raise RuntimeError(f"supervisor fixture error: {server.errors}")
            if self.process.poll() is not None: raise RuntimeError(f"UI exited during {label}: {self.process.returncode}")
            if predicate(): return
        raise TimeoutError(label)

    def send(self, data): os.write(self.master, data.encode())

    def close(self):
        stop(self.process)
        os.close(self.master)


TAG = 'PA_GOAL_FIXTURE_ID'


def process_identity(pid):
    stat = Path('/proc/%d/stat' % pid).read_text()
    fields = stat[stat.rfind(')') + 2:].split()
    return 'proc:' + fields[19], fields[0]


# A session is hosted inside the daemon process: its descriptor's `pid` is an id
# from this range (never a process id) and its `processStartId` names the hosting
# daemon and one lifetime, `hosted:<daemon pid>:<lifetime>`.
HOSTED_PID_BASE = 0x40000000


def session_serving(socket_path):
    """Whether something accepts connections on a session's socket."""
    # Through the directory's descriptor: a session socket's full path can
    # exceed what a socket address holds.
    try:
        directory = os.open(os.path.dirname(str(socket_path)), os.O_RDONLY | os.O_DIRECTORY)
    except FileNotFoundError:
        return False
    probe = socket.socket(socket.AF_UNIX)
    try:
        probe.connect('/proc/self/fd/%d/%s' % (directory, os.path.basename(str(socket_path))))
        return True
    except (FileNotFoundError, ConnectionRefusedError):
        return False
    finally:
        probe.close()
        os.close(directory)


def verify_session(row, active, binary, socket_path, daemon_pid):
    """Prove a descriptor names a live session hosted by the fixture's own daemon.

    Returns the session's socket and its lifetime's generation: a rebuilt
    session keeps the socket and gets a new generation.
    """
    assert row.get('rootActiveSessionId') == active, 'wrong descriptor identity'
    assert row.get('supervisorSocketPath') == str(socket_path), 'wrong supervisor descriptor'
    command = Path('/proc/%d/cmdline' % daemon_pid).read_bytes().split(b'\0')
    assert command[0] == str(binary).encode(), 'session host argv0 is not fixture binary'
    pid = row.get('pid')
    assert isinstance(pid, int) and pid >= HOSTED_PID_BASE, 'session is not hosted in the daemon'
    generation = row.get('processStartId') or ''
    assert generation.startswith('hosted:%d:' % daemon_pid), 'session hosted by another daemon'
    assert session_serving(row['socketPath']), 'hosted session is not serving its socket'
    return row['socketPath'], generation



def verify_runtime_session(client, active, binary, daemon_pid):
    """Prove `active` is a session the fixture's own runtime daemon holds open:
    the daemon is the fixture binary and lists it among its open sessions
    (a runtime session runs inside the daemon; it has no process or socket of
    its own). Returns its row of `sessions.list`."""
    command = Path('/proc/%d/cmdline' % daemon_pid).read_bytes().split(b'\0')
    assert command[0] == str(binary).encode(), 'session host argv0 is not fixture binary'
    rows = client.query('sessions.list', open=True)['sessions']
    matches = [row for row in rows if row.get('session') == active]
    assert len(matches) == 1 and matches[0].get('open') is True, 'session is not open in the fixture daemon'
    return matches[0]


class OwnedProcesses:
    def __init__(self, binary, root, socket_path):
        self.binary, self.root, self.socket_path = binary, root, socket_path
        self.direct = {}
        self.targets = {}
        self.errors = []

    def register(self, process, command):
        identity, state = process_identity(process.pid)
        assert state != 'Z', 'fixture process exited before identity capture'
        self.direct[process.pid] = (identity, [str(arg).encode() for arg in command])
        self.targets[process.pid] = identity

    def ownership(self, pid, identity):
        try:
            current, state = process_identity(pid)
            if current != identity or state == 'Z':
                return False
            env = dict(item.split(b'=', 1) for item in Path('/proc/%d/environ' % pid).read_bytes().split(b'\0') if b'=' in item)
            if env.get(TAG.encode()) != str(self.root).encode():
                return False
            command = Path('/proc/%d/cmdline' % pid).read_bytes().rstrip(b'\0').split(b'\0')
            if pid in self.direct:
                return self.direct[pid] == (identity, command)
            if command[0] == str(self.binary).encode():
                # Sessions run inside the registered daemon: no other process
                # of the fixture binary belongs to this fixture.
                return False
            # Kernels retain the owner PID supplied by the actual launcher.
            return ('python' in Path(command[0].decode()).name
                    and env.get(b'PRIME_AGENT_KERNEL_OWNER_PID') in
                    {str(owner).encode() for owner in self.targets})
        except (OSError, ValueError, KeyError, AssertionError, UnicodeError):
            return False

    def discover(self):
        for entry in Path('/proc').iterdir():
            if entry.name.isdigit():
                pid = int(entry.name)
                try:
                    identity, _ = process_identity(pid)
                    if self.ownership(pid, identity):
                        self.targets[pid] = identity
                except OSError:
                    pass

    def live(self):
        alive = []
        for pid, identity in list(self.targets.items()):
            try:
                current, state = process_identity(pid)
                if current == identity and state != 'Z':
                    alive.append(pid)
            except (FileNotFoundError, ProcessLookupError):
                pass
        return alive

    def wait_gone(self, timeout):
        deadline = time.monotonic() + timeout
        while True:
            self.discover()
            alive = self.live()
            if not alive:
                return
            if time.monotonic() >= deadline:
                raise AssertionError('owned goal fixture processes remained: ' + str(alive))
            threading.Event().wait(0.025)

    def send(self, pid, signum):
        identity = self.targets[pid]
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return False
        try:
            if not self.ownership(pid, identity):
                return False
            signal.pidfd_send_signal(fd, signum)
            return True
        except ProcessLookupError:
            return False
        finally:
            os.close(fd)


def env_agent_dir(env):
    """The agent dir a product process run with `env` resolves."""
    if env.get("PRIME_AGENT_CODING_AGENT_DIR"):
        return Path(env["PRIME_AGENT_CODING_AGENT_DIR"])
    return Path(env["HOME"]) / ".prime/agent"


def saved_transcripts(agent_dir, binary=None, timeout=60, pattern="*.jsonl"):
    """The session transcripts the product keeps for `agent_dir`, by locator
    (resolved), as text. Sessions live in the agent dir's database (`agent.db`),
    not in JSONL files: `prime-agent export-jsonl --all` writes them out under the
    agent dir's layout (`other/<absolute path>` for locators outside it), and this
    maps each file back to its locator. `binary` defaults to `PA_RUST_BINARY`."""
    binary = binary or os.environ["PA_RUST_BINARY"]
    agent = Path(agent_dir).resolve()
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TZ") if key in os.environ}
    env.update(HOME=str(agent.parent), PRIME_AGENT_CODING_AGENT_DIR=str(agent), DO_NOT_TRACK="1",
               PI_OFFLINE="1")
    with tempfile.TemporaryDirectory() as out:
        subprocess.run([str(binary), "export-jsonl", "--all", "--out", out], env=env,
                       stdin=subprocess.DEVNULL, capture_output=True, check=True, timeout=timeout)
        found = {}
        for path in sorted(Path(out).rglob(pattern)):
            relative = path.relative_to(out)
            if relative.parts[0] == "other":
                locator = Path("/", *relative.parts[1:])
            else:
                locator = agent / relative
            found[locator] = path.read_text()
        return found


def has_database(agent_dir):
    """Whether a product process opened the transcript database of `agent_dir`
    (before any did, its sessions can only be JSONL files)."""
    return (Path(agent_dir) / "agent.db").is_file()


def transcripts_under(agent_dir, directory, binary=None):
    """Every saved transcript under `directory` (recursively), as (locator, text)
    sorted by locator; before any process opened the agent dir's database, the
    `.jsonl` files there as they are."""
    root = Path(directory).resolve()
    if has_database(agent_dir):
        found = saved_transcripts(agent_dir, binary).items()
    else:
        found = [(path.resolve(), path.read_text()) for path in sorted(root.rglob("*.jsonl"))]
    return [(locator, text) for locator, text in found if root in locator.parents]


def session_transcripts(agent_dir, directory, binary=None):
    """The saved sessions under `directory` (see `transcripts_under`): the
    transcripts whose first line is a `session` header."""
    return [(locator, text) for locator, text in transcripts_under(agent_dir, directory, binary)
            if text.strip() and json.loads(text.splitlines()[0]).get("type") == "session"]


def transcript_text(agent_dir, locator, binary=None):
    """The saved session at `locator` as text (see `saved_transcripts`)."""
    wanted = Path(locator)
    if not has_database(agent_dir):
        return wanted.read_text()
    wanted = wanted.parent.resolve() / wanted.name
    transcripts = saved_transcripts(agent_dir, binary)
    if wanted not in transcripts:
        raise FileNotFoundError("no saved session at %s: %s" % (locator, sorted(transcripts)))
    return transcripts[wanted]


def state_file_text(agent_dir, locator, binary=None):
    """The agent state file at `locator` (a harness store, `scheduled-jobs.json`,
    the event bus) as text: the database's copy, which `export-jsonl --all`
    writes in the file's layout, or the file itself without a database."""
    wanted = Path(locator)
    if not has_database(agent_dir):
        return wanted.read_text()
    wanted = wanted.parent.resolve() / wanted.name
    files = saved_transcripts(agent_dir, binary, pattern="*.json")
    if wanted not in files:
        raise FileNotFoundError("no saved state at %s: %s" % (locator, sorted(files)))
    return files[wanted]


def finish(scope, client, daemon, observation):
    """Fallback can reap leftovers but never converts failed shutdown into success."""
    observation['cleanup_verified'] = False
    try:
        scope.discover()
        if daemon is not None:
            assert client is not None, 'owned daemon cleanup client unavailable'
            client.call('shutdown', force=True)
            observation['shutdown_wire'] = client.wire
            client.close()  # The daemon's close waits for raw fixture connections.
            daemon.wait(timeout=15)
            assert daemon.returncode == 0, 'owned goal daemon shutdown failed'
        scope.wait_gone(5)
        observation['cleanup_verified'] = True
    except Exception as error:
        observation['cleanup_error'] = str(error)
        observation['fallback_signals'] = []
        for signum, timeout in ((signal.SIGTERM, 3), (signal.SIGKILL, 2)):
            try:
                scope.discover()
            except Exception as discovery_error:
                scope.errors.append('fallback discovery failed: ' + str(discovery_error))
            for pid in scope.live():
                try:
                    if scope.send(pid, signum):
                        observation['fallback_signals'].append({'pid': pid, 'signal': signum.name})
                except OSError as signal_error:
                    scope.errors.append(str(signal_error))
            try:
                scope.wait_gone(timeout)
                break
            except Exception as wait_error:
                scope.errors.append('fallback wait failed: ' + str(wait_error))
        observation['fallback_remaining'] = scope.live()
        observation['fallback_errors'] = scope.errors
    finally:
        if client is not None:
            try:
                client.close()
            except Exception as close_error:
                observation['cleanup_verified'] = False
                observation.setdefault('cleanup_error', str(close_error))
        observation['owned_process_identities'] = dict(scope.targets)


class DockScreen:
    """Existing xterm cell capture with explicit per-cell SGR attributes."""
    def __init__(self, width=120, height=40):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.styles = [[{} for _ in range(width)] for _ in range(height)]
        self.sgr = {}
        self.unsupported_sgr = []
        self.unsupported_terminal = []
        self.keyboard_controls = []
        self.autowrap = True
        self.alternate = None
        self.row = self.col = 0
        self.saved = (0, 0, {})
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        replies = []
        while self.pending:
            if self.pending[0] == "\x1b":
                if len(self.pending) < 2:
                    break
                if self.pending[1] in "]P_^":
                    end = re.search(r"\x07|\x1b\\", self.pending[2:])
                    if end is None:
                        break
                    sequence = self.pending[:2 + end.end()]
                    # OSC titles/hyperlinks do not move terminal cells. DCS,
                    # APC and PM can carry graphics or sixel: never silently
                    # accept their unknown effect on the captured frame.
                    payload = self.pending[2:2 + end.start()]
                    osc_kind = payload.split(';', 1)[0]
                    # Terminals may be asked for the default foreground/background via
                    # OSC10;? / OSC11;?. Queries
                    # do not paint or set a color; a legacy PTY may stay silent.
                    # Color-setting variants remain unsupported and fail closed.
                    color_query = payload in ('10;?', '11;?')
                    if self.pending[1] != ']' or (not color_query and osc_kind not in ('0', '1', '2', '7', '8', '52', '133')):
                        self.unsupported_terminal.append(sequence)
                    self.pending = self.pending[2 + end.end():]
                    continue
                if self.pending[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                    if match is None:
                        break
                    params, intermediate, command = match.groups()
                    sequence = self.pending[:match.end()]
                    self.pending = self.pending[match.end():]
                    nums = [int(x) if x.isdigit() else 0 for x in params.lstrip("?<>").split(";")]
                    n = nums[0] or 1
                    if intermediate and not (command == 'q' and intermediate == ' '):
                        self.unsupported_terminal.append(sequence)
                        continue
                    # Actual hosted captures contain these keyboard
                    # protocol controls. They do not paint cells and must not
                    # be interpreted as SGR4 (underline) after stripping >.
                    if command == 'm' and params in ('>4;0', '>4;1', '>4;2'):
                        self.keyboard_controls.append(sequence)
                        continue
                    if command == 'u' and params == '?':
                        self.keyboard_controls.append(sequence)
                        # Even a zero-flags reply advertises Kitty support to
                        # crossterm (vendor terminal/sys/unix.rs:255). This PTY
                        # emits legacy keys: stay silent on the Kitty query.
                        # The following DA1 query's response settles non-Kitty
                        # through KeyboardEnhancementFlagsFilter instead.
                        continue
                    if command == 'u' and (re.fullmatch(r'[>=]\d+(?:;\d+)?', params)
                                           or re.fullmatch(r'<\d*', params)):
                        self.keyboard_controls.append(sequence)
                        continue
                    if params.startswith(('?', '<', '>')) and command not in 'hlnc':
                        self.unsupported_terminal.append(sequence)
                        continue
                    if command == "m": self.apply_sgr(nums)
                    elif command in "Hf":
                        self.row = min(self.height - 1, max(0, (nums[0] or 1) - 1))
                        self.col = min(self.width - 1, max(0, ((nums[1] if len(nums) > 1 else 1) or 1) - 1))
                    elif command == "A": self.row = max(0, self.row - n)
                    elif command == "B": self.row = min(self.height - 1, self.row + n)
                    elif command == "C": self.col = min(self.width - 1, self.col + n)
                    elif command == "D": self.col = max(0, self.col - n)
                    elif command == "G": self.col = min(self.width - 1, n - 1)
                    elif command == "J":
                        if nums[0] == 2:
                            self.cells = [[" "] * self.width for _ in range(self.height)]
                            self.styles = [[dict(self.sgr) for _ in range(self.width)] for _ in range(self.height)]
                        elif nums[0] == 0:
                            self.cells[self.row][self.col:] = [" "] * (self.width - self.col)
                            self.styles[self.row][self.col:] = [dict(self.sgr) for _ in range(self.width - self.col)]
                            for row in range(self.row + 1, self.height):
                                self.cells[row] = [" "] * self.width
                                self.styles[row] = [dict(self.sgr) for _ in range(self.width)]
                        elif nums[0] != 3:  # ED3 clears scrollback, not visible cells.
                            self.unsupported_terminal.append(sequence)
                    elif command == "K":
                        if nums[0] not in (0, 1, 2):
                            self.unsupported_terminal.append(sequence)
                            continue
                        start, end = (0, self.width) if nums[0] == 2 else ((0, self.col + 1) if nums[0] == 1 else (self.col, self.width))
                        self.cells[self.row][start:end] = [" "] * (end - start)
                        self.styles[self.row][start:end] = [dict(self.sgr) for _ in range(end - start)]
                    elif command == "P":
                        line = self.cells[self.row]
                        line[self.col:] = (line[self.col + n:] + [" "] * n)[:self.width - self.col]
                        styles = self.styles[self.row]
                        styles[self.col:] = (styles[self.col + n:] + [dict(self.sgr) for _ in range(n)])[:self.width - self.col]
                    elif command == "X":
                        end = min(self.width, self.col + n)
                        self.cells[self.row][self.col:end] = [" "] * (end - self.col)
                        self.styles[self.row][self.col:end] = [dict(self.sgr) for _ in range(end - self.col)]
                    elif command == "n" and nums[0] == 6:
                        replies.append(f"\x1b[{self.row + 1};{self.col + 1}R".encode())
                    elif command == "c": replies.append(b"\x1b[?1;2c")
                    elif command in 'hl' and params.startswith('?'):
                        for mode in nums:
                            if mode == 7:
                                self.autowrap = command == 'h'
                            elif mode == 1049:
                                if command == 'h' and self.alternate is None:
                                    self.alternate = (self.cells, self.styles, self.row, self.col,
                                                      dict(self.sgr), self.autowrap)
                                    self.cells = [[' '] * self.width for _ in range(self.height)]
                                    self.styles = [[{} for _ in range(self.width)] for _ in range(self.height)]
                                    self.row = self.col = 0
                                elif command == 'l' and self.alternate is not None:
                                    self.cells, self.styles, self.row, self.col, self.sgr, self.autowrap = self.alternate
                                    self.alternate = None
                            elif mode not in (12, 25, 1000, 1002, 1003, 1004, 1006, 2004, 2026):
                                self.unsupported_terminal.append(sequence)
                    elif command == 'q' and intermediate == ' ':
                        pass  # Cursor presentation, not cell geometry.
                    elif command == 't' and params in ('22;0', '23;0'):
                        pass  # XTWINOPS title-stack push/pop: no cell effect.
                    elif command == 'r' and params == '':
                        # DECSTBM reset to the full screen homes the cursor; a
                        # partial region would change scrolling and stays unsupported.
                        self.row = self.col = 0
                    else:
                        self.unsupported_terminal.append(sequence)
                    continue
                command = self.pending[1]
                if command in '()' and len(self.pending) < 3:
                    break
                self.pending = self.pending[2:]
                if command == "7": self.saved = (self.row, self.col, dict(self.sgr))
                elif command == "8":
                    self.row, self.col, saved_sgr = self.saved
                    self.sgr = dict(saved_sgr)
                elif command in "()" and self.pending:
                    charset = self.pending[0]
                    self.pending = self.pending[1:]
                    if charset != 'B':
                        self.unsupported_terminal.append('\x1b' + command + charset)
                else:
                    self.unsupported_terminal.append('\x1b' + command)
                continue
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r": self.col = 0
            elif char == "\n":
                self.row += 1
                if self.row >= self.height:
                    self.cells.pop(0); self.cells.append([" "] * self.width)
                    self.styles.pop(0); self.styles.append([dict(self.sgr) for _ in range(self.width)])
                    self.row = self.height - 1
            elif char == "\b": self.col = max(0, self.col - 1)
            elif char == '\t': self.col = min(self.width - 1, ((self.col // 8) + 1) * 8)
            elif char >= " " and not unicodedata.combining(char):
                if self.col >= self.width:
                    if self.autowrap:
                        self.col = 0
                        self.row += 1
                        if self.row == self.height:
                            self.cells.pop(0); self.cells.append([' '] * self.width)
                            self.styles.pop(0); self.styles.append([dict(self.sgr) for _ in range(self.width)])
                            self.row = self.height - 1
                    else:
                        self.col = self.width - 1
                self.cells[self.row][self.col] = char
                self.styles[self.row][self.col] = dict(self.sgr)
                width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                if width == 2 and self.col + 1 < self.width:
                    # A terminal paints both cells of a wide glyph, replacing
                    # any old text and background in its continuation cell.
                    self.cells[self.row][self.col + 1] = ' '
                    self.styles[self.row][self.col + 1] = dict(self.sgr)
                self.col += width
            elif char not in ('\x00', '\x07') and not unicodedata.combining(char):
                self.unsupported_terminal.append(char)
        return replies

    def apply_sgr(self, nums):
        i = 0
        while i < len(nums):
            value = nums[i]
            if value == 0:
                self.sgr = {}
            elif value in (1, 2, 3, 4, 7, 9):
                self.sgr[str(value)] = True
            elif value in (22, 23, 24, 27, 29):
                for key in {22: ('1', '2'), 23: ('3',), 24: ('4',), 27: ('7',), 29: ('9',)}[value]:
                    self.sgr.pop(key, None)
            elif 30 <= value <= 37 or 90 <= value <= 97:
                self.sgr['fg'] = ['ansi', value]
            elif 40 <= value <= 47 or 100 <= value <= 107:
                self.sgr['bg'] = ['ansi', value]
            elif value in (39, 49):
                self.sgr.pop('fg' if value == 39 else 'bg', None)
            elif value in (38, 48) and i + 1 < len(nums):
                count = {5: 1, 2: 3}.get(nums[i + 1])
                if count is not None and i + 2 + count <= len(nums):
                    self.sgr['fg' if value == 38 else 'bg'] = nums[i + 1:i + 2 + count]
                    i += 1 + count
                else:
                    self.unsupported_sgr.append(nums[i:])
                    break
            else:
                self.unsupported_sgr.append([value])
            i += 1

    def dock_styles(self):
        return [{"text": ''.join(row).rstrip(), "styles": [dict(style) for style in self.styles[index]]}
                for index, row in enumerate(self.cells)
                if 'subagents' in ''.join(row) or 'running' in ''.join(row)]
