//! The scripted supervisor the executor tests run against: it answers the
//! four host requests a run makes (`rlm.run`, `rlm.collect`,
//! `rlm.delete_subagent`, `factory.progress`) from outcomes a test sets per
//! node or per child, and a temporary agent directory and harness store.
//!
//! A child's name carries its node (`sw-<node>-<run>-...`, or
//! `<run6>-<configured name>`), so node ids in these tests hold no hyphen.
//! Like the real host, a collect of children that have not settled waits
//! out its timeout (or until an outcome changes) before answering.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};

use super::executor::Executor;
use super::report::Detail;
use super::store::FactoryEnvironment;
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::refinement::{update_harness_state, HarnessEntry, HarnessScope, RefinementKind};

/// What a collect reports for a child.
#[derive(Clone)]
pub(super) struct Outcome {
    status: &'static str,
    answer: Option<String>,
    error: Option<String>,
}

impl Outcome {
    pub(super) fn done(answer: &str) -> Self {
        Outcome {
            status: "done",
            answer: Some(answer.to_string()),
            error: None,
        }
    }

    /// Settled done, its answer not handed off yet.
    pub(super) fn done_without_answer() -> Self {
        Outcome {
            status: "done",
            answer: None,
            error: None,
        }
    }

    /// Never settles.
    pub(super) fn running() -> Self {
        Outcome {
            status: "running",
            answer: None,
            error: None,
        }
    }

    pub(super) fn error(reason: &str) -> Self {
        Outcome {
            status: "error",
            answer: None,
            error: Some(reason.to_string()),
        }
    }

    /// An exit that left its last words.
    pub(super) fn error_with_answer(reason: &str, answer: &str) -> Self {
        Outcome {
            status: "error",
            answer: Some(answer.to_string()),
            error: Some(reason.to_string()),
        }
    }
}

#[derive(Default)]
struct HostState {
    calls: Vec<(String, Value)>,
    notices: Vec<Value>,
    /// Live children: id, name, node.
    children: Vec<(String, String, String)>,
    spawned: usize,
    outcomes: HashMap<String, Outcome>,
    child_outcomes: HashMap<String, Outcome>,
    /// Outcomes a child reports once each before its scripted one.
    first_outcomes: HashMap<String, Outcome>,
    rate_limit_first: HashMap<String, usize>,
    failing: HashSet<&'static str>,
    gates: HashMap<(String, usize), Arc<tokio::sync::Semaphore>>,
    entered: HashMap<(String, usize), Arc<tokio::sync::Semaphore>>,
    call_numbers: HashMap<String, usize>,
    spawn_takes: Duration,
}

/// Gives `abandoned` a permit when a collect is dropped before it answered:
/// the task waiting on it was aborted.
struct CollectInFlight {
    abandoned: Arc<tokio::sync::Semaphore>,
    answered: bool,
}

impl Drop for CollectInFlight {
    fn drop(&mut self) {
        if !self.answered {
            self.abandoned.add_permits(1);
        }
    }
}

pub(super) struct FakeHost {
    state: Mutex<HostState>,
    changed: tokio::sync::Notify,
    /// One permit per collect abandoned mid-wait.
    pub(super) abandoned_collects: Arc<tokio::sync::Semaphore>,
}

impl Default for FakeHost {
    fn default() -> Self {
        FakeHost {
            state: Mutex::default(),
            changed: tokio::sync::Notify::new(),
            abandoned_collects: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}

/// The node a spawn name belongs to.
fn node_of_name(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts[0] == "sw" && parts.len() > 2 {
        return parts[1].to_string();
    }
    if parts[0].len() == 6 && parts[0].chars().all(|digit| digit.is_ascii_hexdigit()) {
        let mut base = &parts[1..];
        while base.len() > 1
            && base.last().is_some_and(|part| {
                part.len() > 1
                    && (part.starts_with('i') || part.starts_with('a'))
                    && part[1..].chars().all(|digit| digit.is_ascii_digit())
            })
        {
            base = &base[..base.len() - 1];
        }
        return base.join("-");
    }
    name.to_string()
}

impl FakeHost {
    pub(super) fn set_node(&self, node: &str, outcome: Outcome) {
        self.state
            .lock()
            .unwrap()
            .outcomes
            .insert(node.to_string(), outcome);
        self.changed.notify_waiters();
    }

    pub(super) fn set_child(&self, child: &str, outcome: Outcome) {
        self.state
            .lock()
            .unwrap()
            .child_outcomes
            .insert(child.to_string(), outcome);
        self.changed.notify_waiters();
    }

    /// The child reports `outcome` on its first collect only.
    pub(super) fn set_child_first(&self, child: &str, outcome: Outcome) {
        self.state
            .lock()
            .unwrap()
            .first_outcomes
            .insert(child.to_string(), outcome);
    }

    /// The node's first `count` admissions fail with a rate limit.
    pub(super) fn rate_limit(&self, node: &str, count: usize) {
        self.state
            .lock()
            .unwrap()
            .rate_limit_first
            .insert(node.to_string(), count);
    }

    /// Every request of this type fails.
    pub(super) fn fail(&self, request_type: &'static str) {
        self.state.lock().unwrap().failing.insert(request_type);
    }

    /// Every spawn takes this long.
    pub(super) fn spawns_take(&self, duration: Duration) {
        self.state.lock().unwrap().spawn_takes = duration;
    }

    /// Hold the `number`-th request of a type (1-based) until the returned
    /// gate gets a permit; the second semaphore gets one when the request
    /// arrives.
    pub(super) fn gate(
        &self,
        request_type: &str,
        number: usize,
    ) -> (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>) {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let mut state = self.state.lock().unwrap();
        let key = (request_type.to_string(), number);
        state.gates.insert(key.clone(), Arc::clone(&gate));
        state.entered.insert(key, Arc::clone(&entered));
        (gate, entered)
    }

    pub(super) fn calls_of(&self, request_type: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(kind, _)| kind == request_type)
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    /// The prompts a node's children were spawned with, in order.
    pub(super) fn prompts(&self, node: &str) -> Vec<String> {
        self.spawns(node)
            .iter()
            .map(|payload| payload["prompt"].as_str().unwrap().to_string())
            .collect()
    }

    /// A node's spawn requests, in order.
    pub(super) fn spawns(&self, node: &str) -> Vec<Value> {
        self.calls_of("rlm.run")
            .into_iter()
            .filter(|payload| node_of_name(payload["kwargs"]["name"].as_str().unwrap()) == node)
            .collect()
    }

    pub(super) fn deleted(&self) -> Vec<String> {
        self.calls_of("rlm.delete_subagent")
            .iter()
            .map(|payload| payload["target"].as_str().unwrap().to_string())
            .collect()
    }

    pub(super) fn notice_kinds(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .notices
            .iter()
            .map(|notice| notice["kind"].as_str().unwrap().to_string())
            .collect()
    }

    pub(super) fn notices(&self) -> Vec<Value> {
        self.state.lock().unwrap().notices.clone()
    }

    fn collected(&self, targets: &[Value]) -> Vec<Value> {
        let mut state = self.state.lock().unwrap();
        let mut results = Vec::new();
        for target in targets.iter().filter_map(Value::as_str) {
            // A deleted child vanishes from collect results.
            let Some((_, name, node)) = state
                .children
                .iter()
                .find(|(child, _, _)| child == target)
                .cloned()
            else {
                continue;
            };
            let outcome = state
                .first_outcomes
                .remove(target)
                .or_else(|| state.child_outcomes.get(target).cloned())
                .or_else(|| state.outcomes.get(&node).cloned())
                .unwrap_or_else(|| Outcome::done(&format!("answer-{node}")));
            let mut row = json!({
                "rlm_child_id": target,
                "session_name": name,
                "session_dir": format!("/tmp/{target}"),
                "status": outcome.status,
                "settled": outcome.status != "running",
                "tool_use_count": 1,
                "duration_ms": 5,
            });
            if let Some(answer) = &outcome.answer {
                // The roster preview is compact; the full text binds.
                let compact = answer.split_whitespace().collect::<Vec<_>>().join(" ");
                row["answer_preview"] = json!(if compact.chars().count() > 160 {
                    format!("{}...", compact.chars().take(157).collect::<String>())
                } else {
                    compact
                });
                row["answer_text"] = json!(answer);
            }
            if let Some(error) = &outcome.error {
                row["error"] = json!(error);
            }
            results.push(row);
        }
        results
    }

    async fn answer(&self, request_type: &str, payload: Value) -> anyhow::Result<Value> {
        let (gate, takes) = {
            let mut state = self.state.lock().unwrap();
            let number = state
                .call_numbers
                .entry(request_type.to_string())
                .or_default();
            *number += 1;
            let key = (request_type.to_string(), *number);
            if let Some(entered) = state.entered.remove(&key) {
                entered.add_permits(1);
            }
            (state.gates.remove(&key), state.spawn_takes)
        };
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        match request_type {
            "rlm.run" => {
                if !takes.is_zero() {
                    tokio::time::sleep(takes).await;
                }
                let mut state = self.state.lock().unwrap();
                let name = payload["kwargs"]["name"].as_str().unwrap().to_string();
                let node = node_of_name(&name);
                // The supervisor rejects duplicate sibling names.
                if state.children.iter().any(|(_, known, _)| *known == name) {
                    return Err(anyhow!(
                        "Agent name \"{name}\" is unavailable: an agent of that name already exists at depth 1 under this parent"
                    ));
                }
                let attempted = state
                    .calls
                    .iter()
                    .filter(|(kind, earlier)| {
                        kind == "rlm.run"
                            && node_of_name(earlier["kwargs"]["name"].as_str().unwrap()) == node
                    })
                    .count();
                state.calls.push((request_type.to_string(), payload));
                if attempted < state.rate_limit_first.get(&node).copied().unwrap_or(0) {
                    return Err(anyhow!("429 rate limit exceeded"));
                }
                state.spawned += 1;
                let child = format!("child-{}", state.spawned);
                state.children.push((child.clone(), name.clone(), node));
                Ok(json!({
                    "rlm_child_id": child,
                    "name": name,
                    "session_dir": format!("/tmp/{child}"),
                    "model": "fake-model",
                }))
            }
            "rlm.collect" => {
                let fails = {
                    let mut state = self.state.lock().unwrap();
                    state
                        .calls
                        .push((request_type.to_string(), payload.clone()));
                    state.failing.contains("rlm.collect")
                };
                if fails {
                    return Err(anyhow!("collect: the bridge is gone"));
                }
                let mut in_flight = CollectInFlight {
                    abandoned: Arc::clone(&self.abandoned_collects),
                    answered: false,
                };
                let targets = payload["targets"].as_array().unwrap().clone();
                let deadline = tokio::time::Instant::now()
                    + Duration::from_millis(payload["timeout_ms"].as_u64().unwrap());
                loop {
                    let changed = self.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let results = self.collected(&targets);
                    let settled = results.iter().any(|row| row["settled"] == true);
                    if settled || tokio::time::Instant::now() >= deadline {
                        in_flight.answered = true;
                        return Ok(json!({ "results": results }));
                    }
                    // Either an outcome changed or the wait ran out.
                    let _ = tokio::time::timeout_at(deadline, changed).await;
                }
            }
            "rlm.delete_subagent" => {
                let mut state = self.state.lock().unwrap();
                state
                    .calls
                    .push((request_type.to_string(), payload.clone()));
                if state.failing.contains("rlm.delete_subagent") {
                    return Err(anyhow!("delete_subagent: child already gone"));
                }
                let target = payload["target"].as_str().unwrap();
                state.children.retain(|(child, _, _)| child != target);
                Ok(json!({
                    "subagent": {"rlm_child_id": target, "status": "cancelled"},
                    "outcome": "deleted",
                }))
            }
            "factory.progress" => {
                let mut state = self.state.lock().unwrap();
                if state.failing.contains("factory.progress") {
                    return Err(anyhow!("bridge dead"));
                }
                state.notices.push(payload);
                Ok(json!({}))
            }
            other => panic!("unexpected host request type {other}"),
        }
    }

    pub(super) fn handlers(self: &Arc<Self>) -> HostRequestHandlers {
        let mut handlers = HostRequestHandlers::new();
        for request_type in [
            "rlm.run",
            "rlm.collect",
            "rlm.delete_subagent",
            "factory.progress",
        ] {
            let host = Arc::clone(self);
            handlers.register(
                request_type,
                host_handler(move |payload: HostRequestPayload| {
                    let host = Arc::clone(&host);
                    async move { host.answer(request_type, payload.data).await }
                }),
            );
        }
        handlers
    }
}

/// An executor over a scripted host, an agent directory with the factory
/// enabled, and a harness store holding the `worker` subagent.
pub(super) struct Fixture {
    temp: tempfile::TempDir,
    pub(super) host: Arc<FakeHost>,
    pub(super) executor: Executor,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeHost::default());
        std::fs::create_dir_all(temp.path().join("work")).unwrap();
        let fixture = Fixture {
            executor: Executor::new(
                host.handlers(),
                FactoryEnvironment {
                    agent_dir: Some(temp.path().join("agent")),
                    local_harness_dir: Some(temp.path().join("harness")),
                    machines_dir: None,
                },
                temp.path().join("work"),
            ),
            host,
            temp,
        };
        fixture.settings(&json!({"factory": {"enabled": true}}));
        fixture.entry(
            RefinementKind::Subagent,
            "worker",
            "Worker",
            "Do the work carefully.",
            json!({}),
            json!({}),
        );
        fixture
    }

    pub(super) fn agent_dir(&self) -> PathBuf {
        self.temp.path().join("agent")
    }

    pub(super) fn work_dir(&self) -> PathBuf {
        self.temp.path().join("work")
    }

    /// A Scheme kernel provisioner over this fixture's host and stores.
    pub(super) fn provisioner(&self) -> SchemeKernelProvisioner {
        SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: self.host.handlers(),
            cwd: self.work_dir(),
            agent_dir: Some(self.agent_dir()),
            // Its `harness` directory is the session's local store.
            artifact_dir: Some(self.temp.path().to_path_buf()),
            ..SchemeKernelOptions::default()
        })
    }

    /// Write the agent directory's settings document.
    pub(super) fn settings(&self, document: &Value) {
        std::fs::create_dir_all(self.agent_dir()).unwrap();
        std::fs::write(self.agent_dir().join("settings.json"), document.to_string()).unwrap();
    }

    /// Store one harness entry in the session's local store.
    pub(super) fn entry(
        &self,
        kind: RefinementKind,
        id: &str,
        title: &str,
        content: &str,
        arguments: Value,
        metadata: Value,
    ) {
        let entry = HarnessEntry {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            content: content.to_string(),
            path: format!("{id}.md"),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::new(),
            arguments: serde_json::from_value(arguments).unwrap(),
            metadata: serde_json::from_value(metadata).unwrap(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            version: 1,
        };
        update_harness_state(
            &self.temp.path().join("harness"),
            HarnessScope::Local,
            |state| {
                state
                    .entries
                    .entry(kind)
                    .or_default()
                    .insert(id.to_string(), entry);
            },
        )
        .unwrap();
    }

    /// Store a factory entry `sw` carrying `spec` under `key` (`dag` or
    /// `machine`), and start a run of it.
    pub(super) async fn start(&self, key: &str, spec: Value) -> Value {
        self.store("sw", key, spec);
        self.executor.run("sw", None).await.unwrap()
    }

    pub(super) fn store(&self, id: &str, key: &str, spec: Value) {
        self.entry(
            RefinementKind::Factory,
            id,
            "Factory",
            "Factory content",
            Value::Object([(key.to_string(), spec)].into_iter().collect()),
            json!({}),
        );
    }

    /// Follow the run until it leaves `running`, and return its status.
    pub(super) async fn settle(&self, started: &Value) -> Value {
        self.until(started, |graph| graph["state"] != "running")
            .await;
        self.executor
            .status(started["run_id"].as_str().unwrap())
            .unwrap()
    }

    /// Follow the run until `reached` holds for its graph snapshot.
    pub(super) async fn until(&self, started: &Value, reached: impl Fn(&Value) -> bool) -> Value {
        let run_id = started["run_id"].as_str().unwrap();
        for _ in 0..100_000 {
            let snapshot = self.executor.graph(Some(run_id), Detail::Full).unwrap();
            if reached(&snapshot) {
                return snapshot;
            }
            // A change wakes the watch; the short timeout only covers a
            // change that landed between the look above and this wait.
            self.executor
                .watch(run_id, 0.2, Detail::Full)
                .await
                .unwrap();
        }
        panic!("run {run_id} never reached the awaited shape");
    }
}

/// One state's report in a status or graph payload.
pub(super) fn node<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id)
        .unwrap()
}

/// The ledger events of one kind in a status or graph payload.
pub(super) fn events<'a>(status: &'a Value, kind: &str) -> Vec<&'a Value> {
    status["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .collect()
}

/// One field of every instance row of a state.
pub(super) fn instance_column(status: &Value, id: &str, column: &str) -> Vec<Value> {
    node(status, id)["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[column].clone())
        .collect()
}
