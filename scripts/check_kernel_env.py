#!/usr/bin/env python3
"""Actual kernel check for the bash prefix and Serper wiring.

Only synthetic credentials and loopback provider traffic. One cell has the
kernel's real `bash` report what its command saw. Web-search credentials
resolve on the host per search, so a stored (command) key is never run to
start a session and never reaches a command's environment; only the ambient
SERPER_API_KEY is inherited, verbatim.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import shlex
import subprocess
import time
import sys
import tempfile
import threading

from check_restore_notice import texts, Server
from lifecycle_lib import Wire2Client, digest, stop, session_serving, verify_runtime_session, wire2_socket

MARKER = 'KERNEL_ENV_PARITY='
LIMIT = 35
CODE = '''(define shell (bash "printf '%s|%s' \\"${PA_PREFIX_SENTINEL:-ABSENT}\\" \\"${SERPER_API_KEY-UNSET}\\""))
(displayln (string-append "KERNEL_ENV_PARITY="
                          (value->jsexpr-string (hash 'bash_output (hash-ref shell 'output)
                                                      'bash_exit (hash-ref shell 'exit-code)))))'''
CASES = {
    'command_key_and_prefix': {'skill': True, 'ambient': None, 'prefix': 'export PA_PREFIX_SENTINEL=prefix-ran'},
    'ambient_key_wins': {'skill': True, 'ambient': ' ambient-fixture ', 'prefix': ''},
    'no_skill_no_command': {'skill': False, 'ambient': None, 'prefix': ''},
    'blank_ambient_is_inherited': {'skill': True, 'ambient': '  ', 'prefix': ''},
}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        if self.path != '/v1/chat/completions':
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests = self.server.requests[self.server.phase]
        requests.append(body)
        if len(requests) == 1:
            offered = {tool.get('function', {}).get('name') for tool in body.get('tools', [])}
            name = 'eval' if 'eval' in offered else None
            if name is None:
                self.send_error(422, 'missing offered kernel tool')
                return
            delta = {'tool_calls': [{'index': 0, 'id': 'kernel_' + self.server.phase,
                'type': 'function', 'function': {'name': name, 'arguments': json.dumps({'code': CODE})}}]}
            finish = 'tool_calls'
        else:
            delta, finish = {'content': 'Kernel environment fixture complete.'}, 'stop'
        def chunk(delta, reason=None):
            return {'id': 'kernel-env', 'object': 'chat.completion.chunk', 'created': 1,
                'model': 'env-fixture', 'choices': [{'index': 0, 'delta': delta, 'finish_reason': reason}]}
        wire = ''.join('data: ' + json.dumps(item) + '\n\n' for item in (
            chunk({'role': 'assistant', **delta}), chunk({}, finish))) + 'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(wire.encode())))
        self.end_headers()
        self.wfile.write(wire.encode())


def current_probe(requests, phase):
    assert len(requests) == 2, 'unexpected provider sequence'
    results = [message for message in requests[-1]['messages'] if message.get('role') == 'tool'
               and message.get('tool_call_id') == 'kernel_' + phase]
    assert len(results) == 1, 'missing/duplicate current tool result'
    probes = [json.loads(line[len(MARKER):]) for text in texts(results[0].get('content'))
              for line in text.splitlines() if line.startswith(MARKER)]
    assert len(probes) == 1, 'missing/duplicate current kernel probe'
    return probes[0]


def expected(case):
    ambient = 'UNSET' if case['ambient'] is None else case['ambient']
    return {'bash_output': ('prefix-ran' if case['prefix'] else 'ABSENT') + '|' + ambient,
            'bash_exit': 0}


def wait_gone(sessions):
    """Sessions die with the daemon that hosts them: nothing may still serve the
    sockets they were reached on (a runtime session's is its daemon's)."""
    deadline = time.monotonic() + LIMIT
    while time.monotonic() < deadline:
        serving = [path for path, _generation in sessions if session_serving(path)]
        if not serving:
            return
        threading.Event().wait(0.025)
    raise AssertionError('owned fixture sessions were still served after shutdown: ' + str(serving))


def finish_daemon(client, process):
    client.call('shutdown', force=True)
    client.close()
    process.wait(timeout=15)
    assert process.returncode == 0, 'fixture supervisor failed graceful shutdown'


# The runtime daemon's wire2 client; a call takes its own request budget
# (`timeout_seconds`) for a cold kernel.
KernelClient = Wire2Client


def run_case(binary, name, case, root, observation):
    for directory in ('home', 'tmp', 'work', 'agent', 'sessions'):
        (root / directory).mkdir(parents=True)
    agent, cwd = root / 'agent', root / 'work'
    env = {key: os.environ[key] for key in ('PATH', 'LANG', 'LC_ALL', 'TZ') if key in os.environ}
    env.update(HOME=str(root / 'home'), TMPDIR=str(root / 'tmp'), DO_NOT_TRACK='1', PI_OFFLINE='1',
        PI_CODING_AGENT_DIR=str(agent), PRIME_AGENT_CODING_AGENT_DIR=str(agent),
        PI_SESSION_DIR=str(root / 'sessions'), PRIME_AGENT_SESSION_DIR=str(root / 'sessions'),
        XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'), XDG_DATA_HOME=str(root / 'data'))
    if case['ambient'] is not None:
        env['SERPER_API_KEY'] = case['ambient']
    # Explicit identical custom skill avoids differences in bundled inventory.
    # Bundled websearch is disabled in every case; only this custom skill gates
    # injection, exercising the documented bundled-or-custom skill condition.
    if case['skill']:
        skill = agent / 'skills' / 'websearch' / 'SKILL.md'
        skill.parent.mkdir(parents=True)
        skill.write_text('---\nname: websearch\ndescription: Synthetic environment fixture\n---\nNo network work.\n')
    invoked = root / 'credential-command-invoked'
    command = "!printf ' resolved-fixture-key '; printf invoked > " + shlex.quote(str(invoked))
    (agent / 'auth.json').write_text(json.dumps({'serper': {'type': 'api_key', 'key': command}}))
    server = Server(root)
    server.RequestHandlerClass = Handler
    server.phase = name
    server.requests = {name: []}
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    (agent / 'models.json').write_text(json.dumps({'providers': {'fixture': {
        'api': 'openai-completions', 'apiKey': 'fixture-only',
        'baseUrl': 'http://127.0.0.1:%s/v1' % server.server_address[1],
        'models': [{'id': 'env-fixture', 'name': 'Environment fixture', 'reasoning': False,
                    'contextWindow': 128000, 'maxTokens': 4096}]}}}))
    (agent / 'settings.json').write_text(json.dumps({'defaultProvider': 'fixture',
        'defaultModel': 'env-fixture',
        'shellCommandPrefix': case['prefix'], 'bundledSkills': {'websearch': False}}))
    socket_path = root / 'd.sock'
    log = open(root / 'daemon.log', 'wb')
    process = subprocess.Popen([str(binary), '--mode', 'daemon', '--daemon-socket', str(socket_path)],
        cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    client = None
    tracked = set()
    primary_error = None
    try:
        client = KernelClient(socket_path, process)
        created = client.call('create', name='kernel-env-' + name, config={
            'cwd': str(cwd), 'agentDir': str(agent), 'sessionDir': str(root / 'sessions'),
            'provider': 'fixture', 'model': 'env-fixture', 'noExtensions': True,
            'noContextFiles': True, 'telemetryDisabled': True})
        active = created.get('activeSessionId') or created.get('id')
        assert active, 'create omitted active identity'
        # The session runs inside the fixture daemon, which `stop` reaps below.
        verify_runtime_session(client, active, binary, process.pid)
        tracked.add((str(wire2_socket(socket_path)), active))
        client.call('prompt_and_wait', timeout_seconds=90, activeSessionId=active, message='Run the kernel environment fixture cell.')
        observation['probe'] = current_probe(server.requests[name], name)
        assert observation['probe'] == expected(case), (observation['probe'], expected(case))
        assert not invoked.exists(), 'a stored credential command ran to start the session'
        observation['credential_command_ran'] = invoked.exists()
    except BaseException as error:
        primary_error = error
        observation['primary_error'] = str(error)
        raise
    finally:
        observation['requests'] = server.requests[name]
        cleanup_error = None
        try:
            assert client is not None, 'fixture did not establish cleanup control'
            observation['wire'] = client.wire
            finish_daemon(client, process)  # Close after ack, before waiting for exit.
            wait_gone(tracked)
            observation['cleanup_verified'] = True
        except Exception as error:
            cleanup_error = error
            observation['cleanup_error'] = str(error)
            observation['cleanup_verified'] = False
        finally:
            if client:
                client.close()
            stop(process)
            log.close()
            observation['daemon_log'] = (root / 'daemon.log').read_text(errors='replace')[-6000:]
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
        if cleanup_error and primary_error is None:
            raise AssertionError('fixture cleanup failed: ' + str(cleanup_error)) from cleanup_error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--receipt', type=Path, required=True)
    args = parser.parse_args()
    receipt = {'verified': False, 'scope': 'Raw daemon create/prompt_and_wait + the real kernel bash output; '
        'command-backed credential, trimming, ambient precedence, custom skill gate, empty-prefix omission',
        'not_verified': ['TUI/JSON CLI sender routing; runtime settings reload of captured env']}
    try:
        assert sys.platform == 'linux', 'requires hosted Linux binaries'
        with tempfile.TemporaryDirectory(prefix='kernel-env-check-', dir='/tmp') as folder:
            binary = Path(os.environ['PA_RUST_BINARY']).resolve()
            receipt.update(binary_sha256=digest(binary), cases={})
            for name, case in CASES.items():
                observation = receipt['cases'][name] = {}
                try:
                    run_case(binary, name, case, Path(folder) / name, observation)
                except Exception as error:
                    observation['error'] = str(error)
        assert all('error' not in case for case in receipt['cases'].values()), receipt['cases']
        receipt['verified'] = True
    except Exception as error:
        receipt['error'] = str(error)
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    args.receipt.write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'verified': receipt['verified'], 'receipt': str(args.receipt), 'error': receipt.get('error')}))
    return 0 if receipt['verified'] else 1


if __name__ == '__main__':
    sys.exit(main())
