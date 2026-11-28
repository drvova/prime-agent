#!/usr/bin/env python3
"""Mock product modes: daemon shutdown waits for its held UI connection.

Only used by the Rust harness regression; never loads or launches the product.
The daemon mode speaks the runtime daemon's wire2 on `daemon2.sock` beside the
run's socket: private frames (two big-endian u32 lengths, a JSON header, a
payload), a `hello` answered with a `welcome`, and the `daemon.shutdown` call.
"""
import json
from pathlib import Path
import select
import socket
import struct
import sys
import time

path = Path(sys.argv[sys.argv.index('--daemon-socket') + 1]).with_name('daemon2.sock')


def frame(header):
    body = json.dumps(header).encode()
    return struct.pack('>II', len(body), 0) + body


def frames(buffer):
    """The complete frames' headers in `buffer`, and the bytes left over."""
    headers = []
    while len(buffer) >= 8:
        header_len, payload_len = struct.unpack('>II', buffer[:8])
        end = 8 + header_len + payload_len
        if len(buffer) < end:
            break
        headers.append(json.loads(buffer[8:8 + header_len]))
        buffer = buffer[end:]
    return headers, buffer


if '--mode' not in sys.argv:
    with socket.socket(socket.AF_UNIX) as peer:
        peer.connect(str(path))
        peer.sendall(frame({'type': 'hello', 'version': 1, 'client': 'mock-ui'}))
        peer.recv(4096)
        sys.stdout.write('▗▄▄█▀ mock paint ?/1.0M\n')
        sys.stdout.flush()
        # The mock UI remains connected until its owned process is stopped.
        while peer.recv(4096):
            pass
    sys.exit(0)

listener = socket.socket(socket.AF_UNIX)
listener.bind(str(path))
listener.listen()
peers = {}
accepting = True
shutdown = False
deadline = time.monotonic() + 20
try:
    while accepting or peers:
        assert time.monotonic() < deadline, 'mock daemon connection drain timed out'
        ready, _, _ = select.select(([listener] if accepting else []) + list(peers), [], [], 1)
        for peer in ready:
            if peer is listener:
                client, _ = listener.accept()
                peers[client] = b''
                continue
            try:
                data = peer.recv(4096)
            except ConnectionResetError:
                data = b''
            if not data:
                peer.close()
                del peers[peer]
                continue
            headers, peers[peer] = frames(peers[peer] + data)
            for header in headers:
                try:
                    if header['type'] == 'hello':
                        peer.sendall(frame({'type': 'welcome', 'version': 1,
                                            'runtimeVersion': 'mock', 'lastSeq': 0}))
                        continue
                    assert header['type'] == 'call' and header['method'] == 'daemon.shutdown', header
                    peer.sendall(frame({'type': 'reply', 'id': header['id'],
                                        'status': 'ok', 'result': {}}))
                except (BrokenPipeError, ConnectionResetError):
                    continue
                listener.close()
                accepting = False
                shutdown = True
    assert shutdown, 'mock daemon never received the daemon.shutdown call'
    path.with_suffix('.drained').write_text('all UI/control connections closed\n')
finally:
    listener.close()
    for peer in peers:
        peer.close()
