//! The headless mocks' wire2 bridge greets a client as the runtime daemon does: its `welcome` is
//! the first frame the client reads, whenever the scenario writes its `daemon_hello`. The TUI's
//! handshake refuses a reply that comes before the welcome, and a refused first connection used
//! up the one connection a scenario serves, so its retries found the socket closed.
#![cfg(unix)]

mod support;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use serde_json::{json, Value};
use support::{write_json, UnixListener};

/// Read the next whole frame's header off `stream`.
fn next_frame(stream: &mut UnixStream, decoder: &mut PrivateFrameDecoder) -> Value {
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .expect("a frame within the read bound");
        assert!(read > 0, "the bridge closed the client");
        if let Some(frame) = decoder
            .push(&chunk[..read])
            .expect("a well-formed frame")
            .into_iter()
            .next()
        {
            return frame.header;
        }
    }
}

/// A client whose hello and identity query reach the bridge before the scenario's
/// `daemon_hello` reads the welcome first, then the identity once the scenario's hello is in.
#[test]
fn the_welcome_comes_first_whenever_the_scenario_says_hello() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("handshake.sock");
    let listener = UnixListener::bind(&socket).expect("bind the mock socket");
    let mut client = UnixStream::connect(&socket).expect("connect the client");
    // The bound on a missing frame: a bridge that holds the welcome back fails the read.
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("client read bound");
    let (mut scenario, _) = listener.accept().expect("accept the client");
    for frame in [
        json!({ "type": "hello", "version": pa_types::wire2::WIRE2_VERSION, "client": "test" }),
        json!({ "type": "query", "id": "compat:identity", "name": "daemon.identity", "args": {} }),
    ] {
        let bytes = encode_private_frame(&frame, &[], DEFAULT_PRIVATE_FRAME_LIMITS)
            .expect("encode a client frame");
        client.write_all(&bytes).expect("write a client frame");
    }
    let mut decoder = PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS);
    let welcome = next_frame(&mut client, &mut decoder);
    write_json(
        &mut scenario,
        &json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
            "clientId": "mock",
        }),
    );
    let identity = next_frame(&mut client, &mut decoder);
    assert_eq!(
        [welcome, identity],
        [
            json!({
                "type": "welcome",
                "version": pa_types::wire2::WIRE2_VERSION,
                "runtimeVersion": "0.0.0",
                "lastSeq": 0,
            }),
            json!({
                "type": "reply",
                "id": "compat:identity",
                "status": "ok",
                "result": { "serverCapabilities": [], "clientId": "mock" },
            }),
        ]
    );
}
