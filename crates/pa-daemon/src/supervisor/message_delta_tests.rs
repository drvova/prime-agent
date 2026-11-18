//! The client-hop `message_update` encoding: a client whose attach advertised
//! `message_update_delta` reads bounded delta frames that restore to exactly
//! the full snapshots every other client still reads.

use super::handshake_tests::{
    accept_fake_worker, answer_supervisor_frame, bind_fake_worker, read_supervisor_frame,
    FakeWorkerSocket,
};
use super::*;
use pa_types::daemon::message_delta::{MessageDeltaDecoder, MESSAGE_UPDATE_DELTA_CAPABILITY};
use std::fmt::Write as _;

const SESSION: &str = "w-delta";

/// A client connection served by `handle_client`, past its hello.
struct TestClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl TestClient {
    async fn connect(supervisor: &Arc<Supervisor>) -> Self {
        let (server_side, client_side) = tokio::net::UnixStream::pair().expect("socket pair");
        let supervisor = Arc::clone(supervisor);
        let stream: Box<dyn TransportStream> = Box::new(server_side);
        tokio::spawn(async move { supervisor.handle_client(stream).await });
        let (read_half, writer) = client_side.into_split();
        let mut client = TestClient {
            reader: BufReader::new(read_half),
            writer,
        };
        assert!(client.read_line().await.contains("daemon_hello"));
        client
    }

    async fn read_line(&mut self) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
            .await
            .expect("a line in time")
            .expect("the connection stays open");
        line
    }

    /// Attach with `capabilities`; the fake worker answers the routed attach.
    async fn attach(&mut self, fake: &mut FakeWorkerSocket, id: &str, capabilities: &[&str]) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "clientId": id,
            "command": {
                "type": "attach",
                "id": id,
                "activeSessionId": SESSION,
                "capabilities": capabilities,
            },
        });
        self.writer
            .write_all((envelope.to_string() + "\n").as_bytes())
            .await
            .expect("send the attach");
        let frame = tokio::time::timeout(Duration::from_secs(5), read_supervisor_frame(fake))
            .await
            .expect("the attach reaches the worker");
        assert_eq!(frame.header.get("commandType"), Some(&json!("attach")));
        let request_id = frame.header["requestId"].as_str().expect("request id");
        answer_supervisor_frame(fake, request_id, "attach").await;
        let response = self.read_line().await;
        assert!(response.contains("\"success\":true"), "attach: {response}");
    }
}

async fn send_session_event(fake: &mut FakeWorkerSocket, event: &Value) {
    let payload = json!({
        "type": "session_event",
        "activeSessionId": SESSION,
        "event": event,
    });
    write_frame(
        &mut fake.write_half,
        &json!({ "kind": "outbound", "outboundType": "session_event", "activeSessionId": SESSION }),
        &serde_json::to_vec(&payload).unwrap(),
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .await
    .expect("write the session event");
}

#[tokio::test]
async fn a_delta_client_reads_bounded_frames_and_a_plain_client_reads_full_snapshots() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = dir.path().join("worker.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.path().join("daemon.sock"),
            agent_dir,
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
        "version": 2,
        "workerId": SESSION,
        "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "delta-token",
        "rootActiveSessionId": SESSION,
        "createdAt": "2026-09-23T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(SESSION.to_string(), descriptor, dir.path().join("w.json"));
    supervisor.registry.insert(Arc::clone(&resident)).await;
    let listener = bind_fake_worker(&socket_path).await;
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    let auth = read_supervisor_frame(&mut fake).await;
    let request_id = auth.header["requestId"].as_str().expect("request id");
    answer_supervisor_frame(&mut fake, request_id, "worker_auth").await;
    connect.await.expect("connect task").expect("handshake");
    // The worker's session exists: routed client commands may reach it.
    resident.transition(crate::registry::WorkerTransition::SessionReady);

    let mut delta_client = TestClient::connect(&supervisor).await;
    delta_client
        .attach(
            &mut fake,
            "delta-client",
            &["attach_snapshot", MESSAGE_UPDATE_DELTA_CAPABILITY],
        )
        .await;
    let mut plain_client = TestClient::connect(&supervisor).await;
    plain_client
        .attach(&mut fake, "plain-client", &["attach_snapshot"])
        .await;

    // One streamed reply: start, 40 growing updates (~40 KB at the end), end.
    let message =
        |text: &str| json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] });
    let mut events = vec![json!({ "type": "message_start", "message": message("") })];
    let mut text = String::new();
    for step in 0..40 {
        let padding = "x".repeat(1000);
        write!(text, "{step:03}{padding}").unwrap();
        events.push(json!({ "type": "message_update", "message": message(&text) }));
    }
    events.push(json!({ "type": "message_end", "message": message(&text) }));
    for event in &events {
        send_session_event(&mut fake, event).await;
    }

    let mut decoder = MessageDeltaDecoder::new();
    for (index, event) in events.iter().enumerate() {
        let delta_line = delta_client.read_line().await;
        let plain_line = plain_client.read_line().await;
        let plain: Value = serde_json::from_str(&plain_line).unwrap();
        // The plain client reads exactly today's full frame.
        assert_eq!(&plain["event"], event, "frame {index}");
        if event["type"] == "message_update" && index > 1 {
            assert!(
                delta_line.len() < 1_500,
                "update {index} carried its prefix: {} bytes",
                delta_line.len()
            );
        }
        let mut restored: Value = serde_json::from_str(&delta_line).unwrap();
        decoder.decode(&mut restored);
        assert_eq!(restored, plain, "frame {index} restores to the full frame");
    }
}
