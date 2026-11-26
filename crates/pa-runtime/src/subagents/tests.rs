//! Subagents as runtime sessions, on engines over pa-ai's faux provider:
//! a spawn opens the child's session, its `done` reaches the parent, a
//! collect waits for it (and ends with its cell), and a settle waits for it.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_core::session::manager::{get_session_file_path, SessionManager};
use pa_core::session_engine::rlm_host::{rlm_depth_limit_error, RlmSpawnRequest, RlmSubagentHost};
use pa_types::event_log::{AgentDone, AgentInput, EventId, NewEvent, TaskState, Topic, Trust};
use serde_json::{json, Value};

use pa_core::session_engine::agent_messaging::{
    create_agent_session_message_prompt, AgentFamilyRelationship, AgentMessageController,
    AgentMessageDeliveryStatus, AgentMessageKind, AgentMessagePromptPayload, AgentMessageReceipt,
    AgentMessageSendInput, DEFAULT_AGENT_MESSAGE_WAKE_BUDGET, DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS,
};
use pa_core::session_engine::bash_notices::{KernelNotice, NoticeAdmission, NoticeDelivery};
use pa_core::session_engine::messages::{
    create_async_bash_quiet_message, QuietJob, ASYNC_BASH_QUIET_PREVIEW_LABEL,
};

use super::{
    kernel_notice_sink, Delivery, FamilyMessages, MessageSender, Node, SessionSubagents, Subagents,
};
use crate::bus::{Heard, Listener};
use crate::runtime::{Runtime, RuntimeOptions};
use crate::session::{
    BuiltSession, DriverEvents, Outbox, SessionBuilder, SessionSeams, SessionSpec, SettleAfter,
};
use crate::subscriptions::MemorySessionTree;

/// A subagent task whose model calls wait at the builder's [`Hold`].
const HOLD_TASK: &str = "hold on";

/// Where a subagent's model call over [`HOLD_TASK`] waits: it says it has started,
/// then waits for one permit, which it gives back.
struct Hold {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

/// Builds the print session's engine over `script.responses` and each
/// subagent's over `script.children.<its name>`, each on a faux provider of
/// its own, with the runtime's seams installed and the transcript under
/// `<cwd>/sessions`.
struct TreeBuilder {
    agent_dir: PathBuf,
    script: Value,
    hold: Arc<Hold>,
}

impl SessionBuilder for TreeBuilder {
    async fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: SessionSeams,
    ) -> anyhow::Result<BuiltSession> {
        let responses = match (&spec.subagent, &spec.name) {
            (Some(_), Some(name)) => self.script["children"][name].clone(),
            (Some(_), None) => anyhow::bail!("a subagent is spawned with its name"),
            (None, _) => self.script["responses"].clone(),
        };
        let parsed = pa_ai::faux::script::parse_faux_script(&json!({ "responses": responses }))
            .map_err(anyhow::Error::msg)?;
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some(format!("faux-{id}")),
                models: Some(vec![parsed.model]),
                ..Default::default()
            });
        registration.set_responses(parsed.responses);
        let model = registration.get_model();
        let inner = pa_core::session_engine::provider_adapter::real_stream_fn(None, model.clone());
        // Only a subagent holds: its parent's messages carry the task too.
        let hold = spec.subagent.is_some().then(|| Arc::clone(&self.hold));
        let stream_fn: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
            let messages = serde_json::to_string(&context.messages).unwrap_or_default();
            let gate = hold.clone().filter(|_| messages.contains(HOLD_TASK));
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    let _permit = gate.release.acquire().await?;
                }
                inner(model, context, options).await
            })
        });
        let sessions = spec.cwd.join("sessions");
        let engine = Box::pin(pa_core::session_engine::engine::create_session(
            pa_core::session_engine::engine::SessionEngineConfig {
                on_late_sent_agent_message: Some(seams.on_late_sent_agent_message),
                semantic_edges: None,
                cron_store: None,
                steering_mode: None,
                follow_up_mode: None,
                image_model_router: None,
                telemetry: None,
                cwd: spec.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                mcp_manager: None,
                model: pa_core::session_engine::provider_adapter::json_round_trip(&model),
                thinking_level: None,
                stream_fn: Some(stream_fn),
                tools: Vec::new(),
                custom_system_prompt: None,
                prompt_guidelines: Vec::new(),
                generic_mcp_servers: Vec::new(),
                allow_recursion: None,
                session_manager: Some(SessionManager::open(
                    &spec.cwd,
                    &sessions,
                    &get_session_file_path(&sessions, id),
                )),
                extra_host_handlers: Some(seams.host_handlers),
                conversation_log_path: None,
                additional_skill_paths: Vec::new(),
                additional_prompt_paths: Vec::new(),
                extra_builtin_skill_overrides: Vec::new(),
                rlm_subagent_host: Some(seams.subagents),
                rlm_usage_store: None,
                progress_note_emit: None,
                interaction: None,
                rlm_depth: spec.subagent.as_ref().map(|subagent| subagent.depth),
                factory_child: false,
                daemonless: true,
                model_info: Some(model.clone()),
                prewarm_kernel: Some(true),
                on_background_work_settled: Some(Arc::clone(&seams.bash_wake)),
                bash_notice_wake: None,
                kernel_notice_sink: Some(seams.kernel_notices),
                call_policy: Some(seams.call_policy),
                queued_goal_context_purge: None,
                queued_steering_probe: None,
                should_stop_after_turn: None,
            },
        ))
        .await?;
        Ok(BuiltSession {
            engine: Arc::new(engine),
            model,
            api_key: None,
            agent_dir: self.agent_dir.clone(),
            events: DriverEvents::Stream,
            autonomous: None,
            queue_modes: crate::session::QueueModes::Built,
        })
    }
}

/// A runtime with one open session (the parent) over `script`.
struct Bed {
    runtime: Runtime<TreeBuilder>,
    listener: Listener,
    parent: String,
    cwd: PathBuf,
    hold: Arc<Hold>,
    _dir: tempfile::TempDir,
}

async fn bed(script: Value) -> Bed {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "compaction": { "enabled": false } }).to_string(),
    )
    .unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let hold = Arc::new(Hold {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let runtime = Runtime::open(
        &agent_dir,
        RuntimeOptions {
            builder: TreeBuilder {
                agent_dir: agent_dir.clone(),
                script,
                hold: Arc::clone(&hold),
            },
            spare_kernels: 1,
            max_concurrent_runs: NonZeroUsize::new(4).unwrap(),
            listener_capacity: NonZeroUsize::new(4096).unwrap(),
            cwd: cwd.clone(),
            sessions: Vec::new(),
        },
    )
    .await
    .unwrap();
    let listener = runtime.listen();
    let parent = runtime
        .create_session(SessionSpec {
            cwd: cwd.clone(),
            ..SessionSpec::default()
        })
        .await
        .unwrap();
    Bed {
        runtime,
        listener,
        parent,
        cwd,
        hold,
        _dir: dir,
    }
}

impl Bed {
    /// Publishes `text` as the parent's input; the input event's id.
    async fn input(&self, text: &str) -> EventId {
        self.runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{}/input", self.parent)).unwrap(),
                source: "test".to_string(),
                session: Some(self.parent.clone()),
                payload: serde_json::to_value(AgentInput {
                    text: text.to_string(),
                    images: Vec::new(),
                    priority: None,
                })
                .unwrap(),
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await
            .unwrap()
            .event
            .id
    }

    /// The parent's stream payloads up to its next `done`, and the `done`.
    async fn until_done(&mut self) -> (Vec<Value>, AgentDone) {
        until_done_of(&mut self.listener, &self.parent).await
    }

    /// The node of the session `id`.
    fn node(&self, id: &str) -> Arc<Node> {
        self.runtime
            .subagents()
            .node(id)
            .expect("the session is open")
    }

    /// The parent's children's nodes, by name.
    async fn kids(&self) -> BTreeMap<String, Arc<Node>> {
        self.node(&self.parent)
            .children
            .lock()
            .await
            .iter()
            .map(|child| {
                let name = child.node.session_name().expect("a spawned child is named");
                (name, Arc::clone(&child.node))
            })
            .collect()
    }

    /// `from`'s agent message `text` to the session `to`, and its receipt.
    async fn send(&self, from: &Arc<Node>, to: &str, text: &str) -> AgentMessageReceipt {
        FamilyMessages(Arc::clone(from))
            .send_agent_message(AgentMessageSendInput {
                target: to.to_string(),
                message: text.to_string(),
                receiver_role: None,
                kind: AgentMessageKind::Direct,
            })
            .await
            .unwrap()
    }

    /// Finishes the parent's run, closes the runtime, and reads the parent's
    /// transcript.
    async fn close(self) -> Vec<Value> {
        self.runtime.finish(&self.parent).await.unwrap();
        self.runtime.shutdown().await;
        entries(&self.cwd, &self.parent)
    }
}

/// The transcript entries of the session `id` working in `cwd`.
fn entries(cwd: &std::path::Path, id: &str) -> Vec<Value> {
    let sessions = cwd.join("sessions");
    SessionManager::open(cwd, &sessions, &get_session_file_path(&sessions, id))
        .get_entries()
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect()
}

/// `session`'s stream payloads up to its next `done` that `listener` hears,
/// and the `done`; other sessions' events are passed over.
async fn until_done_of(listener: &mut Listener, session: &str) -> (Vec<Value>, AgentDone) {
    let stream = format!("agent/{session}/stream");
    let done = format!("agent/{session}/done");
    let mut payloads = Vec::new();
    loop {
        let event = heard(listener).await;
        if event.event.topic.as_str() == stream {
            payloads.push(event.event.payload.clone());
        } else if event.event.topic.as_str() == done {
            return (
                payloads,
                serde_json::from_value(event.event.payload.clone()).unwrap(),
            );
        }
    }
}

/// The payload of the next event on `topic` that `listener` hears.
async fn payload_on(listener: &mut Listener, topic: &str) -> Value {
    loop {
        let event = heard(listener).await;
        if event.event.topic.as_str() == topic {
            return event.event.payload.clone();
        }
    }
}

/// The next event `listener` hears.
async fn heard(listener: &mut Listener) -> Arc<pa_types::event_log::Event> {
    let heard = tokio::time::timeout(Duration::from_secs(60), listener.recv())
        .await
        .expect("the runtime publishes")
        .expect("the bus is open");
    let Heard::Event(event) = heard else {
        panic!("the listener lagged");
    };
    event
}

fn eval_cell(code: &str) -> Value {
    json!({ "content": [{ "type": "toolCall", "id": "cell", "name": "eval", "arguments": { "code": code } }] })
}

/// The parent's turn: spawns `kid` with `task`, then ends.
fn spawn_kid(task: &str) -> Value {
    eval_cell(&format!("(rlm-spawn \"{task}\" #:name \"kid\")"))
}

/// A spawned child runs its task as its own session, and its `done`
/// delivers today's no-reply notice to the parent, whose settle runs it.
#[tokio::test]
async fn a_spawned_childs_done_delivers_its_notice_to_the_parent() {
    let bed = bed(json!({
        "responses": [spawn_kid("run the task"), { "text": "WAITING" }, { "text": "PARENT DONE" }],
        "children": { "kid": [{ "text": "kid finished" }] },
    }))
    .await;
    let input = bed.input("delegate").await;
    bed.runtime
        .settle(&bed.parent, SettleAfter::Task(input))
        .await
        .unwrap();
    let notices: Vec<Value> = bed
        .close()
        .await
        .into_iter()
        .filter(|entry| entry["customType"] == "rlm_child_terminal_notice")
        .map(|entry| entry["content"].clone())
        .collect();
    assert_eq!(
        notices,
        [json!(
            "[child-exited: no-reply child:kid]\n\nLast assistant text: kid finished"
        )]
    );
}

/// A session at its depth bound refuses a spawn with the shared error.
#[tokio::test]
async fn a_spawn_at_the_depth_bound_is_refused() {
    let (outbox, _outgoing) = Outbox::channel();
    let tree = Arc::new(Subagents::new(outbox, MemorySessionTree::default()));
    let node = Arc::new(Node::new(
        &tree,
        "deep".to_string(),
        &SessionSpec::default(),
        /*depth*/ 1,
        pa_core::session_engine::rlm_depth::DepthBound::Fixed(1),
        /*parent*/ None,
    ));
    let refused = SessionSubagents(node)
        .spawn(RlmSpawnRequest {
            prompt: "go deeper".to_string(),
            name: None,
            model: None,
            thinking: None,
            cwd: None,
            spawned_by_request_id: None,
            factory: None,
        })
        .await
        .map(|handle| handle.rlm_child_id)
        .map_err(|error| format!("{error:#}"));
    assert_eq!(refused, Err(format!("{:#}", rlm_depth_limit_error(1, 1))));
}

/// A collect in the parent's cell returns the child's answer once the
/// child settles.
#[tokio::test]
async fn a_collect_returns_once_the_child_settles() {
    let mut bed = bed(json!({
        "responses": [
            eval_cell(
                "(rlm-spawn \"answer\" #:name \"kid\") \
                 (hash-ref (car (rlm-collect (list \"kid\") #:timeout-ms 60000)) 'answer_text)",
            ),
            { "text": "COLLECTED" },
        ],
        "children": { "kid": [{ "text": "kid answered" }] },
    }))
    .await;
    bed.input("collect").await;
    let (stream, done) = bed.until_done().await;
    assert_eq!(done.outcome, TaskState::Done);
    let answers: Vec<Value> = stream
        .iter()
        .filter(|payload| payload["type"] == "tool_execution_end")
        .map(|payload| payload["result"]["details"]["result"].clone())
        .collect();
    assert_eq!(answers, [json!("\"kid answered\"")]);
    bed.close().await;
}

/// A collect waiting on a child that never settles ends with its cell: the
/// cancelled task ends long before the collect's deadline.
#[tokio::test]
async fn a_collect_ends_with_its_cancelled_cell() {
    let mut bed = bed(json!({
        "responses": [
            eval_cell(&format!(
                "(rlm-spawn \"{HOLD_TASK}\" #:name \"kid\") (rlm-collect (list \"kid\") #:timeout-ms 600000)"
            )),
            { "text": "never read" },
        ],
        "children": { "kid": [{ "text": "kid released" }] },
    }))
    .await;
    bed.input("collect").await;
    // The child is running (its model call waits): the parent's cell has
    // spawned it and goes on to its collect.
    bed.hold.entered.notified().await;
    bed.runtime.cancel(&bed.parent).await.unwrap();
    let (_, done) = tokio::time::timeout(Duration::from_secs(30), bed.until_done())
        .await
        .expect("the cancelled cell's collect ends with it");
    assert_eq!(done.outcome, TaskState::Cancelled);
    bed.hold.release.add_permits(1);
    bed.close().await;
}

/// A settle waits for an unsettled child: it answers once the child's
/// notice has run as the parent's turn.
#[tokio::test]
async fn a_settle_waits_for_an_unsettled_child() {
    let mut bed = bed(json!({
        "responses": [spawn_kid(HOLD_TASK), { "text": "WAITING" }, { "text": "PARENT DONE" }],
        "children": { "kid": [{ "text": "kid finished" }] },
    }))
    .await;
    bed.input("delegate").await;
    let (_, done) = bed.until_done().await;
    assert_eq!(done.outcome, TaskState::Done);
    // Queued while the child's model call is held: a settle that did not
    // wait for it would answer with the parent's "WAITING".
    let settle = bed.runtime.settle(&bed.parent, SettleAfter::Queued);
    bed.hold.entered.notified().await;
    bed.hold.release.add_permits(1);
    let settled = settle.await.unwrap().expect("the run settled");
    assert_eq!(
        settled
            .terminal
            .primary
            .and_then(|primary| primary.stdout_text()),
        Some("PARENT DONE".to_string())
    );
    bed.close().await;
}

/// A late receipt on a child holds its ancestors' rest from its admission
/// (waking an armed scan) until it is recorded, and a failed record
/// reaches the ancestors' drives as their error.
#[tokio::test]
async fn a_childs_late_receipt_holds_and_wakes_its_ancestors() {
    let (outbox, _outgoing) = Outbox::channel();
    let tree = Arc::new(Subagents::new(outbox, MemorySessionTree::default()));
    let bound = pa_core::session_engine::rlm_depth::DepthBound::Fixed(2);
    let root = Arc::new(Node::new(
        &tree,
        "root".to_string(),
        &SessionSpec::default(),
        /*depth*/ 0,
        bound,
        /*parent*/ None,
    ));
    let child = Arc::new(Node::new(
        &tree,
        "child".to_string(),
        &SessionSpec::default(),
        /*depth*/ 1,
        bound,
        Some(Arc::downgrade(&root)),
    ));
    let admitted = root.activity.notified();
    tokio::pin!(admitted);
    admitted.as_mut().enable();
    super::late_sent_agent_message_sink(&child)(
        "call-1",
        pa_core::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_1".to_string(),
            message: "Ping.".to_string(),
            delivery_status: pa_core::kernel::shared::SentDeliveryStatus::Delivered,
            receiver_role: Some(pa_core::kernel::shared::ReceiverRole::Parent),
            target: pa_core::kernel::shared::SentAgentMessageTarget {
                active_session_id: "a1".to_string(),
                session_id: "s1".to_string(),
                session_name: None,
            },
        },
    );
    assert!(super::has_unsettled_rlm_work(&root).await);
    assert!(
        tokio::time::timeout(Duration::ZERO, admitted).await.is_ok(),
        "the receipt's admission wakes an armed scan before it is recorded"
    );
    let recorded = root.activity.notified();
    tokio::pin!(recorded);
    recorded.as_mut().enable();
    tokio::time::timeout(Duration::from_secs(5), recorded)
        .await
        .expect("the record wakes the ancestor");
    assert_eq!(
        (
            root.pending_receipts
                .load(std::sync::atomic::Ordering::SeqCst),
            crate::lock(&root.late_receipt_error).clone(),
        ),
        (
            0,
            Some("late receipt arrived without a live session engine".to_string())
        )
    );
}

/// An agent message to an idle sibling wakes it: it is delivered, and the
/// sibling's next task runs over today's `agent_message` row.
#[tokio::test]
async fn an_inbox_message_wakes_an_idle_sibling_with_todays_row() {
    let bed = bed(json!({
        "responses": [
            eval_cell("(rlm-spawn \"go\" #:name \"a\") (rlm-spawn \"go\" #:name \"b\")"),
            { "text": "WAITING" },
            { "text": "PARENT" },
            { "text": "PARENT" },
        ],
        "children": {
            "a": [{ "text": "a done" }],
            "b": [{ "text": "b done" }, { "text": "b heard a" }],
        },
    }))
    .await;
    let input = bed.input("delegate").await;
    bed.runtime
        .settle(&bed.parent, SettleAfter::Task(input))
        .await
        .unwrap();
    let kids = bed.kids().await;
    let (a, b) = (&kids["a"], &kids["b"]);
    let mut listener = bed.runtime.listen();
    let receipt = bed.send(a, &b.session_id, "ping from a").await;
    let (_, done) = until_done_of(&mut listener, &b.session_id).await;
    let rows: Vec<Value> = entries(&bed.cwd, &b.session_id)
        .into_iter()
        .filter(|entry| entry["customType"] == "agent_message")
        .map(|entry| entry["content"].clone())
        .collect();
    assert_eq!(
        (receipt.delivery_status, done.outcome, rows),
        (
            AgentMessageDeliveryStatus::Delivered,
            TaskState::Done,
            vec![
                json!("[task from parent]\n\ngo"),
                json!(create_agent_session_message_prompt(
                    &AgentMessagePromptPayload {
                        message: "ping from a".to_string(),
                        sender_name: "a".to_string(),
                        from_relationship: Some(AgentFamilyRelationship::Sibling),
                    }
                )),
            ]
        )
    );
    bed.close().await;
}

/// One sender wakes an idle receiver at most the budget's times in the
/// window: the next message is refused with `wake_refused` and when to
/// retry, and is not delivered.
#[tokio::test]
async fn the_wake_past_the_budget_of_an_idle_receiver_is_refused() {
    let mut kid = vec![json!({ "text": "kid done" })];
    kid.extend(std::iter::repeat_n(
        json!({ "text": "heard" }),
        DEFAULT_AGENT_MESSAGE_WAKE_BUDGET + 1,
    ));
    let bed = bed(json!({
        "responses": [spawn_kid("start"), { "text": "WAITING" }, { "text": "PARENT" }],
        "children": { "kid": kid },
    }))
    .await;
    let input = bed.input("delegate").await;
    bed.runtime
        .settle(&bed.parent, SettleAfter::Task(input))
        .await
        .unwrap();
    let parent = bed.node(&bed.parent);
    let kid = Arc::clone(&bed.kids().await["kid"]);
    let started = std::time::Instant::now();
    // Each message finds the child settled: it wakes it, and the child's
    // task over it ends before the next is sent.
    let mut listener = bed.runtime.listen();
    for round in 0..DEFAULT_AGENT_MESSAGE_WAKE_BUDGET {
        let receipt = bed
            .send(&parent, &kid.session_id, &format!("ping {round}"))
            .await;
        let (_, done) = until_done_of(&mut listener, &kid.session_id).await;
        assert_eq!(
            (receipt.delivery_status, done.outcome),
            (AgentMessageDeliveryStatus::Delivered, TaskState::Done)
        );
    }
    let refused = bed.send(&parent, &kid.session_id, "one more").await;
    let window = DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS;
    // Whole milliseconds since the first wake: at most the window, and at
    // least what is left of it after everything since the loop began.
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap() + 1;
    let in_window = matches!(
        refused.delivery_status,
        AgentMessageDeliveryStatus::WakeRefused { retry_after_ms }
            if retry_after_ms <= window && retry_after_ms + elapsed >= window
    );
    assert_eq!(
        (
            in_window,
            refused.delivered_at,
            refused.queued_at,
            kid.wakes.spent(&bed.parent)
        ),
        (true, None, None, DEFAULT_AGENT_MESSAGE_WAKE_BUDGET)
    );
    bed.close().await;
}

/// A message to a receiver in a turn queues behind it and spends no wake.
#[tokio::test]
async fn a_busy_receiver_queues_a_message_without_spending_a_wake() {
    let mut bed = bed(json!({
        "responses": [spawn_kid(HOLD_TASK), { "text": "WAITING" }, { "text": "PARENT DONE" }],
        "children": { "kid": [{ "text": "kid finished" }, { "text": "kid heard" }] },
    }))
    .await;
    bed.input("delegate").await;
    bed.until_done().await;
    // The child's model call waits: its turn is streaming.
    bed.hold.entered.notified().await;
    let parent = bed.node(&bed.parent);
    let kid = Arc::clone(&bed.kids().await["kid"]);
    let receipt = bed.send(&parent, &kid.session_id, "ping").await;
    assert_eq!(
        (receipt.delivery_status, kid.wakes.spent(&bed.parent)),
        (AgentMessageDeliveryStatus::Queued, 0)
    );
    bed.hold.release.add_permits(8);
    bed.close().await;
}

/// A client's follow-up (the CLI's `send --follow-up`) to a session in a
/// turn waits on its agent's follow-up queue, not its steers, spends no
/// sender's wake, and is answered as queued.
#[tokio::test]
async fn a_clients_follow_up_to_a_busy_session_queues_behind_its_turn() {
    let mut bed = bed(json!({
        "responses": [spawn_kid(HOLD_TASK), { "text": "WAITING" }, { "text": "PARENT DONE" }],
        "children": { "kid": [{ "text": "kid finished" }, { "text": "kid heard" }] },
    }))
    .await;
    bed.input("delegate").await;
    bed.until_done().await;
    bed.hold.entered.notified().await;
    let kid = Arc::clone(&bed.kids().await["kid"]);
    let receipt = bed
        .runtime
        .subagents()
        .send_message(
            &kid.session_id,
            "ping",
            MessageSender::Client("cli".to_string()),
            Delivery::FollowUp,
        )
        .await
        .unwrap();
    let engine = kid.engine().expect("the kid runs");
    let agent = engine.session.agent();
    assert_eq!(
        (
            receipt["deliveryStatus"].clone(),
            receipt["deliveryMode"].clone(),
            receipt["from"].clone(),
            agent.steering_previews().len(),
            agent.follow_up_previews().len(),
            kid.wakes.spent("cli"),
        ),
        (
            json!("queued"),
            json!("follow_up"),
            json!({ "clientId": "cli" }),
            0,
            1,
            0
        )
    );
    bed.hold.release.add_permits(8);
    bed.close().await;
}

/// A background command's end is published on the session's
/// `bash/done` topic with its row steered, and the settle's drive runs
/// that row as a follow-up turn.
#[tokio::test]
async fn a_background_commands_end_runs_a_follow_up_turn_through_its_topic() {
    let mut bed = bed(json!({
        "responses": [
            eval_cell("(hash-ref (bash \"sleep 1; echo hi\" (hash 'background #t)) 'running)"),
            { "text": "STARTED" },
            { "text": "SAW IT" },
        ],
        "children": {},
    }))
    .await;
    let input = bed.input("start it").await;
    let settled = bed
        .runtime
        .settle(&bed.parent, SettleAfter::Task(input))
        .await
        .unwrap()
        .expect("the run settled");
    let topic = format!("session/{}/bash/done", bed.parent);
    let notice = payload_on(&mut bed.listener, &topic).await;
    assert_eq!(
        (
            notice["taken"].clone(),
            notice["row"]["customType"].clone(),
            settled
                .terminal
                .primary
                .and_then(|primary| primary.stdout_text())
        ),
        (
            json!("steered"),
            json!("async_bash_completion"),
            Some("SAW IT".to_string())
        )
    );
    bed.close().await;
}

/// A wake-idle notice (`[bash-quiet]`) that reaches a session in a turn is
/// dropped: nothing is steered, and its topic says so.
#[tokio::test]
async fn a_wake_idle_notice_to_a_session_in_a_turn_is_dropped() {
    let mut bed = bed(json!({
        "responses": [spawn_kid(HOLD_TASK), { "text": "WAITING" }],
        "children": { "kid": [{ "text": "kid finished" }] },
    }))
    .await;
    bed.input("delegate").await;
    bed.until_done().await;
    bed.hold.entered.notified().await;
    let kid = Arc::clone(&bed.kids().await["kid"]);
    let row = create_async_bash_quiet_message(
        &[QuietJob {
            job: "job-1".to_string(),
            pid: 1,
            command: "sleep 600".to_string(),
            elapsed_ms: 180_000,
            quiet_ms: 180_000,
            output: String::new(),
            output_truncated: false,
        }],
        0,
    );
    let admission = kernel_notice_sink(&kid)(KernelNotice {
        row,
        preview_label: ASYNC_BASH_QUIET_PREVIEW_LABEL,
        delivery: NoticeDelivery::WakeIdle,
    });
    let topic = format!("session/{}/bash/quiet", kid.session_id);
    let notice = payload_on(&mut bed.listener, &topic).await;
    let queued = kid
        .engine()
        .is_some_and(|engine| engine.session.agent().has_queued_messages());
    assert_eq!(
        (admission, queued, notice["taken"].clone()),
        (NoticeAdmission::DroppedBusy, false, json!("dropped_busy"))
    );
    bed.hold.release.add_permits(8);
    bed.close().await;
}
