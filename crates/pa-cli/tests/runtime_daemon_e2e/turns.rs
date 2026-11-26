//! Interactive turn semantics on the runtime daemon (phase 7b), ported from
//! the old daemon's `steer_boundary_batch_e2e`, `queue_delivery_e2e` and
//! `abort_kernel_e2e` onto wire2, with the same expectations where the
//! runtime's turn model allows (see each test):
//!
//! - steers published while a kernel cell runs are delivered together at
//!   its tool boundary with one reply (steering mode `all`, the settings'
//!   default), and follow-ups run after it, one turn each, in order; each
//!   publish's reply carries the task `agent/<id>/accepted` announces;
//! - `session.abort` mid-cell settles the turn at once, the cell dies, and
//!   the queue parks with the unread steers back at its front until
//!   `session.resume_queue`; `session.abort_and_send_queued` keeps it going;
//! - `session.withdraw` and `session.restore` take queued input out and put
//!   it back;
//! - `session.compact` runs in the lane and the state queries read the
//!   session.

use std::time::{Duration, Instant};

use pa_types::event_log::Topic;
use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::{json, Value};

use super::{ok, turn_rows, Bed, Client};

/// The aborted turn's budget to settle: a wedged cell would hold it for its
/// whole sleep.
const ABORT_SETTLE: Duration = Duration::from_secs(20);

/// A scripted response that runs `code` as an `eval` cell.
pub(super) fn cell(text: &str, code: &str) -> Value {
    json!({ "content": [
        { "type": "text", "text": text },
        { "type": "toolCall", "name": "eval", "id": "toolu_cell01", "arguments": { "code": code } },
    ]})
}

/// A client attached to one open session's topics, keeping every frame.
pub(super) struct Attached {
    client: Client,
    pub(super) id: String,
    frames: Vec<ServerFrame>,
}

impl Attached {
    /// Creates a session in `cwd` and subscribes to its topics.
    pub(super) async fn open(bed: &Bed, cwd: &std::path::Path) -> Self {
        let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
        let created = ok(client.call("session.create", json!({ "cwd": cwd })).await);
        let id = created["session"]
            .as_str()
            .expect("a session id")
            .to_string();
        client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
        Attached {
            client,
            id,
            frames: Vec::new(),
        }
    }

    /// Attaches to open session `id`, replaying its stored events from the
    /// start of the log.
    pub(super) async fn attach(bed: &Bed, id: &str) -> Self {
        let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
        let (frames, _) = client
            .subscribe(&format!("agent/{id}/#"), Since::After { seq: 0 })
            .await;
        Attached {
            client,
            id: id.to_string(),
            frames,
        }
    }

    /// Publishes `text` on the session's input at `priority` (`None`: a
    /// prompt); the publish's reply.
    pub(super) async fn publish(&mut self, text: &str, priority: Option<&str>) -> Value {
        let mut payload = json!({ "text": text });
        if let Some(priority) = priority {
            payload["priority"] = json!(priority);
        }
        let request = self.client.id();
        self.client
            .send(&ClientFrame::Publish {
                id: request.clone(),
                topic: Topic::new(format!("agent/{}/input", self.id)).unwrap(),
                payload,
            })
            .await;
        ok(self.client.reply(&request, &mut self.frames).await)
    }

    pub(super) async fn call(&mut self, method: &str, args: Value) -> Value {
        let request = self.client.id();
        let mut args = args;
        args["session"] = json!(self.id);
        self.client
            .send(&ClientFrame::Call {
                id: request.clone(),
                method: method.to_string(),
                args,
            })
            .await;
        ok(self.client.reply(&request, &mut self.frames).await)
    }

    pub(super) async fn query(&mut self, name: &str) -> Value {
        let request = self.client.id();
        self.client
            .send(&ClientFrame::Query {
                id: request.clone(),
                name: name.to_string(),
                args: json!({ "session": self.id }),
            })
            .await;
        ok(self.client.reply(&request, &mut self.frames).await)
    }

    /// The payloads of the events on the session's `kind` topic so far.
    pub(super) fn payloads(&self, kind: &str) -> Vec<Value> {
        self.events(kind)
            .into_iter()
            .map(|event| event.event.payload)
            .collect()
    }

    /// The events on the session's `kind` topic so far.
    pub(super) fn events(&self, kind: &str) -> Vec<pa_types::event_log::Event> {
        let topic = format!("agent/{}/{kind}", self.id);
        self.frames
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event { event, .. } if event.event.topic.as_str() == topic => {
                    Some(event.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Reads frames until `seen` holds for what has arrived.
    pub(super) async fn until(&mut self, seen: impl Fn(&Self) -> bool) {
        while !seen(self) {
            let frame = self.client.frame().await;
            self.frames.push(frame);
        }
    }

    /// Reads frames until a stream event of `kind` has arrived.
    pub(super) async fn until_streamed(&mut self, kind: &str) {
        self.until(|session| {
            session
                .payloads("stream")
                .iter()
                .any(|event| event["type"] == kind)
        })
        .await;
    }

    /// Reads frames until task `task`'s `done`; its payload.
    pub(super) async fn done(&mut self, task: &Value) -> Value {
        self.until(|session| {
            session
                .payloads("done")
                .iter()
                .any(|done| done["task"] == *task)
        })
        .await;
        self.payloads("done")
            .into_iter()
            .find(|done| done["task"] == *task)
            .expect("the task's done")
    }

    /// The user and assistant texts of the session's transcript, in order.
    pub(super) async fn texts(&mut self) -> Vec<String> {
        let page = self.query("transcript.page").await;
        turn_rows(&page)
            .into_iter()
            .filter(|(_, role, _)| matches!(role.as_deref(), Some("user" | "assistant")))
            .filter_map(|(_, _, text)| text.filter(|text| !text.is_empty()))
            .collect()
    }

    /// How many turns started on the stream after its first `agent_end`.
    fn turns_after_the_first(&self) -> usize {
        let stream = self.payloads("stream");
        let first_end = stream
            .iter()
            .position(|event| event["type"] == "agent_end")
            .expect("the first turn ended");
        stream[first_end..]
            .iter()
            .filter(|event| event["type"] == "agent_start")
            .count()
    }
}

/// `steer_boundary_batch_e2e` and `queue_delivery_e2e`'s lane order: steers
/// parked mid-cell co-deliver as one batch with one reply, then the
/// follow-ups each run as a turn of their own, in order. The runtime's
/// running turn reads the steers at its tool boundary (the old daemon ended
/// the turn there and ran them as the next one), so the turns after the
/// first are the follow-ups'. Every publish is answered with its task and
/// admission, as `agent/<id>/accepted` announces them, and waiting for a
/// prompt is waiting for its task's `done`.
#[tokio::test(flavor = "multi_thread")]
async fn steers_parked_mid_cell_co_deliver_and_follow_ups_run_after_one_turn_each() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("Running the sleep cell.", "(bash \"sleep 3\" (hash 'wait-ms #f))\n(displayln \"slept\")"),
        "batch reply for all three steers",
        "follow one reply",
        "follow two reply",
        "follow three reply",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let first = session.publish("run the sleep", None).await;
    session.until_streamed("tool_execution_start").await;
    let mut replies = Vec::new();
    for steer in ["steer one", "steer two", "steer three"] {
        replies.push(session.publish(steer, Some("steer")).await);
    }
    for follow_up in ["follow one", "follow two", "follow three"] {
        replies.push(session.publish(follow_up, Some("follow_up")).await);
    }
    let last = replies.last().unwrap()["task"].clone();
    assert_eq!(session.done(&last).await["outcome"], "done");
    assert_eq!(session.done(&first["task"]).await["outcome"], "done");

    let admissions: Vec<&Value> = replies.iter().map(|reply| &reply["admission"]).collect();
    let accepted: Vec<Value> = session
        .payloads("accepted")
        .into_iter()
        .map(|accepted| json!({ "task": accepted["task"], "admission": accepted["admission"] }))
        .collect();
    let answered: Vec<Value> = std::iter::once(&first)
        .chain(&replies)
        .map(|reply| json!({ "task": reply["task"], "admission": reply["admission"] }))
        .collect();
    assert_eq!(
        (admissions, accepted),
        (
            vec![
                &json!("steered"),
                &json!("steered"),
                &json!("steered"),
                &json!("queued"),
                &json!("queued"),
                &json!("queued"),
            ],
            answered
        )
    );
    assert_eq!(
        (session.texts().await, session.turns_after_the_first()),
        (
            [
                "run the sleep",
                "Running the sleep cell.",
                "steer one",
                "steer two",
                "steer three",
                "batch reply for all three steers",
                "follow one",
                "follow one reply",
                "follow two",
                "follow two reply",
                "follow three",
                "follow three reply",
            ]
            .map(str::to_string)
            .to_vec(),
            3
        )
    );
}

/// The script of the abort tests: a cell that would sleep for minutes, then
/// the replies of the turns after the abort.
fn wedge_script() -> Value {
    json!({ "responses": [
        cell(
            "Running the wedge cell.",
            "(bash \"touch wedge-started; sleep 300; touch wedge-finished\" (hash 'wait-ms #f))\n(displayln \"cell completed\")",
        ),
        "batch reply to the steers",
        "follow-up reply",
    ]})
}

/// `abort_kernel_e2e`, with the old daemon's parked queue: an abort mid-cell
/// settles the turn at once, the cell dies, and the queue parks with the
/// steers the turn had not read back at its front; resuming it delivers
/// them together as one turn, then the follow-up.
#[tokio::test(flavor = "multi_thread")]
async fn abort_settles_a_cell_at_once_and_parks_the_queue_until_it_is_resumed() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&wedge_script());
    let mut session = Attached::open(&bed, &cwd).await;
    let first = session.publish("run the wedge cell", None).await;
    session.until_streamed("tool_execution_start").await;
    let steer = session.publish("steer A", Some("steer")).await;
    session.publish("steer B", Some("steer")).await;
    let follow_up = session.publish("follow F", Some("follow_up")).await;

    let aborted_at = Instant::now();
    let aborted = session.call("session.abort", json!({})).await;
    let ended = session.done(&first["task"]).await;
    assert!(
        aborted_at.elapsed() < ABORT_SETTLE,
        "the aborted turn took {:?} to settle",
        aborted_at.elapsed()
    );
    let tasks = session.query("tasks").await;
    let queued: Vec<&Value> = tasks["queued"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| &task["text"])
        .collect();
    assert_eq!(
        (
            ended["outcome"].clone(),
            aborted["requeued"].clone(),
            aborted["parked"].clone(),
            queued,
            tasks["running"].clone(),
        ),
        (
            json!("cancelled"),
            json!(2),
            json!(true),
            vec![&json!("steer A"), &json!("steer B"), &json!("follow F")],
            Value::Null,
        )
    );

    let resumed = session.call("session.resume_queue", json!({})).await;
    assert_eq!(resumed["resumed"], true);
    assert_eq!(session.done(&steer["task"]).await["outcome"], "done");
    assert_eq!(session.done(&follow_up["task"]).await["outcome"], "done");
    let texts = session.texts().await;
    let from = texts.iter().position(|text| text == "steer A").unwrap();
    assert_eq!(
        (&texts[from..], cwd.join("wedge-finished").exists()),
        (
            &[
                "steer A",
                "steer B",
                "batch reply to the steers",
                "follow F",
                "follow-up reply"
            ]
            .map(str::to_string)[..],
            false
        )
    );
}

/// `abort_and_send_queued`: the abort keeps the queue going, so the steers
/// the aborted turn had not read run next, delivered together.
#[tokio::test(flavor = "multi_thread")]
async fn abort_and_send_queued_runs_the_unread_steers_as_the_next_turn() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&wedge_script());
    let mut session = Attached::open(&bed, &cwd).await;
    let first = session.publish("run the wedge cell", None).await;
    session.until_streamed("tool_execution_start").await;
    let steer = session.publish("steer A", Some("steer")).await;
    session.publish("steer B", Some("steer")).await;

    let aborted = session
        .call("session.abort_and_send_queued", json!({}))
        .await;
    assert_eq!(session.done(&first["task"]).await["outcome"], "cancelled");
    assert_eq!(session.done(&steer["task"]).await["outcome"], "done");
    let texts = session.texts().await;
    let from = texts.iter().position(|text| text == "steer A").unwrap();
    assert_eq!(
        (aborted["parked"].clone(), &texts[from..]),
        (
            json!(false),
            &["steer A", "steer B", "batch reply to the steers"].map(str::to_string)[..]
        )
    );
}

/// Queue withdraw and restore: the queued follow-ups come back out in order
/// and never run as their own turns; restored, they are steers the running
/// turn reads at its next step, together.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_input_comes_back_in_order_and_restored_input_steers_the_turn() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": [
        cell("Running the sleep cell.", "(bash \"sleep 3\" (hash 'wait-ms #f))\n(displayln \"slept\")"),
        "reply to the restored input",
    ]}));
    let mut session = Attached::open(&bed, &cwd).await;
    let first = session.publish("run the sleep", None).await;
    session.until_streamed("tool_execution_start").await;
    let follow_x = session.publish("follow X", Some("follow_up")).await;
    session.publish("follow Y", Some("follow_up")).await;

    let withdrawn = session.call("session.withdraw", json!({})).await;
    // A withdrawn task ends with its `done`, as a prompt-and-wait waits for.
    assert_eq!(
        session.done(&follow_x["task"]).await["outcome"],
        "cancelled"
    );
    let texts: Vec<Value> = withdrawn["withdrawn"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["text"].clone())
        .collect();
    let restored = session
        .call("session.restore", json!({ "messages": texts }))
        .await;
    assert_eq!(session.done(&first["task"]).await["outcome"], "done");
    let tasks = session.query("tasks").await;
    let admissions: Vec<&Value> = restored["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| &task["admission"])
        .collect();
    assert_eq!(
        (
            texts,
            admissions,
            tasks["queued"].clone(),
            session.texts().await
        ),
        (
            vec![json!("follow X"), json!("follow Y")],
            vec![&json!("steered"), &json!("steered")],
            json!([]),
            [
                "run the sleep",
                "Running the sleep cell.",
                "follow X",
                "follow Y",
                "reply to the restored input",
            ]
            .map(str::to_string)
            .to_vec()
        )
    );
}

/// `session.compact` runs in the session's lane and answers with its task's
/// outcome and the `compaction_end` it streamed (a one-turn session is too
/// short to compact: the old daemon's skip, a warning); the state queries read the
/// session's context, usage, tree, context tree and breakdown.
#[tokio::test(flavor = "multi_thread")]
async fn compact_answers_in_the_lane_and_the_state_queries_read_the_session() {
    let bed = Bed::new();
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["first answer"] }));
    let mut session = Attached::open(&bed, &cwd).await;
    let first = session.publish("say something", None).await;
    session.done(&first["task"]).await;

    let compacted = session.call("session.compact", json!({})).await;
    assert_eq!(
        (
            compacted["outcome"].clone(),
            compacted["compaction"]["type"].clone(),
            compacted["compaction"]["errorMessage"].clone(),
            compacted["compaction"]["errorSeverity"].clone(),
        ),
        (
            json!("done"),
            json!("compaction_end"),
            json!("Session is too short to compact — try again once it grows"),
            json!("warning"),
        )
    );

    let context = session.query("session.context").await;
    let usage = session.query("session.usage").await;
    let tree = session.query("session.tree").await;
    let context_tree = session.query("session.context_tree").await;
    let breakdown = session.query("session.breakdown").await;
    let roles: Vec<&Value> = context["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| &message["role"])
        .filter(|role| *role == "user" || *role == "assistant")
        .collect();
    let leaf = tree["nodes"]
        .as_array()
        .unwrap()
        .last()
        .map(|node| node["id"].clone());
    assert_eq!(
        (
            roles,
            usage["messages"].clone(),
            usage["contextUsage"]["contextWindow"].is_u64(),
            leaf,
            context_tree["label"].clone(),
            breakdown["compaction"]["count"].clone(),
            breakdown["categories"].is_object(),
        ),
        (
            vec![&json!("user"), &json!("assistant")],
            json!({ "user": 1, "assistant": 1, "toolCalls": 0, "toolResults": 0 }),
            true,
            Some(tree["leafId"].clone()),
            json!("main agent"),
            json!(0),
            true,
        )
    );
}
