#!/usr/bin/env python3
"""Actual daemon/kernel launch-env check; owned Linux CI processes only.

Exercises raw create wire (not TUI sender rendering), isolation, resident crash
recovery, owned recovery, promotion lifetime, and durable secret exclusion.
Requests/responses and actual provider tool results are retained. No ambient
credential is copied. Sessions run inside the fixture's own daemon: recovery
crashes only a descriptor-verified session of this fixture, through that
session's own cell, and signals no process.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

from check_agent_bounce import Client
from lifecycle_lib import digest, env_for, session_serving, stop, transcript_text, verify_session
from check_restore_notice import texts

MARKER = 'LAUNCH_ENV_PARITY='
LIMIT = 35
CODE = """(define (command-env name)
  (let ([seen (hash-ref (bash (string-append "printf '%s' \\"${" name "-__UNSET__}\\"")) 'output)])
    (if (equal? seen "__UNSET__") void seen)))
(define result
  (hash 'env (hash 'PA_ENV_MARKER (command-env "PA_ENV_MARKER")
                   'PA_ENV_BASE (command-env "PA_ENV_BASE")
                   'PA_ENV_PRECEDENCE (command-env "PA_ENV_PRECEDENCE")
                   'PRIME_AGENT_INTERNAL_PARITY_CANARY (command-env "PRIME_AGENT_INTERNAL_PARITY_CANARY"))
        'child_marker (hash-ref (bash "sh -c 'printf %s \\"${PA_ENV_MARKER:-ABSENT}\\"'") 'output)))
(displayln (string-append "LAUNCH_ENV_PARITY=" (value->jsexpr-string result)))"""


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.phase = None
        self.code = CODE
        self.requests = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        if self.path != '/v1/chat/completions':
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        phase = self.server.phase
        requests = self.server.requests.setdefault(phase, [])
        requests.append(body)
        if len(requests) == 1:
            offered = {tool.get('function', {}).get('name') for tool in body.get('tools', [])}
            name = 'eval' if 'eval' in offered else None
            if name is None:
                self.send_error(422, 'missing kernel tool')
                return
            delta = {'tool_calls': [{'index': 0, 'id': 'launch_' + phase, 'type': 'function',
                'function': {'name': name, 'arguments': json.dumps({'code': self.server.code})}}]}
            finish = 'tool_calls'
        else:
            delta, finish = {'content': 'Launch environment fixture complete.'}, 'stop'
        def chunk(delta, reason=None):
            return {'id': 'launch-env', 'object': 'chat.completion.chunk', 'created': 1,
                'model': 'parity-1', 'choices': [{'index': 0, 'delta': delta, 'finish_reason': reason}]}
        wire = ''.join('data: ' + json.dumps(row) + '\n\n' for row in (
            chunk({'role': 'assistant', **delta}), chunk({}, finish))) + 'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(wire.encode())))
        self.end_headers()
        self.wfile.write(wire.encode())


def current_probe(requests, phase):
    assert len(requests) == 2, 'unexpected provider sequence'
    matches = [message for message in requests[-1]['messages'] if message.get('role') == 'tool'
               and message.get('tool_call_id') == 'launch_' + phase]
    assert len(matches) == 1, 'missing/duplicate current tool result'
    probes = [json.loads(line[len(MARKER):]) for text in texts(matches[0].get('content'))
              for line in text.splitlines() if line.startswith(MARKER)]
    assert len(probes) == 1, 'missing/duplicate current probe'
    return probes[0]


class LaunchClient(Client):
    def event(self, active, kind, since):
        """The first `kind` event of session `active` among the frames from index `since` on."""
        index = since
        deadline = time.monotonic() + LIMIT
        while time.monotonic() < deadline:
            if index == len(self.wire):
                line = self.reader.readline()
                assert line, 'daemon closed connection'
                self.wire.append(json.loads(line))
            frame = self.wire[index]
            index += 1
            if (frame.get('type') == 'session_event' and frame.get('activeSessionId') == active
                    and frame.get('event', {}).get('type') == kind):
                return frame['event']
        raise TimeoutError('session event never arrived: ' + kind)


def worker_descriptor(agent, active, socket_path, deadline=None, previous=None):
    deadline = deadline or time.monotonic() + LIMIT
    while time.monotonic() < deadline:
        candidates = list((agent / 'daemon-workers').rglob('*.json'))
        for path in candidates:
            try:
                row = json.loads(path.read_text())
                pid = row.get('pid')
                if (row.get('rootActiveSessionId') == active
                        and row.get('supervisorSocketPath') == str(socket_path)
                        and isinstance(pid, int) and pid > 0 and pid != previous):
                    return path, row
            except (OSError, ValueError):
                pass  # Atomic/transitional descriptor writes are not readiness.
        threading.Event().wait(0.025)
    raise AssertionError('fixture worker descriptor never reached readiness')


def wait_gone(sessions):
    """Sessions die with the daemon that hosts them: nothing may still serve their sockets."""
    deadline = time.monotonic() + LIMIT
    while time.monotonic() < deadline:
        serving = [path for path, _generation in sessions if session_serving(path)]
        if not serving:
            return
        threading.Event().wait(0.025)
    raise AssertionError('owned fixture sessions were still served after shutdown: ' + str(serving))


def restart(client, provider, phase, active, agent, binary, config, owned, observation, tracked, session_info):
    socket_path = agent.parent / 'd.sock'
    path, row = worker_descriptor(agent, active, socket_path)
    tracked.add(verify_session(row, active, binary, socket_path, client.process.pid))
    pid = row['pid']
    # End the session with no teardown of its own: its cell outlives the abort
    # (the daemon's stuck-cell grace is the fixture's), so the daemon ends the
    # session as crashed.
    # The cell enters the kernel's test seam (debug builds, asked for in the
    # daemon's environment): a native call no interrupt reaches, which writes
    # its marker once it is inside and returns when the marker is gone.
    marker = agent.parent / ('wedged-' + phase)
    provider.phase, provider.code = phase, '(%%test-block %s)' % json.dumps(str(marker))
    since = len(client.wire)
    client.call('prompt', activeSessionId=active, message='Wedge ' + phase)
    deadline = time.monotonic() + LIMIT
    while not marker.exists():
        assert time.monotonic() < deadline, 'the cell never entered its blocking call'
        time.sleep(0.02)
    client.call('abort', activeSessionId=active)
    crashed = observation['crash'] = client.event(active, 'session_crashed', since)
    provider.code = CODE
    marker.unlink()
    assert crashed.get('kind') == 'stuck', crashed
    # The daemon rebuilds the crashed session and tells its attached client.
    observation['recovered'] = client.event(active, 'session_recovered', since)
    if owned:
        # Same owner context; refreshed attach launchEnv is deliberately out of scope.
        client.call('attach', activeSessionId=active, recoveryConfig=config)
        observation['recovery_policy'] = 'Owned recovery retained the original client launchEnv'
    else:
        observation['recovery_policy'] = 'Automatically relaunched from durable descriptor without retained client launchEnv'
    new_path, new_row = worker_descriptor(agent, active, socket_path, previous=pid)
    assert new_path == path
    replacement = verify_session(new_row, active, binary, socket_path, client.process.pid)
    tracked.add(replacement)
    observation['replacement_session_lifetime'] = replacement[1]
    if not owned:
        client.call('attach', activeSessionId=active)
    state = client.call('get_state', activeSessionId=active)
    observation['replacement_state'] = state
    assert state['sessionId'] == session_info['sessionId'], 'recovery changed durable session identity'
    assert state['sessionFile'] == session_info['sessionFile'], 'recovery changed durable session file'
    return active


def finish_daemon(client, process, kind, **payload):
    client.call(kind, **payload)  # Successful acknowledgement is mandatory.
    client.close()  # The daemon's close waits for every fixture client to end.
    process.wait(timeout=15)
    assert process.returncode == 0, 'supervisor did not shut down successfully'


def run_case(binary, root, observed):
    env = env_for(root)
    env.update(PA_ENV_BASE='daemon-base', PA_ENV_PRECEDENCE='daemon', RLM_DEPTH='3',
        PRIME_AGENT_STUCK_CELL_GRACE_MS='300', PRIME_AGENT_TEST_BLOCKING_CELL_SEAM='1')
    provider = Provider()
    thread = threading.Thread(target=provider.serve_forever, daemon=True)
    thread.start()
    models_path = root / 'agent/models.json'
    models = json.loads(models_path.read_text())
    models['providers']['parity-local']['baseUrl'] = 'http://127.0.0.1:%s/v1' % provider.server_port
    models_path.write_text(json.dumps(models))
    settings = root / 'agent/settings.json'
    settings.write_text(json.dumps({'prewarmPythonKernel': False, 'onboardingCompleted': True,
        'telemetry': {'noticeShown': True}, 'defaultProvider': 'parity-local', 'defaultModel': 'parity-1'}))
    log = open(root / 'daemon.log', 'wb')
    process = subprocess.Popen([str(binary), '--mode', 'daemon', '--daemon-socket', str(root / 'd.sock')],
        cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    client = None
    probes = observed['probes'] = {}
    session_info = {}
    tracked = set()
    secret = 'fixture-launch-secret'
    prepared_transaction = False
    primary_error = None
    config = {'cwd': str(root), 'agentDir': str(root / 'agent'), 'provider': 'parity-local',
        'model': 'parity-1', 'noExtensions': True, 'noContextFiles': True, 'telemetryDisabled': True}
    launch = {'PA_ENV_MARKER': secret, 'PA_ENV_PRECEDENCE': 'client', 'RLM_DEPTH': '9',
              'PRIME_AGENT_INTERNAL_PARITY_CANARY': 'must-not-inherit'}
    def create(name, lifecycle, forwarded):
        row = client.call('create', name=name, lifecycle=lifecycle, config=config, **forwarded)
        active = row.get('activeSessionId') or row.get('id')
        assert active
        header = json.loads(transcript_text(root / 'agent', row['sessionFile']).splitlines()[0])
        assert header.get('rlmDepth', 0) == 0, header
        session_info[name] = {'sessionId': header['id'], 'sessionFile': row['sessionFile']}
        assert row['sessionId'] == header['id'], 'create response/file identity mismatch'
        client.call('attach', activeSessionId=active)
        _, descriptor = worker_descriptor(root / 'agent', active, root / 'd.sock')
        tracked.add(verify_session(descriptor, active, binary, root / 'd.sock', process.pid))
        return active
    def probe(active, phase, expected_marker, expected_precedence):
        provider.phase = phase
        # A submit as the TUI sends it: after a crash it resumes the queue the
        # crash's abort parked, which a rebuilt session keeps parked.
        client.call('prompt_and_wait', activeSessionId=active, message='Probe ' + phase,
            streamingBehavior='followUp')
        result = current_probe(provider.requests[phase], phase)
        expected = {'env': {'PA_ENV_MARKER': expected_marker, 'PA_ENV_BASE': 'daemon-base',
            'PA_ENV_PRECEDENCE': expected_precedence, 'PRIME_AGENT_INTERNAL_PARITY_CANARY': None},
            'child_marker': expected_marker or 'ABSENT'}
        assert result == expected, (phase, result, expected)
        probes[phase] = result
    try:
        client = LaunchClient(root / 'd.sock', process)
        client.process = process
        a = create('launch-a', 'resident', {'launchEnv': launch})
        b = create('launch-b', 'resident', {})
        probe(a, 'resident_initial', secret, 'client')
        probe(b, 'isolated_other', None, 'daemon')
        observed['resident_recovery'] = {}
        a = restart(client, provider, 'resident_crash', a, root / 'agent', binary, config, False, observed['resident_recovery'], tracked, session_info['launch-a'])
        probe(a, 'resident_recovered', None, 'daemon')
        owner = create('launch-owned', 'client_owned', {'launchEnv': launch})
        probe(owner, 'owned_initial', secret, 'client')
        observed['owned_recovery'] = {}
        owner = restart(client, provider, 'owned_crash', owner, root / 'agent', binary, config, True, observed['owned_recovery'], tracked, session_info['launch-owned'])
        probe(owner, 'owned_recovered', secret, 'client')
        client.call('promote_owned_session', activeSessionId=owner)
        observed['promoted_recovery'] = {}
        owner = restart(client, provider, 'promoted_crash', owner, root / 'agent', binary, config, False, observed['promoted_recovery'], tracked, session_info['launch-owned'])
        probe(owner, 'promoted_recovered', None, 'daemon')
        descriptors = list((root / 'agent/daemon-workers').rglob('*.json'))
        assert len(descriptors) >= 3, 'durable descriptor audit would be empty'
        for path in descriptors:
            assert secret not in path.read_text(), 'launch secret leaked into durable worker metadata'
        observed['durable_launch_secret_absent'] = True
        prepared = client.call('prepare_update_restart', updateId='launch-env-parity')
        prepared_transaction = True
        observed['prepare_update'] = prepared
        assert prepared.get('state') == 'prepared', 'update did not prepare'
        rosters = list((root / 'agent/update-restarts').glob('*/prepared/launch-env-parity/roster.json'))
        assert len(rosters) == 1, 'actual prepared roster missing/ambiguous'
        roster = json.loads(rosters[0].read_text())
        assert len(roster['workers']) == 3, 'prepared roster omitted fixture workers'
        assert secret not in json.dumps(roster), 'launch secret leaked into actual update roster'
        observed['update_roster'] = roster
    except BaseException as error:
        primary_error = error
        observed['primary_error'] = str(error)
        raise
    finally:
        observed['requests'] = provider.requests
        cleanup_error = None
        try:
            if client:
                observed['wire'] = client.wire
                if process.poll() is None:
                    if prepared_transaction:
                        finish_daemon(client, process, 'commit_update_restart', updateId='launch-env-parity')
                    else:
                        finish_daemon(client, process, 'shutdown', force=True)
                assert process.returncode == 0, 'supervisor did not shut down successfully'
                wait_gone(tracked)
                observed['cleanup_verified'] = True
            else:
                raise AssertionError('fixture supervisor did not establish cleanup control')
        except Exception as error:
            cleanup_error = error
            observed['cleanup_error'] = str(error)
        finally:
            if client:
                client.close()
            stop(process)
            log.close()
            observed['daemon_log'] = (root / 'daemon.log').read_text(errors='replace')[-6000:]
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=2)
        if cleanup_error and primary_error is None:
            raise AssertionError('fixture cleanup failed: ' + str(cleanup_error)) from cleanup_error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--receipt', type=Path, required=True)
    args = parser.parse_args()
    receipt = {'verified': False, 'scope': 'Raw daemon create, the env of the kernel commands and their children, '
        'env clearing/retention, internal-prefixed launchEnv re-filtering and durable launch-env secret exclusion.',
        'not_verified': ['TUI/ACP sender env collection', 'refreshed attach launchEnv recovery',
                         'rlm-spawn and minimal depth-0 root forwarding']}
    try:
        assert sys.platform == 'linux', 'requires Linux /proc ownership checks'
        with tempfile.TemporaryDirectory(prefix='launch-env-check-', dir='/tmp') as folder:
            binary = Path(os.environ['PA_RUST_BINARY']).resolve()
            run = receipt['run'] = {'binary_sha256': digest(binary)}
            try:
                run_case(binary, Path(folder) / 'rust', run)
            except Exception as error:
                run['error'] = str(error)
        assert 'error' not in run, run
        receipt['verified'] = True
    except Exception as error:
        receipt['error'] = str(error)
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    args.receipt.write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'verified': receipt['verified'], 'receipt': str(args.receipt), 'error': receipt.get('error')}))
    return 0 if receipt['verified'] else 1


if __name__ == '__main__':
    sys.exit(main())
