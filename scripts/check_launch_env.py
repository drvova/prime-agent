#!/usr/bin/env python3
"""Actual runtime daemon/kernel launch-env check; owned Linux CI processes only.

Exercises the raw `session.create` (wire2, not TUI sender rendering): a client's
`launchEnv` reaches the session's kernel commands and their children over the
daemon's own environment, re-filtered (no daemon-internal variable, no
`RLM_DEPTH`); another session does not see it; a crashed session rebuilt in the
same daemon keeps it (it is kept in memory for the session's lifetimes in this
process, `pa-runtime` `daemon/handlers/create.rs`); an ephemeral session
(`--no-session`, owned by its connection) runs in it, keeps it across its own
rebuild and keeps no transcript; a new root session's transcript is at depth 0
whatever `RLM_DEPTH` the daemon or the client carries;
and the secret reaches no durable state: no file of the agent dir (its database
included) and not the roster a prepared update writes (`pa_core::update::restart`).
Requests/responses and actual provider tool results are retained. No ambient
credential is copied. Sessions run inside the fixture's own daemon: recovery
crashes only a session this fixture created and its daemon holds open, through
that session's own cell, and signals no process.
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

from lifecycle_lib import (Wire2Client, digest, env_for, saved_transcripts, session_serving, stop,
                           transcript_text, verify_runtime_session, wire2_socket)
from check_restore_notice import texts

MARKER = 'LAUNCH_ENV_PARITY='
LIMIT = 35
# The probe reports the marker reversed (`reversed_marker`): the cell's output
# is kept in the transcript, which must not hold the secret itself.
CODE = """(define (reversed value)
  (if (string? value) (list->string (reverse (string->list value))) value))
(define (command-env name)
  (let ([seen (hash-ref (bash (string-append "printf '%s' \\"${" name "-__UNSET__}\\"")) 'output)])
    (if (equal? seen "__UNSET__") void seen)))
(define result
  (hash 'env (hash 'PA_ENV_MARKER (reversed (command-env "PA_ENV_MARKER"))
                   'PA_ENV_BASE (command-env "PA_ENV_BASE")
                   'PA_ENV_PRECEDENCE (command-env "PA_ENV_PRECEDENCE")
                   'PRIME_AGENT_INTERNAL_PARITY_CANARY (command-env "PRIME_AGENT_INTERNAL_PARITY_CANARY"))
        'child_marker (reversed (hash-ref (bash "sh -c 'printf %s \\"${PA_ENV_MARKER:-ABSENT}\\"'") 'output))))
(displayln (string-append "LAUNCH_ENV_PARITY=" (value->jsexpr-string result)))"""


def reversed_marker(value):
    """`value` as the probe reports it."""
    return None if value is None else value[::-1]


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


def stream_event(client, active, kind, since):
    """The first `kind` event on session `active`'s stream among the client's
    events from index `since` on."""
    topic = f'agent/{active}/stream'
    return client.wait('session event ' + kind, lambda: next(
        (event['payload'] for event in client.events[since:]
         if event['topic'] == topic and event['payload'].get('type') == kind), None), LIMIT)


def wait_gone(sessions):
    """Sessions die with the daemon that hosts them: nothing may still serve the
    socket they were reached on (a runtime session's is its daemon's)."""
    deadline = time.monotonic() + LIMIT
    while time.monotonic() < deadline:
        serving = [path for path, _session in sessions if session_serving(path)]
        if not serving:
            return
        threading.Event().wait(0.025)
    raise AssertionError('owned fixture sessions were still served after shutdown: ' + str(serving))


def restart(client, provider, phase, active, agent, binary, process, observation):
    """Crash session `active` (one this fixture's daemon holds open) through its
    own cell and wait for the daemon to rebuild it; answers its state after."""
    observation['session_row'] = verify_runtime_session(client, active, binary, process.pid)
    # End the session with no teardown of its own: its cell outlives the abort
    # (the daemon's stuck-cell grace is the fixture's), so the daemon ends the
    # session as crashed.
    # The cell enters the kernel's test seam (debug builds, asked for in the
    # daemon's environment): a native call no interrupt reaches, which writes
    # its marker once it is inside and returns when the marker is gone.
    marker = agent.parent / ('wedged-' + phase)
    provider.phase, provider.code = phase, '(%%test-block %s)' % json.dumps(str(marker))
    client.follow(active)
    since = len(client.events)
    client.call('prompt', activeSessionId=active, message='Wedge ' + phase)
    deadline = time.monotonic() + LIMIT
    while not marker.exists():
        assert time.monotonic() < deadline, 'the cell never entered its blocking call'
        threading.Event().wait(0.02)
    # The abort the TUI sends: the turn ends and the lane keeps going
    # (a plain `abort` parks the lane until `session.resume_queue`).
    client.call('abort_and_send_queued', activeSessionId=active)
    crashed = observation['crash'] = stream_event(client, active, 'session_crashed', since)
    provider.code = CODE
    marker.unlink()
    assert crashed.get('kind') == 'stuck', crashed
    # The daemon rebuilds the crashed session and tells its attached client.
    observation['recovered'] = stream_event(client, active, 'session_recovered', since)
    observation['recovered_row'] = verify_runtime_session(client, active, binary, process.pid)
    state = observation['replacement_state'] = client.call('get_state', activeSessionId=active)
    return state


def finish_daemon(client, process, kind, **payload):
    client.call(kind, **payload)  # Successful acknowledgement is mandatory.
    client.close()  # The daemon's close waits for every fixture client to end.
    process.wait(timeout=15)
    assert process.returncode == 0, 'fixture daemon did not shut down successfully'


def durable_files_with(agent, secret):
    """The agent dir's files (its database and its WAL included) holding `secret`."""
    return [str(path) for path in sorted(agent.rglob('*'))
            if path.is_file() and secret.encode() in path.read_bytes()]


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
    socket_path = root / 'd.sock'
    log = open(root / 'daemon.log', 'wb')
    process = subprocess.Popen([str(binary), '--mode', 'daemon', '--daemon-socket', str(socket_path)],
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
    def create(name, forwarded):
        row = client.call('create', name=name, config=config, **forwarded)
        active = row.get('activeSessionId')
        assert active, row
        client.call('attach', activeSessionId=active)
        verify_runtime_session(client, active, binary, process.pid)
        tracked.add((str(wire2_socket(socket_path)), active))
        if forwarded.get('noSession'):
            return active, row
        header = json.loads(transcript_text(root / 'agent', row['sessionFile']).splitlines()[0])
        assert header.get('rlmDepth', 0) == 0, header
        session_info[name] = {'sessionId': header['id'], 'sessionFile': row['sessionFile']}
        assert row['sessionId'] == header['id'], 'create response/file identity mismatch'
        return active, row
    def probe(active, phase, expected_marker, expected_precedence):
        provider.phase = phase
        # A submit as the TUI sends it while a turn may still stream.
        client.call('prompt_and_wait', activeSessionId=active, message='Probe ' + phase,
            streamingBehavior='followUp')
        result = current_probe(provider.requests[phase], phase)
        expected = {'env': {'PA_ENV_MARKER': reversed_marker(expected_marker), 'PA_ENV_BASE': 'daemon-base',
            'PA_ENV_PRECEDENCE': expected_precedence, 'PRIME_AGENT_INTERNAL_PARITY_CANARY': None},
            'child_marker': reversed_marker(expected_marker or 'ABSENT')}
        assert result == expected, (phase, result, expected)
        probes[phase] = result
    try:
        client = Wire2Client(socket_path, process)
        a, _ = create('launch-a', {'launchEnv': launch})
        b, _ = create('launch-b', {})
        probe(a, 'resident_initial', secret, 'client')
        probe(b, 'isolated_other', None, 'daemon')
        observed['resident_recovery'] = recovery = {}
        state = restart(client, provider, 'resident_crash', a, root / 'agent', binary, process, recovery)
        assert state['sessionId'] == session_info['launch-a']['sessionId'], 'recovery changed durable session identity'
        assert state['sessionFile'] == session_info['launch-a']['sessionFile'], 'recovery changed durable session file'
        recovery['recovery_policy'] = 'Rebuilt in the same daemon with the client launchEnv it keeps in memory'
        probe(a, 'resident_recovered', secret, 'client')
        # The client-owned session: ephemeral, owned by this connection, kept nowhere.
        owner, owned = create('launch-owned', {'noSession': True, 'launchEnv': launch})
        observed['owned_create'] = owned
        probe(owner, 'owned_initial', secret, 'client')
        observed['owned_recovery'] = recovery = {}
        restart(client, provider, 'owned_crash', owner, root / 'agent', binary, process, recovery)
        recovery['recovery_policy'] = 'Rebuilt in the same daemon, in memory, with the client launchEnv'
        probe(owner, 'owned_recovered', secret, 'client')
        owned_rows = [row for row in client.query('sessions.list', open=True)['sessions']
                      if row.get('session') == owner]
        observed['owned_row'] = owned_rows
        assert len(owned_rows) == 1 and owned_rows[0].get('open') is True, 'the ephemeral session is not open'
        stored = [str(locator) for locator in saved_transcripts(root / 'agent', binary)
                  if owner in str(locator)]
        assert not stored, 'the ephemeral session left a transcript: ' + str(stored)
        prepared = client.call('prepare_update_restart', updateId='launch-env-parity')
        prepared_transaction = True
        observed['prepare_update'] = prepared
        assert prepared.get('state') == 'prepared', 'update did not prepare'
        rosters = list((root / 'agent').rglob('prepared/launch-env-parity/roster.json'))
        assert len(rosters) == 1, 'actual prepared roster missing/ambiguous'
        roster = observed['update_roster'] = json.loads(rosters[0].read_text())
        rostered = sorted(row['active_session_id'] for row in roster.get('sessions', []))
        # The kept sessions; the ephemeral one has nothing to restore.
        assert rostered == sorted([a, b]), ('prepared roster is not the fixture\'s kept sessions', rostered)
        assert secret not in json.dumps(roster), 'launch secret leaked into actual update roster'
        leaked = durable_files_with(root / 'agent', secret)
        assert not leaked, 'launch secret leaked into durable state: ' + str(leaked)
        observed['durable_launch_secret_absent'] = True
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
                assert process.returncode == 0, 'fixture daemon did not shut down successfully'
                wait_gone(tracked)
                observed['cleanup_verified'] = True
            else:
                raise AssertionError('fixture daemon did not establish cleanup control')
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
    receipt = {'verified': False, 'scope': 'Raw runtime daemon session.create, the env of the kernel commands and '
        'their children, env isolation/retention across a crash rebuild, an ephemeral owned session, '
        'internal-prefixed launchEnv re-filtering and durable launch-env secret exclusion.',
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
