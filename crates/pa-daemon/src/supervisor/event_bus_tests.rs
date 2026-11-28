//! The event bus through the supervisor, between two connected workers: a
//! publisher defines a source and publishes, and the subscriber's worker
//! receives the event as an agent message from `event:<source>`.

use super::handshake_tests::{accept_fake_worker, bind_fake_worker, FakeWorkerSocket};
use super::*;
use pa_types::events::{EventBusRequest, EventDeliveryMode};

/// Serve one fake worker: answer the handshake, `get_state` with `summary`, and every
/// `worker_deliver_message` as delivered, sending its payload to `delivered`.
async fn serve(fake: FakeWorkerSocket, summary: Value, delivered: mpsc::UnboundedSender<Value>) {
    let FakeWorkerSocket {
        listener: _listener,
        mut read_half,
        mut write_half,
    } = fake;
    let mut reader = PrivateFrameReader::new(&mut read_half, DEFAULT_PRIVATE_FRAME_LIMITS);
    while let Ok(Some(frame)) = reader.read_frame().await {
        let command = frame
            .header
            .get("commandType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let request_id = frame
            .header
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = match command.as_str() {
            "worker_auth" => json!({ "capabilities": ["agent_roster"] }),
            "get_state" => summary.clone(),
            "worker_deliver_message" => {
                let payload: Value = serde_json::from_slice(&frame.payload).expect("a command");
                delivered.send(payload).expect("the test listens");
                json!({ "deliveryStatus": "delivered" })
            }
            _ => json!({}),
        };
        let response = crate::protocol::response_line_bytes(&DaemonResponse {
            id: None,
            command,
            success: true,
            data: Some(data),
            error: None,
            error_info: None,
        });
        write_frame(
            &mut write_half,
            &json!({ "kind": "outbound", "requestId": request_id, "outboundType": "response" }),
            &response,
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .await
        .expect("answer the supervisor");
    }
}

/// Register and connect one fake worker for session `session_id`.
async fn connect(
    supervisor: &Arc<Supervisor>,
    dir: &Path,
    session_id: &str,
    delivered: mpsc::UnboundedSender<Value>,
) {
    let socket_path = dir.join(format!("{session_id}.sock"));
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
        "version": 2, "workerId": format!("w-{session_id}"), "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl", "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": format!("{session_id}-token"),
        "rootActiveSessionId": format!("active-{session_id}"),
        "sessionFile": format!("/sessions/{session_id}.jsonl"),
        "createdAt": "2026-11-24T00:00:00Z", "updatedAt": "2026-11-24T00:00:00Z",
        "lifecycle": "ready", "createCommand": {}, "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(
        format!("w-{session_id}"),
        descriptor,
        dir.join(format!("w-{session_id}.json")),
    );
    supervisor.registry.insert(Arc::clone(&resident)).await;
    let listener = bind_fake_worker(&socket_path).await;
    let connecting = {
        let supervisor = Arc::clone(supervisor);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let fake = accept_fake_worker(listener).await;
    tokio::spawn(serve(
        fake,
        json!({
            "activeSessionId": format!("active-{session_id}"),
            "sessionId": session_id,
            "sessionName": session_id,
            "cwd": "/work",
        }),
        delivered,
    ));
    connecting
        .await
        .expect("the connect task lives")
        .expect("the handshake completes");
}

async fn request(
    supervisor: &Arc<Supervisor>,
    session_id: &str,
    request: EventBusRequest,
) -> Value {
    let command = DaemonCommand::EventBus {
        id: Some("e1".to_string()),
        worker_token: format!("{session_id}-token"),
        request,
        rest: Map::new(),
    };
    let response = supervisor
        .handle_event_bus(&command, "e1", "event_bus")
        .await;
    assert!(response.success, "{response:?}");
    response.data.unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_published_event_reaches_the_subscribers_worker_as_an_agent_message() {
    // Short `/tmp` root: the socket paths must fit `SUN_LEN` everywhere.
    let tmp = tempfile::Builder::new()
        .prefix("pa-events-")
        .tempdir_in("/tmp")
        .unwrap();
    let dir = tmp.path();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir: dir.join("agent"),
        })
        .expect("supervisor"),
    );
    let (publisher_tx, mut publisher_rx) = mpsc::unbounded_channel();
    let (subscriber_tx, mut subscriber_rx) = mpsc::unbounded_channel();
    connect(&supervisor, dir, "pub", publisher_tx).await;
    connect(&supervisor, dir, "sub", subscriber_tx).await;

    request(
        &supervisor,
        "pub",
        EventBusRequest::DefineSource {
            name: "deploy.finished".to_string(),
            description: String::new(),
            schema: json!({ "type": "object", "properties": { "env": { "type": "string" } } }),
            timer: None,
            watch: None,
            webhook: None,
        },
    )
    .await;
    let subscription = request(
        &supervisor,
        "sub",
        EventBusRequest::Subscribe {
            source: "deploy.finished".to_string(),
            filter: Map::new(),
            prompt: "check the deploy".to_string(),
            delivery_mode: EventDeliveryMode::Steer,
            label: None,
        },
    )
    .await;
    let published = request(
        &supervisor,
        "pub",
        EventBusRequest::Publish {
            source: "deploy.finished".to_string(),
            payload: json!({ "env": "prod" }),
        },
    )
    .await;

    let delivered = tokio::time::timeout(Duration::from_secs(10), subscriber_rx.recv())
        .await
        .expect("the event is routed")
        .expect("one delivery");
    assert_eq!(
        (published["matched"].clone(), delivered),
        (
            json!(1),
            json!({
                "type": "worker_deliver_message",
                "targetActiveSessionId": "w-sub",
                "message": format!(
                    "Event `deploy.finished` {}, published by pub, for your subscription {}.\n\n\
                     check the deploy\n\nPayload:\n{{\"env\":\"prod\"}}",
                    published["eventId"].as_str().unwrap(),
                    subscription["id"].as_str().unwrap()
                ),
                "sender": { "sessionId": "pub", "sessionName": "event:deploy.finished", "runtimeKind": "event" },
                "deliveryMode": "steer",
                "messageKind": "broadcast",
            })
        )
    );
    assert!(
        publisher_rx.try_recv().is_err(),
        "the publisher receives nothing"
    );
}
