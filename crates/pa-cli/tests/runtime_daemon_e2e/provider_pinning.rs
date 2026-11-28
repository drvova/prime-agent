//! A session whose model id two configured providers serve stays on its own
//! provider on the runtime daemon (the supervisor's `provider_pinning_e2e`):
//! a spent retry budget fails the turn there, and no request ever reaches
//! the other provider.

use serde_json::{json, Value};

use super::openai_mock::{model, Answer, OpenAiMock, LOOPBACK};
use super::turns::Attached;
use super::Bed;

#[tokio::test(flavor = "multi_thread")]
async fn a_spent_retry_budget_fails_the_turn_on_the_sessions_provider() {
    let bed = Bed::new();
    let primary = OpenAiMock::start(|_, _| Answer::Unavailable);
    let other = OpenAiMock::start(|_, _| Answer::Reply {
        content: "answered by the other provider".to_string(),
        usage: super::openai_mock::small_usage(),
    });
    std::fs::write(
        bed.agent_dir().join("models.json"),
        json!({ "providers": {
            "prime-inference": { "api": "openai-completions", "baseUrl": primary.url(),
                "apiKey": "sk-primary", "models": [model("mock-1", "Mock 1")] },
            "prime-backup": { "api": "openai-completions", "baseUrl": other.url(),
                "apiKey": "sk-backup", "models": [model("mock-1", "Mock 1 (backup)")] },
        } })
        .to_string(),
    )
    .unwrap();
    // One quick retry. The retired `retry.failover` block still loads
    // (existing settings files carry it) and changes nothing.
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "retry": {
            "enabled": true, "maxRetries": 1, "baseDelayMs": 50,
            "failover": { "enabled": true, "maxRetries": 1, "baseDelayMs": 50 },
        } })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": [] }), LOOPBACK);
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = Attached::open(&bed, &cwd).await;
    session
        .call(
            "session.set_model",
            json!({ "provider": "prime-inference", "modelId": "mock-1" }),
        )
        .await;
    let task = session.publish("hi", None).await;
    let done = session.done(&task["task"]).await;

    let stream = session.payloads("stream");
    let of_type = |kind: &str| -> Vec<&Value> {
        stream
            .iter()
            .filter(|event| event["type"] == kind)
            .collect()
    };
    let starts = of_type("auto_retry_start");
    let end = of_type("auto_retry_end").last().copied().cloned();
    let turn_end = of_type("turn_end").last().copied().cloned();
    assert_eq!(
        (
            done["outcome"].clone(),
            primary.bodies().len(),
            other.bodies().len(),
            starts.len(),
            starts
                .iter()
                .any(|start| start.get("reason").is_some() || start.get("backupModel").is_some()),
            end.map(|end| (end["success"].clone(), end["attempt"].clone())),
            turn_end.map(|turn_end| (
                turn_end["message"]["provider"].clone(),
                turn_end["message"]["stopReason"].clone()
            )),
        ),
        (
            json!("failed"),
            2,
            0,
            1,
            false,
            Some((json!(false), json!(1))),
            Some((json!("prime-inference"), json!("error"))),
        ),
        "the turn fails on the session's provider after its one retry: {stream:?}"
    );
}
