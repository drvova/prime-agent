use serde_json::json;

use super::*;

/// The old worker's checks and reply shape: a `tail` or `kill` names its command, a `tail`
/// reads 1 to 200 lines (50 by default), and the reply drops the kernel frame's own members
/// and names the command as `id`.
#[test]
fn kernel_bash_commands_are_checked_and_answered_as_the_old_worker_did() {
    let tail = KernelBashRequest::parse(KernelBashAction::Tail, &json!({ "activityId": "a1" }));
    assert_eq!(
        (
            KernelBashRequest::parse(KernelBashAction::List, &json!({ "lines": 0 })),
            tail.clone(),
            KernelBashRequest::parse(KernelBashAction::Kill, &json!({ "activityId": "" })),
            KernelBashRequest::parse(
                KernelBashAction::Tail,
                &json!({ "activityId": "a1", "lines": 201 })
            ),
            tail.unwrap()
                .reply(json!({ "event": "bash_activity", "status": "ok",
                                        "reason": null, "id": "k1", "activityId": "a1",
                                        "tail": "hi" })),
        ),
        (
            Ok(KernelBashRequest {
                action: KernelBashAction::List,
                activity_id: None,
                lines: 50
            }),
            Ok(KernelBashRequest {
                action: KernelBashAction::Tail,
                activity_id: Some("a1".to_string()),
                lines: 50
            }),
            Err("activityId is required"),
            Err("lines must be between 1 and 200"),
            json!({ "id": "a1", "tail": "hi" }),
        )
    );
}

/// Only the unscoped graph of a session with no running kernel answers no runs; any other
/// request keeps the kernel's refusal.
#[test]
fn a_factory_graph_without_a_kernel_lists_no_runs() {
    let not_running = || {
        Err(anyhow::anyhow!(
            pa_types::daemon::KERNEL_NOT_RUNNING_MESSAGE
        ))
    };
    let graph = FactoryLaneRequest::parse(&json!({ "action": "graph" })).unwrap();
    let status = FactoryLaneRequest::parse(&json!({ "action": "status", "runId": "r1" })).unwrap();
    assert_eq!(
        (
            FactoryLaneRequest::parse(&json!({})),
            graph.settle(not_running()).unwrap(),
            status.settle(not_running()).unwrap_err().to_string(),
        ),
        (
            Err("action is required"),
            json!({ "runs": [] }),
            pa_types::daemon::KERNEL_NOT_RUNNING_MESSAGE.to_string(),
        )
    );
}
