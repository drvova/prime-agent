//! The `event-*` kernel functions against a recording controller: each call
//! reaches the bus as its typed request, and a daemonless run refuses them.

use std::future::Future;
use std::sync::{Arc, Mutex};

use pa_types::events::{EventBusRequest, EventDeliveryMode};
use serde_json::{json, Map, Value};

use super::{register_event_bus_host_handlers, EventBusController, NoEventBus};
use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::HostRequestHandlers;
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner};

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
    tool_call_id: None,
};

/// Records each request and answers `{"ok": true}`.
#[derive(Default)]
struct Recording(Mutex<Vec<EventBusRequest>>);

impl EventBusController for Recording {
    fn request(
        &self,
        request: EventBusRequest,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send {
        self.0.lock().unwrap().push(request);
        std::future::ready(Ok(json!({ "ok": true })))
    }
}

/// What each cell printed, or the error it raised.
async fn run(handlers: HostRequestHandlers, cells: &[&str]) -> Vec<Result<String, String>> {
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let mut printed = Vec::new();
    for cell in cells {
        let result = kernel.execute(cell, NO_OPTIONS).await.unwrap();
        printed.push(match result.error {
            None => Ok(result.stdout),
            Some(error) => Err(error.evalue),
        });
    }
    printed
}

#[tokio::test(flavor = "multi_thread")]
async fn each_event_function_sends_its_typed_request() {
    let recording = Arc::new(Recording::default());
    let mut handlers = HostRequestHandlers::new();
    register_event_bus_host_handlers(&recording, &mut handlers);
    let printed = run(
        handlers,
        &[
            r#"(event-source-define "deploy.finished" (hash 'type "object") #:description "a deploy ended")"#,
            r#"(event-publish "deploy.finished" (hash 'env "prod"))"#,
            r#"(event-subscribe "deploy.finished" "check it" #:filter (hash 'env "prod") #:delivery-mode "steer" #:label "prod")"#,
            r#"(event-subscribe "deploy.finished" "any")"#,
            r#"(event-unsubscribe (hash 'id "sub-1"))"#,
            r#"(event-source-remove "deploy.finished")"#,
            r"(event-list)",
        ],
    )
    .await;
    assert_eq!(
        (
            printed.iter().filter(|cell| cell.is_ok()).count(),
            recording.0.lock().unwrap().clone()
        ),
        (
            7,
            vec![
                EventBusRequest::DefineSource {
                    name: "deploy.finished".to_string(),
                    description: "a deploy ended".to_string(),
                    schema: json!({ "type": "object" }),
                    timer: None,
                    watch: None,
                },
                EventBusRequest::Publish {
                    source: "deploy.finished".to_string(),
                    payload: json!({ "env": "prod" }),
                },
                EventBusRequest::Subscribe {
                    source: "deploy.finished".to_string(),
                    filter: Map::from_iter([("env".to_string(), json!("prod"))]),
                    prompt: "check it".to_string(),
                    delivery_mode: EventDeliveryMode::Steer,
                    label: Some("prod".to_string()),
                },
                EventBusRequest::Subscribe {
                    source: "deploy.finished".to_string(),
                    filter: Map::new(),
                    prompt: "any".to_string(),
                    delivery_mode: EventDeliveryMode::FollowUp,
                    label: None,
                },
                EventBusRequest::Unsubscribe {
                    subscription_id: "sub-1".to_string(),
                },
                EventBusRequest::RemoveSource {
                    name: "deploy.finished".to_string(),
                },
                EventBusRequest::List,
            ]
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_the_daemon_refuses_the_event_functions() {
    let mut handlers = HostRequestHandlers::new();
    register_event_bus_host_handlers(&Arc::new(NoEventBus), &mut handlers);
    let printed = run(handlers, &["(event-list)"]).await;
    let refused = printed[0]
        .as_ref()
        .err()
        .is_some_and(|error| error.contains("the event bus runs in the Prime Agent daemon"));
    assert!(refused, "{printed:?}");
}
