//! The daemon-wide budget: two workers share one budget through the supervisor, a hanging
//! supervisor falls back to the worker's local pool, and the learned state survives a
//! supervisor restart.

use std::sync::Arc;
use std::time::Duration;

use pa_ai::admission::{AdmissionKey, AdmissionRequest, AdmissionSource, Priority};
use pa_types::control::{AdjustReason, LearnedState, LimitReport, Observation, Outcome};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::protocol::{response_line, response_success};

fn key(provider: &str) -> AdmissionKey {
    AdmissionKey {
        provider: provider.to_string(),
        credential: "0123456789ab".to_string(),
        scope: "model-1".to_string(),
    }
}

fn request(provider: &str) -> AdmissionRequest {
    AdmissionRequest {
        key: key(provider),
        priority: Priority::Foreground,
        enqueued_at_ms: pa_ai::admission::now_ms(),
    }
}

fn success() -> Observation {
    Observation {
        outcome: Outcome::Success,
        latency_ms: None,
        limits: LimitReport::default(),
    }
}

/// A stand-in supervisor socket that answers admission commands from `service`, one command
/// per connection like the supervisor link.
fn serve_admission(service: Arc<AdmissionService>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("sup.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                writer
                    .write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n")
                    .await
                    .unwrap();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let envelope: Value = serde_json::from_str(&line).unwrap();
                let command: pa_types::daemon::DaemonCommand =
                    serde_json::from_value(envelope["command"].clone()).unwrap();
                let data = handle_admission_command(&service, &command).await.unwrap();
                let response = response_line(&response_success(
                    envelope["id"].as_str(),
                    "provider_admission",
                    Some(data),
                ));
                let mut out = serde_json::to_string(&response).unwrap();
                out.push('\n');
                let _ = writer.write_all(out.as_bytes()).await;
            });
        }
    });
    (dir, socket)
}

/// Two workers, one budget: with the shared concurrency at one, the second worker's request
/// waits in the supervisor until the first worker's request is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_workers_share_one_budget_through_the_supervisor() {
    let agent_dir = tempfile::TempDir::new().unwrap();
    let service = AdmissionService::new(agent_dir.path());
    service.ensure_loaded().await;
    service.pool.seed([(
        key("shared"),
        LearnedState {
            concurrency: 1,
            rate_per_min: 6_000,
            concurrency_ceiling: Some(1),
            rate_ceiling: None,
            ceiling_set_at_ms: Some(pa_ai::admission::now_ms()),
            reason: AdjustReason::Throttled,
            updated_at_ms: 0,
        },
    )]);
    let (_socket_dir, socket) = serve_admission(Arc::clone(&service));
    let worker = |token: &str| {
        SupervisorAdmission::new(
            Arc::new(SupervisorLink::new(socket.clone())),
            token.to_string(),
        )
    };
    let (first, second) = (worker("worker-a"), worker("worker-b"));
    let mut order = Vec::new();
    let held = first.acquire(request("shared")).await;
    order.push("first granted");
    let mut waiting = tokio::spawn(second.acquire(request("shared")));
    let early =
        if let Ok(grant) = tokio::time::timeout(Duration::from_millis(500), &mut waiting).await {
            order.push("second granted while the first holds the permit");
            Some(grant.unwrap())
        } else {
            order.push("second waits while the first holds the permit");
            None
        };
    let _cut = held.finish(success());
    order.push("first released");
    let granted = if let Some(grant) = early {
        grant
    } else {
        let grant = waiting.await.unwrap();
        order.push("second granted");
        grant
    };
    let _cut = granted.finish(success());
    assert_eq!(
        order,
        vec![
            "first granted",
            "second waits while the first holds the permit",
            "first released",
            "second granted",
        ]
    );
}

/// A supervisor that accepts but never answers cannot stall a worker: after the bounded wait
/// the request takes a permit from the worker's local pool.
#[tokio::test]
async fn a_hanging_supervisor_falls_back_to_the_local_pool() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("hung.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let hung = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            // Accept and hold the connection: no hello, no answer.
            held.push(stream);
        }
    });
    let source =
        SupervisorAdmission::new(Arc::new(SupervisorLink::new(socket)), "worker".to_string());
    let started = std::time::Instant::now();
    let grant = tokio::time::timeout(
        Duration::from_secs(10),
        source.acquire(request("hung-supervisor")),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        grant.is_ok() && elapsed >= ACQUIRE_POLL + LINK_GRACE && elapsed < Duration::from_secs(5),
        "the bounded wait then the local pool (took {elapsed:?})"
    );
    hung.abort();
}

/// What the supervisor learned survives its restart: a new service over the same agent dir
/// starts from the persisted state.
#[tokio::test]
async fn persisted_pacing_is_restored_after_a_supervisor_restart() {
    let agent_dir = tempfile::TempDir::new().unwrap();
    let before = AdmissionService::new(agent_dir.path());
    let (lease, _) = before
        .acquire(&request("persisted"), Duration::from_secs(1))
        .await
        .unwrap();
    before.release(
        lease,
        Observation {
            outcome: Outcome::Throttled,
            latency_ms: None,
            limits: LimitReport::default(),
        },
    );
    before.persist().await.unwrap();
    let learned_before = before.learned();
    drop(before);

    let after = AdmissionService::new(agent_dir.path());
    after.ensure_loaded().await;
    let restored: Vec<_> = after
        .learned()
        .into_iter()
        .map(|(key, learned)| {
            (
                key,
                LearnedState {
                    reason: AdjustReason::Throttled,
                    updated_at_ms: 0,
                    ..learned
                },
            )
        })
        .collect();
    let expected: Vec<_> = learned_before
        .into_iter()
        .map(|(key, learned)| {
            (
                key,
                LearnedState {
                    updated_at_ms: 0,
                    ..learned
                },
            )
        })
        .collect();
    assert_eq!(restored, expected);
    assert_eq!(
        after.learned().first().map(|(_, learned)| learned.reason),
        Some(AdjustReason::Restored)
    );
}
