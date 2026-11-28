//! The model-facing host API of the Scheme kernel: thin Scheme wrappers over
//! the session's typed host requests (`rlm-spawn`, `agent-send`,
//! `todo-set`, `ask`, ...), loaded into every session engine after the host
//! bridge.
//!
//! Each wrapper builds the payload its Rust handler parses, sends it through
//! `(%host-request type payload)`, and shapes the reply. Optional arguments
//! are Steel keyword arguments (`(rlm-spawn "task" #:name "worker")`); a
//! keyword a function does not declare is refused before the function runs
//! (`contracts.rs`). Whole numbers in a reply come back as exact integers,
//! and an integer argument given as `8.0` is sent as `8`.
//!
//! A changed todo list, a settled question, a sent agent message, and a
//! transcript read also show on the running cell's result card, recorded
//! through `(%cell ...)` as the Python kernel's display events were.

use anyhow::anyhow;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// The API's Scheme sources in load order: a later file calls the earlier
/// ones, and Steel resolves names when a file is loaded.
const SOURCES: [(&str, &str); 10] = [
    ("bridge", include_str!("host_api/bridge.scm")),
    ("rlm", include_str!("host_api/rlm.scm")),
    ("agents", include_str!("host_api/agents.scm")),
    ("session", include_str!("host_api/session.scm")),
    ("interaction", include_str!("host_api/interaction.scm")),
    ("web", include_str!("host_api/web.scm")),
    ("worktree", include_str!("host_api/worktree.scm")),
    ("harness", include_str!("host_api/harness.scm")),
    ("hypothesis", include_str!("host_api/hypothesis.scm")),
    ("events", include_str!("host_api/events.scm")),
];

/// Define the host API in a session's `engine`, whose `%host-request` bridge
/// is already loaded.
pub(super) fn load(engine: &mut Engine) -> anyhow::Result<()> {
    // Names an open question, so an interrupt can withdraw exactly it.
    engine.register_fn("%fresh-id", || uuid::Uuid::new_v4().simple().to_string());
    // The UTC time `seconds` from now, as a scheduled job's `'at` sends a delay.
    engine.register_fn("%utc-after", |seconds: f64| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        crate::session::manager::format_iso(now.as_millis() as i64 + (seconds * 1000.0) as i64)
    });
    // Scheme's own `error` writes a message it builds as data, escaping its
    // quotes; this raises a built message as it is.
    engine.register_value(
        "%raise",
        steel::rvals::SteelVal::FuncV(|arguments| match arguments {
            [steel::rvals::SteelVal::StringV(reason)] => Err(steel::rerrs::SteelErr::new(
                steel::rerrs::ErrorKind::Generic,
                reason.to_string(),
            )),
            _ => Err(steel::rerrs::SteelErr::new(
                steel::rerrs::ErrorKind::ArityMismatch,
                "%raise takes one string".to_string(),
            )),
        }),
    );
    for (name, source) in SOURCES {
        super::contracts::load(engine, source)
            .map_err(|error| anyhow!("the Scheme host API ({name}) failed to load: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};

    use super::*;
    use crate::interaction::InteractionHub;
    use crate::kernel::scheme::SchemeKernelProvisioner;
    use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
    use crate::kernel::spill::host::register_ctx_handlers;
    use crate::kernel::spill::{SpillCopy, SpillStore};
    use crate::session::manager::SessionManager;
    use crate::session_engine::rlm_host::{RlmCreateSessionHandle, RlmSpawnHandle};
    use crate::tools::kernel_cell::{KernelExecuteOptions, KernelExecutor, KernelProvisioner};

    const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
        signal: None,
        timeout_ms: None,
        on_stream: None,
        on_late_sent_agent_message: None,
        tool_call_id: None,
    };

    /// Handlers answering each request type with its canned reply, and the
    /// payloads they receive, in order.
    fn answering(replies: &[(&str, Value)]) -> (HostRequestHandlers, Arc<Mutex<Vec<Value>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        for (request_type, reply) in replies {
            let (received, reply) = (Arc::clone(&received), reply.clone());
            handlers.register(
                *request_type,
                host_handler(move |payload: HostRequestPayload| {
                    received.lock().unwrap().push(payload.data);
                    let reply = reply.clone();
                    async move { Ok(reply) }
                }),
            );
        }
        (handlers, received)
    }

    async fn kernel_over(handlers: HostRequestHandlers) -> Box<dyn KernelExecutor> {
        SchemeKernelProvisioner::new(handlers, "/")
            .ensure(None, None)
            .await
            .unwrap()
    }

    /// The value of each expression as JSON, or the error it raised.
    async fn values(
        kernel: &dyn KernelExecutor,
        expressions: &[&str],
    ) -> Vec<Result<Value, String>> {
        let mut values = Vec::new();
        for expression in expressions {
            let cell = format!("(display (value->jsexpr-string {expression}))");
            let result = kernel.execute(&cell, NO_OPTIONS).await.unwrap();
            values.push(match result.error {
                None => Ok(serde_json::from_str(&result.stdout).unwrap()),
                Some(error) => Err(error.evalue),
            });
        }
        values
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn children_are_spawned_collected_and_removed_through_the_rlm_requests() {
        let spawned = serde_json::to_value(RlmSpawnHandle {
            rlm_child_id: "c1".to_string(),
            name: "worker".to_string(),
            session_dir: "/sessions/c1".to_string(),
            model: "p/m".to_string(),
            queued_ms: 0,
        })
        .unwrap();
        let created = serde_json::to_value(RlmCreateSessionHandle {
            active_session_id: "a1".to_string(),
            session_id: "s1".to_string(),
            name: "ci".to_string(),
            session_file: "/sessions/s1.jsonl".to_string(),
            model: "p/m".to_string(),
        })
        .unwrap();
        let model = json!({"provider": "p", "id": "m", "name": "M", "selector": "p/m", "image_input": true});
        let row = json!({
            "rlm_child_id": "c1",
            "active_session_id": null,
            "session_id": "s1",
            "session_name": "worker",
            "session_dir": "/sessions/c1",
            "status": "running",
            "tool_use_count": 3,
        });
        let result =
            json!({"rlm_child_id": "c1", "status": "done", "settled": true, "duration_ms": 1200});
        let aborted = json!({"subagent": row, "outcome": "idle"});
        let throttled = json!({"accepted": false, "retry_after_ms": 4000});
        let reloaded = json!({
            "skillsAdded": ["lint"],
            "skills": 4,
        });
        let (handlers, received) = answering(&[
            (
                "better.run",
                json!({"rewrite": "the checked brief", "unchanged": false}),
            ),
            ("rlm.run", spawned.clone()),
            ("rlm.create_session", created.clone()),
            ("rlm.find_models", json!({"models": [model]})),
            ("rlm.list_subagents", json!({"subagents": [row]})),
            ("rlm.collect", json!({"results": [result]})),
            ("rlm.delete_subagent", json!({"subagent": row})),
            ("rlm.abort", aborted.clone()),
            ("rlm.rename", json!({"name": "renamed"})),
            ("rlm.progress.note", throttled.clone()),
            ("rlm.reload", reloaded.clone()),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(rlm-spawn "fix the parser" #:name "worker" #:model "p/m" #:thinking "high" #:cwd "sub" #:better #t)"#,
                r#"(rlm-spawn "watch CI" #:name "ci" #:root #t)"#,
                r#"(rlm-find-models "claude" #:limit 5.0)"#,
                "(agent-roster #:children #t)",
                r#"(rlm-collect (list " reviewer " (hash 'rlm_child_id "c1")) #:timeout-ms 500)"#,
                "(rlm-collect)",
                r#"(rlm-delete " worker ")"#,
                r#"(rlm-abort (hash 'rlm_child_id "c1"))"#,
                r#"(rlm-rename "renamed" #:session-id "s1")"#,
                r#"(rlm-progress-note "half done")"#,
                "(end-turn)",
                "(rlm-reload)",
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(spawned),
                    Ok(created),
                    Ok(json!([model])),
                    Ok(json!([row])),
                    Ok(json!([result])),
                    Ok(json!([result])),
                    Ok(row),
                    Ok(aborted),
                    Ok(json!("renamed")),
                    Ok(throttled),
                    Ok(json!({"endedTurn": true})),
                    Ok(reloaded),
                ],
                vec![
                    json!({"type": "better.run", "task": "fix the parser", "mode": "brief", "cwd": "sub"}),
                    json!({
                        "type": "rlm.run",
                        "prompt": "the checked brief",
                        "kwargs": {"name": "worker", "model": "p/m", "thinking": "high", "cwd": "sub"},
                    }),
                    json!({"type": "rlm.create_session", "prompt": "watch CI", "kwargs": {"name": "ci"}}),
                    json!({"type": "rlm.find_models", "query": "claude", "limit": 5}),
                    json!({"type": "rlm.list_subagents"}),
                    json!({"type": "rlm.collect", "targets": ["reviewer", "c1"], "timeout_ms": 500}),
                    json!({"type": "rlm.collect", "targets": [], "timeout_ms": 0}),
                    json!({"type": "rlm.delete_subagent", "target": "worker"}),
                    json!({"type": "rlm.abort", "target": "c1"}),
                    json!({"type": "rlm.rename", "name": "renamed", "session_id": "s1"}),
                    json!({"type": "rlm.progress.note", "message": "half done"}),
                    json!({"type": "rlm.reload"}),
                ]
            )
        );
    }

    /// The host counts decisions per cell: one cell's decisions share an id,
    /// the next cell's get another.
    #[tokio::test(flavor = "multi_thread")]
    async fn decisions_return_the_chosen_option_and_are_counted_under_their_cell() {
        let (handlers, received) = answering(&[(
            "rlm.decide",
            json!({"index": 1, "row": "[decide] next? -> revert"}),
        )]);
        let kernel = kernel_over(handlers).await;
        let mut answers = Vec::new();
        for cell in [
            r#"(rlm-decide "next?" (list "retry" "revert") #:context "the log") (rlm-decide "then?" (list "stop" "go on"))"#,
            r#"(rlm-decide "next?" (list "retry" "revert"))"#,
        ] {
            let result = kernel.execute(cell, NO_OPTIONS).await.unwrap();
            answers.push((result.stdout, result.result));
        }
        let received = received.lock().unwrap().clone();
        let cells: Vec<&str> = received
            .iter()
            .map(|payload| payload["cell"].as_str().unwrap())
            .collect();
        let payload = |question: &str, options: Value, cell: &str| json!({"type": "rlm.decide", "question": question, "options": options, "cell": cell});
        let mut first = payload("next?", json!(["retry", "revert"]), cells[0]);
        first["context"] = json!("the log");
        assert_eq!(
            (
                answers,
                &received,
                cells[0] != cells[2],
                cells[0].is_empty()
            ),
            (
                vec![
                    (
                        "[decide] next? -> revert\n[decide] next? -> revert\n".to_string(),
                        Some("\"go on\"".to_string())
                    ),
                    (
                        "[decide] next? -> revert\n".to_string(),
                        Some("\"revert\"".to_string())
                    ),
                ],
                &vec![
                    first,
                    payload("then?", json!(["stop", "go on"]), cells[0]),
                    payload("next?", json!(["retry", "revert"]), cells[2]),
                ],
                true,
                false
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcripts_are_read_from_this_session_a_family_member_or_a_saved_session() {
        let rows = json!([{"index": 4, "id": "e5", "message": {"role": "user", "content": "hi"}}]);
        let page = json!({"messages": rows, "total": 5});
        let hit = json!({
            "session_id": "s9",
            "title": "parser work",
            "created_at": "2026-11-01T00:00:00Z",
            "updated_at": "2026-11-02T00:00:00Z",
            "match_count": 2,
            "message_count": 40,
            "snippets": [{"index": 7, "role": "assistant", "text": "the parser bug"}],
        });
        let (handlers, received) = answering(&[
            ("rlm.session.messages", page.clone()),
            ("agent_observe.session_messages", page.clone()),
            ("rlm.session.saved_messages", page),
            ("rlm.session.search", json!({"hits": [hit]})),
            (
                "rlm.session.tool_result",
                json!({"found": true, "toolName": "bash", "isError": false, "text": "the full output"}),
            ),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(history-read #:start -2 #:limit 1 #:role "user")"#,
                r#"(hash-ref (history-read #:limit 0 #:role (list "user" "assistant") #:agent (hash 'rlm_child_id "c1")) 'total)"#,
                r#"(hash-ref (history-read #:session (hash 'session_id "s9")) 'rows)"#,
                r#"(history-search "parser bug" #:limit 3)"#,
                r#"(history-read #:tool-call "call_1")"#,
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(json!({"total": 5, "rows": rows.clone()})),
                    Ok(json!(5)),
                    Ok(rows),
                    Ok(json!([hit])),
                    Ok(json!("the full output")),
                ],
                vec![
                    json!({"type": "rlm.session.messages", "start": -2, "limit": 1, "roles": ["user"]}),
                    json!({
                        "type": "agent_observe.session_messages",
                        "start": -10,
                        "limit": 0,
                        "roles": ["user", "assistant"],
                        "target": "c1",
                    }),
                    json!({
                        "type": "rlm.session.saved_messages",
                        "start": -10,
                        "limit": 10,
                        "roles": ["*"],
                        "session": "s9",
                    }),
                    json!({"type": "rlm.session.search", "query": "parser bug", "limit": 3}),
                    json!({"type": "rlm.session.tool_result", "toolCallId": "call_1"}),
                ]
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn family_members_are_messaged_and_observed() {
        let receipt = json!({"id": "am_1", "deliveryStatus": "delivered", "receiverRole": "child"});
        let waited = json!({"outcome": "timeout", "messageId": null});
        let family =
            json!({"current": {"sessionName": "lead"}, "agents": [{"sessionName": "worker"}]});
        let recent = json!({
            "agent": {"sessionName": "worker"},
            "messages": [],
            "limit": 3,
            "maxChars": 800,
            "truncated": false,
        });
        let (handlers, received) = answering(&[
            ("agent_message.send", receipt.clone()),
            ("agent_message.wait", waited.clone()),
            ("agent_observe.list", family.clone()),
            (
                "agent_observe.get",
                json!({"agent": {"sessionName": "worker"}}),
            ),
            ("agent_observe.recent", recent.clone()),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(agent-send "status?" #:to (hash 'relationship "child" 'sessionName "worker"))"#,
                r#"(agent-send "done" #:to "parent")"#,
                r#"(agent-send "stop" #:all #t)"#,
                r#"(event-wait #:timeout-ms 1000 #:sender-role "child")"#,
                "(agent-roster)",
                r#"(agent-roster "worker")"#,
                r#"(agent-roster "worker" #:recent #t #:limit 3)"#,
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(receipt.clone()),
                    Ok(receipt.clone()),
                    Ok(receipt),
                    Ok(waited),
                    Ok(family),
                    Ok(json!({"sessionName": "worker"})),
                    Ok(recent),
                ],
                vec![
                    json!({
                        "type": "agent_message.send",
                        "message": "status?",
                        "receiver_role": "child",
                        "receiver_name": "worker",
                    }),
                    json!({"type": "agent_message.send", "message": "done", "receiver_role": "parent"}),
                    json!({"type": "agent_message.send", "target": "all", "message": "stop"}),
                    json!({"type": "agent_message.wait", "timeout_ms": 1000, "sender_role": "child"}),
                    json!({"type": "agent_observe.list"}),
                    json!({"type": "agent_observe.get", "target": "worker"}),
                    json!({"type": "agent_observe.recent", "target": "worker", "limit": 3, "max_chars": 800}),
                ]
            )
        );
    }

    /// `(session-info)` is one hash of every section, each as its own call would return it.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_info_gathers_every_section_into_one_hash() {
        let (handlers, _received) = answering(&[
            ("model.info", json!({"id": "m", "provider": "p"})),
            ("settings.session", json!({"autonomous": false})),
            ("settings.host", json!({"daemonless": true})),
            ("usage.session", json!({"costUsd": 0.5})),
            ("usage.quotas", json!({"providers": []})),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(kernel.as_ref(), &["(session-info)"]).await;
        assert_eq!(
            returned,
            vec![Ok(json!({
                "id": false,
                "cwd": "/",
                "artifact-dir": false,
                "model": {"id": "m", "provider": "p"},
                "settings": {"session": {"autonomous": false}, "host": {"daemonless": true}},
                "usage": {"session": {"costUsd": 0.5}, "quotas": {"providers": []}},
                "skills-unavailable": [],
            }))]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compaction_refinement_the_goal_and_the_model_are_driven_from_a_cell() {
        let context = json!({"tokens": 52000, "context_window": 200_000, "percent": 26.5, "scheduled": false});
        let scheduled = json!({"scheduled": true});
        let goal =
            json!({"goal": {"objective": "ship it", "status": "active"}, "remaining_tokens": null});
        let model = json!({
            "id": "m",
            "provider": "p",
            "input": ["text", "image"],
            "contextWindow": 200_000,
            "thinkingLevel": "high",
        });
        let (handlers, received) = answering(&[
            ("compact.status", context.clone()),
            ("compact.run", scheduled.clone()),
            (
                "refine.status",
                json!({"pending": false, "in_flight": false}),
            ),
            ("refine.run", scheduled.clone()),
            ("goal.get", goal.clone()),
            ("goal.create", goal.clone()),
            ("goal.complete", goal.clone()),
            ("model.info", model.clone()),
            ("model.set_effort", json!({"thinkingLevel": "low"})),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                "(compact #:status #t)",
                "(compact)",
                r#"(compact "keep the failing test")"#,
                "(refine-status)",
                r#"(refine-run "the retry loop" #:global #t)"#,
                "(goal-get)",
                r#"(goal-create "ship it" #:token-budget 50000 #:check "make check" #:verify "judge" #:max-cost-usd 2.5)"#,
                "(goal-complete)",
                r#"(session-info "model")"#,
                r#"(model-set-effort "low")"#,
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(context),
                    Ok(scheduled.clone()),
                    Ok(scheduled.clone()),
                    Ok(json!({"pending": false, "in_flight": false})),
                    Ok(scheduled),
                    Ok(goal.clone()),
                    Ok(goal.clone()),
                    Ok(goal),
                    Ok(model),
                    Ok(json!("low")),
                ],
                vec![
                    json!({"type": "compact.status"}),
                    json!({"type": "compact.run"}),
                    json!({"type": "compact.run", "instructions": "keep the failing test"}),
                    json!({"type": "refine.status"}),
                    json!({"type": "refine.run", "instructions": "the retry loop", "global": true}),
                    json!({"type": "goal.get"}),
                    json!({
                        "type": "goal.create",
                        "objective": "ship it",
                        "token_budget": 50000,
                        "check": "make check",
                        "verify": "judge",
                        "max_cost_usd": 2.5,
                    }),
                    json!({"type": "goal.complete"}),
                    json!({"type": "model.info"}),
                    json!({"type": "model.set_effort", "level": "low"}),
                ]
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settings_usage_checkpoints_schedules_and_checks_are_read_and_changed() {
        let checkpoint =
            json!({"id": "cp-1", "label": "before refactor", "created_at": "2026-11-21T10:00:00Z"});
        let job = json!({"id": "job-1", "prompt": "check CI", "label": "ci", "kind": "interval", "status": "active"});
        let heartbeat = json!({"heartbeat": {"id": "hb-1", "status": "active"}});
        let heartbeats =
            json!({"heartbeats": [{"id": "hb-1", "label": "queue", "status": "active"}]});
        let checks = json!({"status": "pending", "checks": 2, "rounds": 1, "max_rounds": 3, "running": false, "last": null});
        let evidence =
            json!({"handle": "out:0123456789", "round": 1, "passed": 1, "failed": 1, "checks": []});
        let (handlers, received) = answering(&[
            ("settings.session", json!({"rlmDepth": 0})),
            (
                "settings.host",
                json!({"rlmMaxDepth": {"maxDepth": 3, "source": "default"}}),
            ),
            ("usage.session", json!({"cost": {"total": 0.25}})),
            ("usage.quotas", json!({"providers": []})),
            ("checkpoint.create", json!({"checkpoint": checkpoint})),
            ("checkpoint.list", json!({"checkpoints": [checkpoint]})),
            (
                "checkpoint.restore",
                json!({"status": "disabled", "message": "restoring is off"}),
            ),
            ("schedule.add", json!({"job": job})),
            ("schedule.list", json!({"jobs": [job]})),
            ("schedule.cancel", json!({"job": job})),
            ("rlm_heartbeat.list", heartbeats.clone()),
            ("rlm_heartbeat.create", heartbeat.clone()),
            ("rlm_heartbeat.update", heartbeat.clone()),
            ("rlm_heartbeat.delete", heartbeat.clone()),
            (
                "better.run",
                json!({"rewrite": "the deeper prompt", "unchanged": false}),
            ),
            ("better.checks", checks.clone()),
            ("better.evidence", evidence.clone()),
        ]);
        let kernel = kernel_over(handlers).await;
        let mut returned = values(
            kernel.as_ref(),
            &[
                r#"(session-info "settings")"#,
                r#"(session-info "usage" #:refresh #t)"#,
                r#"(checkpoint-create "before refactor")"#,
                "(checkpoint-list)",
                r#"(checkpoint-restore "cp-1")"#,
                r#"(event-define "ci" #f #:kind "schedule" #:prompt "check CI" #:timer (hash 'every 1800))"#,
                r#"(event-list #:kind "schedule" #:include-inactive #t)"#,
                r#"(event-remove "job-1" #:kind "schedule")"#,
                r#"(event-list #:kind "heartbeat")"#,
                r#"(event-define "fresh" #f #:kind "heartbeat" #:prompt "review the queue" #:timer (hash 'every "30m") #:delivery-mode "follow_up" #:status "pause")"#,
                r#"(event-define "queue" #f #:kind "heartbeat" #:prompt "review it again")"#,
                r#"(event-remove "queue" #:kind "heartbeat")"#,
                r#"(better "fix the parser" #:cwd "sub")"#,
                "(better-checks)",
                r#"(better-evidence "out:0123456789")"#,
            ],
        )
        .await;
        let mut received = received.lock().unwrap().clone();
        // The restore names its confirmation question with a fresh id.
        let restore = received.remove(6);
        let refused = returned.remove(4);
        assert_eq!(
            (
                returned,
                received,
                restore["id"].clone(),
                restore["question_id"].as_str().map(str::len),
                refused.is_err_and(|error| error.contains("restoring is off")),
            ),
            (
                vec![
                    Ok(json!({
                        "session": {"rlmDepth": 0},
                        "host": {"rlmMaxDepth": {"maxDepth": 3, "source": "default"}},
                    })),
                    Ok(json!({"session": {"cost": {"total": 0.25}}, "quotas": {"providers": []}})),
                    Ok(checkpoint.clone()),
                    Ok(json!([checkpoint])),
                    Ok(job.clone()),
                    Ok(json!([job])),
                    Ok(job),
                    Ok(heartbeats),
                    Ok(heartbeat.clone()),
                    Ok(heartbeat.clone()),
                    Ok(heartbeat),
                    Ok(json!("the deeper prompt")),
                    Ok(checks),
                    Ok(evidence),
                ],
                vec![
                    json!({"type": "settings.session"}),
                    json!({"type": "settings.host"}),
                    json!({"type": "usage.session"}),
                    json!({"type": "usage.quotas", "refresh": true}),
                    json!({"type": "checkpoint.create", "label": "before refactor"}),
                    json!({"type": "checkpoint.list"}),
                    // A job of the same name is cancelled before the new one is added.
                    json!({"type": "schedule.list", "include_inactive": false}),
                    json!({"type": "schedule.cancel", "id": "job-1"}),
                    json!({"type": "schedule.add", "prompt": "check CI", "every": "1800s", "label": "ci"}),
                    json!({"type": "schedule.list", "include_inactive": true}),
                    json!({"type": "schedule.list", "include_inactive": false}),
                    json!({"type": "schedule.cancel", "id": "job-1"}),
                    json!({"type": "rlm_heartbeat.list", "include_inactive": false}),
                    // A new heartbeat is created, and paused when asked.
                    json!({"type": "rlm_heartbeat.list", "include_inactive": false}),
                    json!({
                        "type": "rlm_heartbeat.create",
                        "instruction": "review the queue",
                        "label": "fresh",
                        "interval": "30m",
                        "delivery_mode": "follow_up",
                    }),
                    json!({"type": "rlm_heartbeat.update", "id": "hb-1", "status": "pause"}),
                    // One of that name is updated in place.
                    json!({"type": "rlm_heartbeat.list", "include_inactive": false}),
                    json!({"type": "rlm_heartbeat.update", "id": "hb-1", "instruction": "review it again"}),
                    json!({"type": "rlm_heartbeat.list", "include_inactive": false}),
                    json!({"type": "rlm_heartbeat.delete", "id": "hb-1"}),
                    json!({"type": "better.run", "task": "fix the parser", "mode": "deepen", "cwd": "sub"}),
                    json!({"type": "better.checks"}),
                    json!({"type": "better.evidence", "handle": "out:0123456789"}),
                ],
                json!("cp-1"),
                Some(32),
                true,
            )
        );
    }

    /// Against the session's real `question.*`, `todo.*` and `notify.send`
    /// handlers: with nobody attached a question settles at once on its
    /// default, and the todo list round-trips.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_real_interaction_handlers_accept_questions_todos_and_pings() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            dir.path(),
        )));
        let mut handlers = HostRequestHandlers::new();
        InteractionHub::headless().register_host_handlers(&session, &mut handlers);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(ask "Which store?" (list "sqlite" "postgres") #:default "sqlite" #:timeout 30)"#,
                r#"(ask "Which stores?" (list "sqlite" "postgres") #:multi #t #:default (list "sqlite" "postgres"))"#,
                r#"(ask "What should it be called?")"#,
                r#"(todo-set (list "write the tests" (hash 'text "ship" 'status "in_progress" 'note "after review")))"#,
                r#"(todo-set (hash 'id "1" 'status "done" 'note "green") #:merge #t)"#,
                "(todo-get)",
                r#"(notify "tests pass" #:level "success")"#,
            ],
        )
        .await;
        let todos = json!({"items": [
            {"id": "1", "text": "write the tests", "status": "done", "note": "green"},
            {"id": "2", "text": "ship", "status": "in_progress", "note": "after review"},
        ]});
        assert_eq!(
            returned,
            vec![
                Ok(json!({"status": "no_user", "answer": "sqlite"})),
                Ok(json!({"status": "no_user", "answer": ["sqlite", "postgres"]})),
                Ok(json!({"status": "no_user", "answer": false})),
                Ok(json!({"items": [
                    {"id": "1", "text": "write the tests", "status": "pending"},
                    {"id": "2", "text": "ship", "status": "in_progress", "note": "after review"},
                ]})),
                Ok(todos.clone()),
                Ok(todos),
                Ok(json!(false)),
            ]
        );
    }

    /// Against the real `ctx.*` handlers over a stored output.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_real_ctx_handlers_page_tail_and_grep_a_stored_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SpillStore::for_session_dir(dir.path());
        let text = "alpha\nbeta one\ngamma\nbeta two\nomega\n";
        let handle = format!("out:{}", store.spill(SpillCopy::of_text(text)).id);
        let mut handlers = HostRequestHandlers::new();
        register_ctx_handlers(&mut handlers, Some(&store), /*observer*/ None);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                &format!(r#"(output-read "{handle}" #:info #t)"#),
                &format!(r#"(output-read "{handle}" #:start 1 #:lines 2)"#),
                &format!(r#"(output-read "{handle}" #:tail 1)"#),
                &format!(r#"(output-grep "{handle}" "^beta" #:context 0 #:max-matches 1)"#),
                r#"(output-read "out:0000000000")"#,
            ],
        )
        .await;
        let stored = |fields: Value| {
            let mut reply = json!({
                "handle": handle,
                "state": "stored",
                "totalChars": text.len(),
                "lines": 5,
                "storedLines": 5,
            });
            reply
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            Ok(reply)
        };
        assert_eq!(
            returned,
            vec![
                stored(json!({})),
                stored(json!({"start": 1, "end": 3, "text": "beta one\ngamma\n", "truncated": false})),
                stored(json!({"start": 4, "end": 5, "text": "omega\n", "truncated": false})),
                stored(json!({"pattern": "^beta", "matches": 2, "shown": 1, "text": "1:beta one\n", "truncated": true})),
                Err("Error: Generic:  output-read: out:0000000000 is unknown; re-run the command to see its output".to_string()),
            ]
        );
    }

    /// A call the host would misread is refused in the cell, before any
    /// request is sent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_call_raises_without_reaching_the_host() {
        let (handlers, received) = answering(&[
            ("rlm.run", json!({})),
            ("rlm.collect", json!({})),
            ("question.ask", json!({})),
            ("todo.update", json!({})),
        ]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(rlm-spawn "fix the parser")"#,
                "(rlm-collect #:timeout-ms 1.5)",
                r#"(ask "Which?" #:multi #t)"#,
                r#"(todo-set (hash 'id "1") #:merge #t)"#,
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Err("Error: Generic:  rlm-spawn: #:name is required and must be a string".to_string()),
                    Err("Error: Generic:  rlm-collect: #:timeout-ms must be an integer".to_string()),
                    Err("Error: Generic:  ask: #:multi needs choices".to_string()),
                    Err("Error: Generic:  todo-set: a #:merge item changes at least one of 'status, 'text, 'note".to_string()),
                ],
                Vec::new()
            )
        );
    }

    /// Steel's prelude binds hundreds of names; a definition here that reused
    /// one would replace a builtin for every cell.
    #[test]
    fn no_api_name_replaces_a_steel_builtin() {
        let mut engine = Engine::new();
        let taken: Vec<&str> = SOURCES
            .iter()
            .flat_map(|(_, source)| source.lines())
            .filter_map(|line| line.strip_prefix("(define "))
            .filter_map(|rest| rest.trim_start_matches('(').split([' ', ')']).next())
            .filter(|name| engine.run((*name).to_string()).is_ok())
            .collect();
        assert_eq!(taken, Vec::<&str>::new());
    }

    /// A row the host has not loaded (compacted away in a windowed session)
    /// is found by paging the transcript's tool results, newest first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tool_result_the_host_has_not_loaded_is_found_by_paging_the_transcript() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "rlm.session.tool_result",
            host_handler(|_payload: HostRequestPayload| async { Ok(json!({"found": false})) }),
        );
        let paged = Arc::clone(&received);
        handlers.register(
            "rlm.session.messages",
            host_handler(move |payload: HostRequestPayload| {
                let (start, limit) = (
                    payload.data["start"].as_u64().unwrap(),
                    payload.data["limit"].as_u64().unwrap(),
                );
                paged.lock().unwrap().push(json!([start, limit]));
                let rows: Vec<Value> = (start..start + limit)
                    .map(|index| {
                        json!({
                            "index": index,
                            "id": format!("e{index}"),
                            "message": {
                                "role": "toolResult",
                                "toolCallId": format!("call_{index}"),
                                "content": [
                                    {"type": "text", "text": format!("output {index}")},
                                    {"type": "image", "data": "..."},
                                    {"type": "text", "text": "its last line"},
                                ],
                            },
                        })
                    })
                    .collect();
                async move { Ok(json!({"messages": rows, "total": 120})) }
            }),
        );
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(history-read #:tool-call "call_30")"#,
                r#"(history-read #:tool-call "call_500")"#,
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(json!("output 30\nits last line")),
                    Err("Error: Generic:  history-read: no tool result in this session has the id call_500".to_string()),
                ],
                vec![
                    json!([0, 0]),
                    json!([70, 50]),
                    json!([20, 50]),
                    json!([0, 0]),
                    json!([70, 50]),
                    json!([20, 50]),
                    json!([0, 20]),
                ],
            )
        );
    }

    /// A numeric `'at` is a delay: the host is sent the UTC time that many
    /// seconds from now.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_numeric_schedule_time_is_sent_as_the_utc_time_that_delay_from_now() {
        let (handlers, received) = answering(&[
            ("schedule.add", json!({"job": {"id": "job-1"}})),
            ("schedule.list", json!({"jobs": []})),
        ]);
        let kernel = kernel_over(handlers).await;
        let now = || crate::session::manager::format_iso_now();
        let before = crate::cron::parse_instant(&now()).unwrap();
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(event-define "ci" #f #:kind "schedule" #:prompt "check CI" #:timer (hash 'at 90))"#,
                r#"(event-define "ci" #f #:kind "schedule" #:prompt "check CI" #:timer (hash 'at "in 30m"))"#,
                r#"(event-define "ci" #f #:kind "schedule" #:prompt "check CI" #:timer (hash 'at -5))"#,
            ],
        )
        .await;
        let after = crate::cron::parse_instant(&now()).unwrap();
        let mut received = received.lock().unwrap().clone();
        let at = received[1]["at"].take();
        let fires = crate::cron::parse_instant(at.as_str().unwrap()).unwrap();
        assert_eq!(
            (
                returned,
                received,
                (before + 90_000..=after + 90_000).contains(&fires),
            ),
            (
                vec![
                    Ok(json!({"id": "job-1"})),
                    Ok(json!({"id": "job-1"})),
                    Err("Error: Generic:  event-define: a numeric 'at is a delay in seconds and must be positive".to_string()),
                ],
                vec![
                    json!({"type": "schedule.list", "include_inactive": false}),
                    json!({"type": "schedule.add", "prompt": "check CI", "label": "ci", "at": null}),
                    json!({"type": "schedule.list", "include_inactive": false}),
                    json!({"type": "schedule.add", "prompt": "check CI", "label": "ci", "at": "in 30m"}),
                    json!({"type": "schedule.list", "include_inactive": false}),
                ],
                true,
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn searches_and_reads_reach_the_hosts_engine_and_return_its_replies() {
        let found = json!({
            "provider": "duckduckgo",
            "label": "DuckDuckGo",
            "answer": null,
            "sources": [{"title": "Steel", "url": "https://example.org/steel", "snippet": "a Scheme"}],
            "notes": [],
            "text": "1. Steel",
        });
        let page = json!({
            "url": "https://example.org/steel",
            "finalUrl": "https://example.org/steel/",
            "markdown": "# Steel",
            "outline": [{"level": 1, "text": "Steel", "anchor": "steel"}],
            "truncated": false,
            "totalChars": 7,
        });
        let (handlers, received) =
            answering(&[("web.search", found.clone()), ("web.read", page.clone())]);
        let kernel = kernel_over(handlers).await;
        let returned = values(
            kernel.as_ref(),
            &[
                r#"(web-search "steel scheme site:example.org" #:provider "duckduckgo" #:num-results 5.0 #:recency "month" #:timeout 12.5)"#,
                r#"(web-read (car (hash-ref (web-search "steel") 'sources)) #:max-chars 4000 #:section "steel")"#,
                r#"(web-read " https://example.org/steel ")"#,
                "(web-read 42)",
            ],
        )
        .await;
        assert_eq!(
            (returned, received.lock().unwrap().clone()),
            (
                vec![
                    Ok(found),
                    Ok(page.clone()),
                    Ok(page),
                    Err(
                        "Error: Generic:  web-read: the source must be a URL or carry one under 'url"
                            .to_string()
                    ),
                ],
                vec![
                    json!({
                        "type": "web.search",
                        "query": "steel scheme site:example.org",
                        "provider": "duckduckgo",
                        "numResults": 5,
                        "recency": "month",
                        "timeoutSeconds": 12.5,
                    }),
                    json!({"type": "web.search", "query": "steel"}),
                    json!({
                        "type": "web.read",
                        "url": "https://example.org/steel",
                        "maxChars": 4000,
                        "section": "steel",
                    }),
                    json!({"type": "web.read", "url": "https://example.org/steel"}),
                ],
            )
        );
    }
}
