//! The factory executor: runs canonical machines through the session's
//! supervisor.
//!
//! Ownership is split. The supervisor owns the children: a state is
//! admitted with the `rlm.run` host request, settled through `rlm.collect`,
//! and cancelled with `rlm.delete_subagent`. The executor owns the run
//! state, in this process's memory.
//!
//! A run progresses while no cell is executing: `run` and `resume` enter
//! the ready states, admit what fits, and return; a task on the session's
//! async runtime (the control loop) then drives the run to quiescence,
//! polling the children in bounded slices. That task belongs to the
//! session's kernel: when the kernel is killed its loops are aborted and
//! the runs are gone, while the children stay with the supervisor
//! (`rlm-list-subagents` still sees them).
//!
//! A run is locked only between two waits, never across one, so a stop, a
//! resume or a status read lands promptly while the loop waits on the
//! host. Every wait is therefore followed by a fresh look at the run: a
//! spawn that lands after a stop is retracted, a settle for an instance a
//! stop already cancelled is dropped, and a loop whose generation a resume
//! replaced exits.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};
use tokio::time::Instant;

use super::library::{self, Overwrite, ResolutionError};
use super::naming::spawn_label;
use super::python_state::{python_outputs, PythonCall, PYTHON_STATE_TIMEOUT_MS_DEFAULT};
use super::python_text::repr_str;
use super::report::{cap_frame, spec_snapshot, wire_payload, Detail};
use super::run::{
    capped, About, Place, Prepared, ResolvedSubagent, Run, RunState, Status, ANSWER_BINDING_CAP,
    ANSWER_CAPTURE_CAP,
};
use super::spec::{canonicalize_factory_spec, field};
use super::store::{entry_spec, FactoryEnvironment};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};

/// How long each control-loop `rlm.collect` waits for unsettled children.
pub(super) const POLL_TIMEOUT: Duration = Duration::from_millis(2000);
/// Upper bound on one watch.
const WATCH_TIMEOUT_CAP_SECONDS: f64 = 60.0;
/// The newest terminal runs an all-runs graph carries (every live run
/// reports regardless).
const GRAPH_RUNS_WINDOW: usize = 20;
/// Consecutive rate-limited admissions before the node fails.
const BACKOFF_MAX_ATTEMPTS: u32 = 5;
const BACKOFF_CAP_SECONDS: u64 = 60;
const RATE_LIMIT_MARKERS: [&str; 8] = [
    "rate limit",
    "rate-limit",
    "ratelimit",
    "429",
    "too many requests",
    "throttled",
    "quota",
    "usage limit",
];
const INFRA_ERROR_MARKERS: [&str; 2] = ["child worker unreachable", "supervisor link"];
/// The actions of the daemon and TUI lane over one run.
const ACTIVITY_ACTIONS: [&str; 6] = ["graph", "status", "watch", "run", "stop", "resume"];
const ACTIVITY_TIMEOUT_MS_CAP: u64 = 60_000;

/// The host reports admission failures as error text.
fn is_rate_limit_error(message: &str) -> bool {
    let lowered = message.to_lowercase();
    RATE_LIMIT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// A worker death or a lost supervisor link: infra, not the task's failure.
pub(super) fn is_infra_error(message: &str) -> bool {
    let lowered = message.to_lowercase();
    INFRA_ERROR_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

pub(super) type RunCell = Arc<Mutex<Run>>;

pub(super) fn locked(cell: &RunCell) -> MutexGuard<'_, Run> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One child's row of an `rlm.collect` reply.
#[derive(Clone)]
pub(super) struct Collected {
    pub(super) child_id: String,
    pub(super) status: String,
    pub(super) settled: bool,
    /// The full final answer (else the roster preview), capped; `None`
    /// when empty.
    pub(super) answer: Option<String>,
    pub(super) error: Option<String>,
    pub(super) duration_ms: Option<u64>,
    pub(super) tool_use_count: u64,
}

/// What admitting one instance came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Admitted,
    /// Rate limited: retried by the control loop.
    Deferred,
    /// The failure policy owns the run state now.
    Failed,
    /// The run left `running` while the spawn was in flight.
    Stopped,
}

struct Shared {
    handlers: HostRequestHandlers,
    environment: FactoryEnvironment,
    /// The session's working directory: where relative paths resolve and
    /// python states run.
    cwd: PathBuf,
    /// Every run this kernel hosted, in start order.
    runs: Mutex<Vec<RunCell>>,
    loops: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// One session kernel's factory executor. Clones share the registry.
#[derive(Clone)]
pub(in crate::kernel::scheme) struct Executor {
    shared: Arc<Shared>,
}

impl Executor {
    pub(in crate::kernel::scheme) fn new(
        handlers: HostRequestHandlers,
        environment: FactoryEnvironment,
        cwd: PathBuf,
    ) -> Self {
        Executor {
            shared: Arc::new(Shared {
                handlers,
                environment,
                cwd,
                runs: Mutex::new(Vec::new()),
                loops: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(super) fn environment(&self) -> &FactoryEnvironment {
        &self.shared.environment
    }

    /// A user-given path, relative ones against the session's directory.
    pub(super) fn session_path(&self, path: &str) -> PathBuf {
        self.shared.cwd.join(path)
    }

    /// End every control loop: the kernel that owned the runs is gone.
    /// The children stay with the supervisor.
    pub(in crate::kernel::scheme) fn shutdown(&self) {
        for control_loop in self
            .shared
            .loops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            control_loop.abort();
        }
    }

    // -- host requests -----------------------------------------------------

    pub(super) async fn host(
        &self,
        request_type: &str,
        mut payload: Value,
    ) -> anyhow::Result<Value> {
        let handler = self
            .shared
            .handlers
            .get(request_type)
            .ok_or_else(|| anyhow!("unknown host request: {request_type}"))?
            .clone();
        payload["type"] = json!(request_type);
        handler(HostRequestPayload { data: payload }).await
    }

    pub(super) async fn collect(
        &self,
        targets: &[String],
        timeout: Duration,
    ) -> anyhow::Result<Vec<Collected>> {
        let reply = self
            .host(
                "rlm.collect",
                json!({"targets": targets, "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)}),
            )
            .await?;
        Ok(reply["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let text = |key: &str| row[key].as_str().filter(|text| !text.is_empty());
                Some(Collected {
                    child_id: row["rlm_child_id"].as_str()?.to_string(),
                    status: row["status"].as_str().unwrap_or_default().to_string(),
                    settled: row["settled"].as_bool().unwrap_or(false),
                    answer: text("answer_text")
                        .or_else(|| text("answer_preview"))
                        .map(|answer| capped(answer, ANSWER_BINDING_CAP)),
                    error: text("error").map(str::to_string),
                    duration_ms: row["duration_ms"].as_u64(),
                    tool_use_count: row["tool_use_count"].as_u64().unwrap_or(0),
                })
            })
            .collect())
    }

    pub(super) async fn delete_child(&self, child_id: &str) -> anyhow::Result<Value> {
        self.host("rlm.delete_subagent", json!({"target": child_id}))
            .await
    }

    // -- registry ----------------------------------------------------------

    fn find_run(&self, run_id: &str) -> Option<RunCell> {
        self.shared
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|cell| locked(cell).id == run_id)
            .cloned()
    }

    fn require_run(&self, run_id: &str) -> anyhow::Result<RunCell> {
        self.find_run(run_id)
            .ok_or_else(|| anyhow!("unknown factory run {}", repr_str(run_id)))
    }

    /// Resolve every state's subagent reference, collecting all failures.
    /// A string reference is a harness subagent entry id or title: its
    /// content is the prompt template and its metadata's `model` and
    /// `thinking` are spawn settings. An inline object uses its own fields.
    fn resolve_subagents(
        &self,
        canonical: &Value,
    ) -> anyhow::Result<HashMap<String, ResolvedSubagent>> {
        let mut resolved = HashMap::new();
        let mut errors: Vec<String> = Vec::new();
        for state in canonical["states"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter(|state| !state.contains_key("python"))
        {
            let state_id = field(state, "id").as_str().unwrap_or_default();
            let quoted = repr_str(state_id);
            let (prompt, name, model, thinking) = match field(state, "subagent") {
                Value::Object(inline) => (
                    field(inline, "prompt").clone(),
                    field(inline, "name")
                        .as_str()
                        .map(|name| name.trim().to_string()),
                    field(inline, "model").clone(),
                    field(inline, "thinking").clone(),
                ),
                reference => {
                    let reference = reference.as_str().unwrap_or_default();
                    let Some(entry) = self.shared.environment.subagent_entry(reference) else {
                        errors.push(format!(
                            "state {quoted} references unknown subagent {}",
                            repr_str(reference)
                        ));
                        continue;
                    };
                    (
                        json!(entry.content),
                        None,
                        field(&entry.metadata, "model").clone(),
                        field(&entry.metadata, "thinking").clone(),
                    )
                }
            };
            let Some(prompt) = prompt.as_str().filter(|prompt| !prompt.trim().is_empty()) else {
                errors.push(format!("state {quoted} has an empty subagent prompt"));
                continue;
            };
            let setting = |key: &str, value: &Value| match value {
                Value::Null => Ok(None),
                Value::String(text) if !text.trim().is_empty() => Ok(Some(text.clone())),
                _ => Err(format!(
                    "state {quoted} subagent {key} must be a non-empty string when provided"
                )),
            };
            match (setting("model", &model), setting("thinking", &thinking)) {
                (Ok(model), Ok(thinking)) => {
                    resolved.insert(
                        state_id.to_string(),
                        (
                            prompt.to_string(),
                            name.filter(|name| !name.is_empty()),
                            model,
                            thinking,
                        ),
                    );
                }
                (Err(error), _) | (Ok(_), Err(error)) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(resolved)
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }

    // -- public API --------------------------------------------------------

    /// Validate a stored factory entry's spec and start a run of it;
    /// `None` when no entry has that id. The spec is validated and
    /// canonicalized again and every subagent reference resolved first;
    /// any failure starts nothing.
    async fn run_stored(
        &self,
        spec_id: &str,
        name: Option<String>,
    ) -> anyhow::Result<Option<Value>> {
        let Some(entry) = self.shared.environment.factory_entry(spec_id) else {
            return Ok(None);
        };
        let canonical =
            canonicalize_factory_spec(&entry_spec(&entry)).map_err(|errors| anyhow!(errors))?;
        let resolved = self.resolve_subagents(&canonical)?;
        Ok(Some(
            self.launch(Run::new(entry.id, canonical, resolved, name))
                .await,
        ))
    }

    /// Start a nonblocking run of a stored factory entry, or, when no
    /// entry has that id, of the library machine of that name (a template:
    /// no entry is created). Returns `{run_id, spec_id, name, nodes,
    /// max_parallel, started, pending}`, plus `machine` and `machine_path`
    /// for a library run.
    pub(super) async fn run(&self, spec_id: &str, name: Option<String>) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        if let Some(started) = self.run_stored(spec_id, name.clone()).await? {
            return Ok(started);
        }
        let quoted = repr_str(spec_id);
        let found = library::resolve_machine(&self.shared.environment, spec_id).map_err(
            |error| match error {
                ResolutionError::Broken(error) => {
                    anyhow!("the library machine {quoted} exists but is broken ({error})")
                }
                ResolutionError::Unknown(error) => anyhow!(
                    "unknown factory spec {quoted}: no stored factory entry and \
                     no library machine with that name ({error})"
                ),
                ResolutionError::InvalidName(error) => anyhow!(
                    "unknown factory spec {quoted}: no stored factory entry, and \
                     the id is not a valid machine name either ({error})"
                ),
            },
        )?;
        let canonical =
            canonicalize_factory_spec(&found.machine.spec).map_err(|errors| anyhow!(errors))?;
        let resolved = self.resolve_subagents(&canonical)?;
        let mut started = self
            .launch(Run::new(
                found.machine.name.clone(),
                canonical,
                resolved,
                name,
            ))
            .await;
        started["machine"] = json!(found.machine.name);
        started["machine_path"] = json!(found.path);
        Ok(started)
    }

    /// Register a run, enter its entry states, and start the control loop.
    /// Admission enters every entry state up to `max_parallel` and returns.
    async fn launch(&self, run: Run) -> Value {
        let cell: RunCell = Arc::new(Mutex::new(run));
        self.shared
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&cell));
        {
            let mut run = locked(&cell);
            let detail = format!(
                "{} states, max_parallel {}",
                run.states.len(),
                run.max_parallel
            );
            run.event("run_started", About::default(), Some(detail), Value::Null);
            for state in 0..run.states.len() {
                if field(&run.states[state].spec, "entry") == &Value::Bool(true) {
                    run.enter_state(state, None);
                }
            }
        }
        let started = self.spawn_ready(&cell, 0, /*allow_backoff*/ false).await;
        self.continue_run(&cell, 0).await;
        let run = locked(&cell);
        json!({
            "run_id": run.id,
            "spec_id": run.spec_id,
            "name": run.name,
            "nodes": run.states.len(),
            "max_parallel": run.max_parallel,
            "started": started,
            "pending": run.pending_state_ids(),
        })
    }

    /// After an admission phase: finish a run that is already quiescent,
    /// or hand a running one to a control loop. A run the phase paused
    /// (a budget) gets no loop: resuming is the operator's decision.
    async fn continue_run(&self, cell: &RunCell, generation: u64) {
        let (complete, running) = {
            let run = locked(cell);
            (run.is_complete(), run.state == RunState::Running)
        };
        if complete {
            self.finalize(cell).await;
        } else if running {
            let executor = self.clone();
            let cell = Arc::clone(cell);
            let control_loop =
                tokio::spawn(async move { executor.control_loop(cell, generation).await });
            let mut loops = self
                .shared
                .loops
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            loops.retain(|known| !known.is_finished());
            loops.push(control_loop.abort_handle());
        }
    }

    /// The run's states, the trailing event window, elapsed time and
    /// usage. Each call marks the events the parent had not seen delivered.
    pub(super) fn status(&self, run_id: &str) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        Ok(locked(&self.require_run(run_id)?).status())
    }

    /// Cancel every running child of the run and mark it stopped. The
    /// transitional `stopping` state is set before the first wait, so the
    /// control loop admits nothing and never finalizes meanwhile.
    /// Idempotent: a second stop returns without another ledger event.
    pub(super) async fn stop(&self, run_id: &str) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        let cell = self.require_run(run_id)?;
        {
            let mut run = locked(&cell);
            if matches!(run.state, RunState::Stopping | RunState::Stopped) {
                return Ok(json!({"run_id": run.id, "state": run.state.as_str(), "cancelled": []}));
            }
            run.state = RunState::Stopping;
            run.touch();
        }
        let stopped = self.halt_nonterminal(&cell, "run stopped").await;
        let mut run = locked(&cell);
        run.state = RunState::Stopped;
        let detail = format!("stopped; {} state(s) cancelled", stopped.len());
        run.event("run_stopped", About::default(), Some(detail), Value::Null);
        Ok(json!({"run_id": run.id, "state": "stopped", "cancelled": stopped}))
    }

    /// Resume a paused run. An escalate pause holds its failed entry
    /// unsettled, so resume re-admits that entry first: a fresh attempt of
    /// the same rendered prompt in the same entry slot, with fresh retries.
    /// A budget or `max_transitions` pause is reported once per run.
    pub(super) async fn resume(&self, run_id: &str) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        let cell = self.require_run(run_id)?;
        let generation = {
            let mut run = locked(&cell);
            if run.state != RunState::Paused {
                return Err(anyhow!(
                    "factory run {} is {}, not paused",
                    repr_str(run_id),
                    repr_str(run.state.as_str())
                ));
            }
            // The generation is bumped first: the loop the pause ended may
            // still be winding down, and must not continue as a second one.
            run.loop_generation += 1;
            run.state = RunState::Running;
            run.pause_reason = None;
            // Every pause after a resume notifies the parent again.
            run.milestones.remove("paused");
            run.event(
                "resumed",
                About::default(),
                Some("resumed by caller".to_string()),
                Value::Null,
            );
            // An escalate failure never queued its settle, so a held entry
            // is exactly `error` without a settle. Its errored and
            // cancelled instances run their rendered prompt again;
            // `max_entries` is not spent and the retry budget restarts.
            for state in 0..run.states.len() {
                for entry in 0..run.states[state].entries.len() {
                    let held = &mut run.states[state].entries[entry];
                    if held.status != Status::Error || held.is_settle {
                        continue;
                    }
                    held.status = if held.instances.is_empty() {
                        Status::Pending
                    } else {
                        Status::Running
                    };
                    held.error = None;
                    held.needs_verify = false;
                    for instance in &mut held.instances {
                        if matches!(instance.status, Status::Error | Status::Cancelled) {
                            instance.status = Status::Pending;
                            instance.error = None;
                            instance.answer = None;
                            instance.provisional = false;
                            instance.capture_failed = false;
                            instance.rate_limit_streak = 0;
                            instance.retry_base = instance.attempt;
                            instance.infra_respawned = false;
                        }
                    }
                    run.states[state].error = None;
                    run.entry_event(
                        "readmitted",
                        state,
                        entry,
                        Some(
                            "resume re-admits the failed entry (same rendered prompt)".to_string(),
                        ),
                        Value::Null,
                    );
                }
            }
            run.loop_generation
        };
        // A `max_transitions` pause left transitions mid-settle.
        self.evaluate_settles(&cell).await;
        let started = self
            .spawn_ready(&cell, generation, /*allow_backoff*/ false)
            .await;
        self.continue_run(&cell, generation).await;
        let run = locked(&cell);
        Ok(json!({
            "run_id": run.id,
            "state": run.state.as_str(),
            "started": started,
            "pending": run.pending_state_ids(),
        }))
    }

    /// One machine's structure fused with live state: a run id answers
    /// that run's snapshot, a stored spec id the static structure, and no
    /// reference every live run plus the newest terminal ones, oldest
    /// first, as `{runs}`.
    pub(super) fn graph(&self, reference: Option<&str>, detail: Detail) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        let Some(reference) = reference else {
            let runs = self
                .shared
                .runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            // A done run whose resident children still run is live: the
            // terminal history holds only runs with no child in flight.
            let live: Vec<bool> = runs
                .iter()
                .map(|cell| {
                    let run = locked(cell);
                    run.state.is_live() || run.running_instance_count() > 0
                })
                .collect();
            let terminal = live.iter().filter(|live| !**live).count();
            let mut skip = terminal.saturating_sub(GRAPH_RUNS_WINDOW);
            let mut snapshots = Vec::new();
            for (cell, live) in runs.iter().zip(live) {
                if !live && skip > 0 {
                    skip -= 1;
                    continue;
                }
                snapshots.push(locked(cell).graph_snapshot(detail));
            }
            return Ok(json!({ "runs": snapshots }));
        };
        if let Some(cell) = self.find_run(reference) {
            return Ok(locked(&cell).graph_snapshot(detail));
        }
        let quoted = repr_str(reference);
        let entry = self
            .shared
            .environment
            .factory_entry(reference)
            .ok_or_else(|| anyhow!("unknown factory run or spec {quoted}"))?;
        let canonical = canonicalize_factory_spec(&entry_spec(&entry))
            .map_err(|errors| anyhow!("factory spec {quoted} does not validate: {errors}"))?;
        Ok(spec_snapshot(&entry.id, &canonical))
    }

    /// Wait until the run's state or instance shape changes or the timeout
    /// (seconds, capped at 60) elapses, then return the graph snapshot
    /// plus `changed`: whether a change ended the wait.
    pub(super) async fn watch(
        &self,
        run_id: &str,
        timeout_seconds: f64,
        detail: Detail,
    ) -> anyhow::Result<Value> {
        self.shared.environment.require_enabled()?;
        let cell = self.require_run(run_id)?;
        if !timeout_seconds.is_finite() || timeout_seconds < 0.0 {
            return Err(anyhow!("timeout must be a non-negative number of seconds"));
        }
        let deadline = Instant::now()
            + Duration::from_secs_f64(timeout_seconds.min(WATCH_TIMEOUT_CAP_SECONDS));
        let (baseline, mut revision) = {
            let run = locked(&cell);
            (run.signature(), run.revision.subscribe())
        };
        let changed = loop {
            if locked(&cell).signature() != baseline {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::select! {
                // The sender lives in the run, which `cell` keeps alive.
                _ = revision.changed() => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        };
        let mut snapshot = locked(&cell).graph_snapshot(detail);
        snapshot["changed"] = json!(changed);
        Ok(snapshot)
    }

    /// Export one machine to `MACHINE.md` at `out_path`: a stored factory
    /// entry's spec first, a run's canonical machine second, then the
    /// library machine of that name (copied verbatim, so its documentation
    /// travels with the spec). Returns `{name, path, source}`.
    pub(super) fn export_machine(
        &self,
        target: &str,
        out_path: &str,
        overwrite: Overwrite,
    ) -> anyhow::Result<Value> {
        let destination = self.session_path(out_path);
        let quoted = repr_str(target);
        let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let named = |name: &str, what: String| {
            let mut errors = library::machine_name_errors(&json!(name));
            if errors.is_empty() {
                Ok(())
            } else {
                errors.push(what);
                Err(anyhow!(errors.join("; ")))
            }
        };
        if let Some(entry) = self.shared.environment.factory_entry(target) {
            let spec = entry_spec(&entry);
            if spec.is_null() {
                return Err(anyhow!(
                    "factory entry {quoted} carries no machine or dag spec"
                ));
            }
            named(
                target,
                format!("the stored entry id {quoted} cannot become a machine name"),
            )?;
            let description = Some(one_line(&entry.content))
                .filter(|content| !content.is_empty())
                .unwrap_or_else(|| one_line(&entry.title));
            return library::export_spec(&spec, &destination, target, &description, overwrite)
                .map_err(|error| anyhow!(error));
        }
        if let Some(cell) = self.find_run(target) {
            let (machine, spec_id, description) = {
                let run = locked(&cell);
                let description = run
                    .name
                    .as_deref()
                    .map(one_line)
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("factory run {}", run.id));
                (run.machine.clone(), run.spec_id.clone(), description)
            };
            named(
                &spec_id,
                format!(
                    "the run's spec id {} cannot become a machine name",
                    repr_str(&spec_id)
                ),
            )?;
            return library::export_spec(&machine, &destination, &spec_id, &description, overwrite)
                .map_err(|error| anyhow!(error));
        }
        library::export_library_machine(&self.shared.environment, target, &destination, overwrite)
            .map_err(|error| anyhow!(error))
    }

    /// Answer one request of the daemon and TUI lane: `{action, runId,
    /// specId, timeoutMs}` routed to graph, status, watch, run, stop or
    /// resume. The reply carries the wire's camelCase keys; graph and
    /// watch answer with the compact snapshots.
    async fn activity(&self, request: &Value) -> anyhow::Result<Value> {
        fn required<'a>(action: &str, id: Option<&'a str>, key: &str) -> anyhow::Result<&'a str> {
            id.ok_or_else(|| anyhow!("factory activity {action} requires {key}"))
        }
        self.shared.environment.require_enabled()?;
        let action = request["action"].as_str().unwrap_or_default();
        if !ACTIVITY_ACTIONS.contains(&action) {
            return Err(anyhow!(
                "unknown factory activity action {}",
                super::python_text::repr(&request["action"])
            ));
        }
        let target = |key: &str| match &request[key] {
            Value::Null => Ok(None),
            Value::String(id) => Ok(Some(id.as_str()).filter(|id| !id.is_empty())),
            _ => Err(anyhow!(
                "factory activity {key} must be a string when provided"
            )),
        };
        let (run_id, spec_id) = (target("runId")?, target("specId")?);
        let timeout_ms = match &request["timeoutMs"] {
            Value::Null => 0,
            timeout => timeout
                .as_u64()
                .filter(|timeout| *timeout <= ACTIVITY_TIMEOUT_MS_CAP)
                .ok_or_else(|| {
                    anyhow!(
                        "factory activity timeoutMs must be an integer between 0 and {ACTIVITY_TIMEOUT_MS_CAP}"
                    )
                })?,
        };
        let reply = match action {
            "graph" => self.graph(run_id.or(spec_id), Detail::Compact)?,
            "status" => self.status(required(action, run_id, "runId")?)?,
            "watch" => {
                self.watch(
                    required(action, run_id, "runId")?,
                    Duration::from_millis(timeout_ms).as_secs_f64(),
                    Detail::Compact,
                )
                .await?
            }
            // The lane starts stored entries only: no library fallback.
            "run" => {
                let spec_id = required(action, spec_id, "specId")?;
                self.run_stored(spec_id, None)
                    .await?
                    .ok_or_else(|| anyhow!("unknown factory spec {}", repr_str(spec_id)))?
            }
            "stop" => self.stop(required(action, run_id, "runId")?).await?,
            _ => self.resume(required(action, run_id, "runId")?).await?,
        };
        Ok(wire_payload(&reply))
    }

    /// One request of the daemon and TUI lane as its reply frame: `{event:
    /// "done", id, status: "ok", result}` or `{..., status: "error",
    /// reason}`, kept under the wire cap.
    pub(in crate::kernel::scheme) async fn activity_reply(&self, request: &Value) -> Value {
        let mut frame = match self.activity(request).await {
            Ok(result) => {
                json!({"event": "done", "id": request["id"], "status": "ok", "result": result})
            }
            Err(error) => json!({
                "event": "done",
                "id": request["id"],
                "status": "error",
                "reason": format!("{error:#}"),
            }),
        };
        cap_frame(&mut frame);
        frame
    }

    // -- ledger ------------------------------------------------------------

    /// Record a run milestone and tell the parent conversation once per
    /// kind (a resume re-arms the `paused` notice). Every milestone lands
    /// in the ledger, repeats included.
    pub(super) async fn milestone(
        &self,
        cell: &RunCell,
        kind: &str,
        detail: String,
        node: Option<String>,
    ) {
        let (event, run_id) = {
            let mut run = locked(cell);
            let event = run.event(
                "milestone",
                About {
                    node: node.as_deref(),
                    ..About::default()
                },
                Some(detail.clone()),
                json!({ "milestone": kind }),
            );
            if !run.milestones.insert(kind.to_string()) {
                return;
            }
            (event, run.id.clone())
        };
        let mut payload = json!({"run_id": run_id, "kind": kind, "detail": detail});
        if let Some(node) = node {
            payload["node"] = json!(node);
        }
        // A dead bridge cannot be told; the ledger keeps the milestone and
        // status still shows it to the parent.
        if self.host("factory.progress", payload).await.is_ok() {
            locked(cell).events[event].insert("stage".to_string(), json!("shown"));
        }
    }

    async fn evaluate_settles(&self, cell: &RunCell) {
        let pause = locked(cell).evaluate_settles();
        if let Some(pause) = pause {
            self.milestone(cell, "max_transitions_exceeded", pause.detail, None)
                .await;
        }
    }

    // -- readiness, admission ------------------------------------------------

    /// Whether a control-lane pass is current: the run is running and its
    /// generation still owns the state.
    fn alive(cell: &RunCell, generation: u64) -> bool {
        let run = locked(cell);
        run.state == RunState::Running && run.loop_generation == generation
    }

    /// Prepare ready entries and admit pending instances up to
    /// `max_parallel`. The run budget and the child budget are enforced
    /// before each admission, and admission is skipped while a rate-limit
    /// backoff deadline is outstanding. Returns the states that had an
    /// instance admitted here.
    async fn spawn_ready(
        &self,
        cell: &RunCell,
        generation: u64,
        allow_backoff: bool,
    ) -> Vec<String> {
        enum Step {
            Done,
            Budget,
            Children,
            Python(Place),
            Admit(Place),
        }
        let mut started: Vec<String> = Vec::new();
        while Self::alive(cell, generation) {
            self.prepare_ready_entries(cell).await;
            let step = {
                let run = locked(cell);
                let backing_off = run
                    .admission_backoff_until
                    .is_some_and(|until| Instant::now() < until);
                if run.state != RunState::Running || run.loop_generation != generation {
                    Step::Done
                } else if run.budget_exceeded() {
                    Step::Budget
                } else if backing_off || run.running_instance_count() >= run.max_parallel {
                    Step::Done
                } else {
                    match run.next_pending_instance() {
                        None => Step::Done,
                        Some(place) if run.states[place.state].is_python() => Step::Python(place),
                        Some(_)
                            if !run.max_children_reported
                                && run.spawn_count >= run.max_children =>
                        {
                            Step::Children
                        }
                        Some(place) => Step::Admit(place),
                    }
                }
            };
            let (place, admission) = match step {
                Step::Done => break,
                Step::Budget => {
                    self.pause_for_budget(cell).await;
                    break;
                }
                Step::Children => {
                    // `max_children` bounds every admission over the run's
                    // life, foreach expansions and retries included.
                    let detail = {
                        let mut run = locked(cell);
                        run.state = RunState::Paused;
                        run.pause_reason = Some("max_children exceeded".to_string());
                        run.max_children_reported = true;
                        format!(
                            "run max_children {} exceeded after {} children; \
                             no new spawns; resume with (factory-resume \"{}\")",
                            run.max_children, run.spawn_count, run.id
                        )
                    };
                    self.milestone(cell, "max_children_exceeded", detail, None)
                        .await;
                    break;
                }
                Step::Python(place) => (place, self.start_python(cell, place).await),
                Step::Admit(place) => (place, self.admit(cell, place, allow_backoff).await),
            };
            match admission {
                Admission::Admitted => {
                    let state_id = locked(cell).states[place.state].state_id.clone();
                    if !started.contains(&state_id) {
                        started.push(state_id);
                    }
                }
                // A rate limit is usually global: stop admitting in this
                // phase; the control loop waits out the deadline.
                Admission::Stopped | Admission::Deferred => break,
                Admission::Failed => {}
            }
        }
        started
    }

    /// Pause the run at the budget boundary. Children in flight keep
    /// running and settle once the run resumes; no further budget pause
    /// fires.
    async fn pause_for_budget(&self, cell: &RunCell) {
        let detail = {
            let mut run = locked(cell);
            run.state = RunState::Paused;
            run.pause_reason = Some("run budget exceeded".to_string());
            run.budget_reported = true;
            format!(
                "run budget_ms {} exceeded after {}ms; no new spawns; \
                 resume with (factory-resume \"{}\")",
                run.budget_ms.unwrap_or_default(),
                run.elapsed_ms(),
                run.id
            )
        };
        self.milestone(cell, "budget_exceeded", detail, None).await;
    }

    /// Bind inputs and create instances for every pending entry whose
    /// input sources have settled.
    async fn prepare_ready_entries(&self, cell: &RunCell) {
        let (mut state, mut entry) = (0, 0);
        loop {
            let failure = {
                let mut run = locked(cell);
                let mut failure = None;
                while failure.is_none() && state < run.states.len() {
                    if entry >= run.states[state].entries.len() {
                        (state, entry) = (state + 1, 0);
                        continue;
                    }
                    let current = entry;
                    entry += 1;
                    if run.states[state].entries[current].status != Status::Pending {
                        continue;
                    }
                    match run.prepare_entry(state) {
                        Prepared::Waiting => {}
                        Prepared::Failed(reason) => failure = Some((state, current, reason)),
                        Prepared::Ready(instances) => {
                            let count = instances.len();
                            let prepared = &mut run.states[state].entries[current];
                            prepared.instances = instances;
                            if count > 0 {
                                prepared.status = Status::Running;
                                run.entry_event(
                                    "node_ready",
                                    state,
                                    current,
                                    Some(format!("{count} instance(s) prepared")),
                                    Value::Null,
                                );
                            } else {
                                prepared.status = Status::Done;
                                run.entry_event(
                                    "node_ready",
                                    state,
                                    current,
                                    Some(
                                        "foreach expanded to zero items; nothing to run"
                                            .to_string(),
                                    ),
                                    Value::Null,
                                );
                                run.queue_settle(state, current);
                            }
                        }
                    }
                }
                failure
            };
            let Some((failed_state, failed_entry, reason)) = failure else {
                return;
            };
            self.apply_entry_failure_policy(cell, failed_state, failed_entry, reason)
                .await;
            if locked(cell).state != RunState::Running {
                return;
            }
        }
    }

    /// Spawn one instance. A rate-limited admission never sleeps here: in
    /// the control loop it records a backoff deadline on the run (doubling
    /// delays capped at 60 seconds) and defers, so children keep being
    /// collected while admission waits; in `run` and `resume` it only
    /// defers. Five rate-limited admissions in a row fail the node; any
    /// other admission error fails the entry at once.
    async fn admit(&self, cell: &RunCell, place: Place, allow_backoff: bool) -> Admission {
        let (prompt, kwargs) = {
            let mut run = locked(cell);
            run.instance_mut(place).attempt += 1;
            let state = &run.states[place.state];
            let instance = run.instance(place);
            let child_name = spawn_label(
                state.name.as_deref(),
                &run.id,
                &state.state_id,
                i64::try_from(instance.index).unwrap_or(i64::MAX),
                instance.attempt,
            );
            let mut kwargs = json!({
                "name": child_name,
                "factory": {"runId": run.id, "specId": run.spec_id, "stateId": state.state_id},
            });
            if let Some(model) = &state.model {
                kwargs["model"] = json!(model);
            }
            if let Some(thinking) = &state.thinking {
                kwargs["thinking"] = json!(thinking);
            }
            (instance.prompt.clone(), kwargs)
        };
        let child_name = kwargs["name"].clone();
        let spawned = self
            .host("rlm.run", json!({"prompt": prompt, "kwargs": kwargs}))
            .await
            .and_then(|handle| {
                handle["rlm_child_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("rlm.spawn returned an invalid handle"))
            });
        let child_id = match spawned {
            Ok(child_id) => child_id,
            Err(error) => {
                let last_error = format!("{error:#}");
                if !is_rate_limit_error(&last_error) {
                    self.apply_instance_failure(
                        cell,
                        place,
                        format!("spawn admission failed: {last_error}"),
                        /*retry*/ false,
                    )
                    .await;
                    return Admission::Failed;
                }
                let exhausted = {
                    let mut run = locked(cell);
                    if allow_backoff {
                        let instance = run.instance_mut(place);
                        instance.rate_limit_streak += 1;
                        let streak = instance.rate_limit_streak;
                        if streak < BACKOFF_MAX_ATTEMPTS {
                            let delay = (1u64 << (streak - 1)).min(BACKOFF_CAP_SECONDS);
                            let deadline = Instant::now() + Duration::from_secs(delay);
                            if run
                                .admission_backoff_until
                                .is_none_or(|until| deadline > until)
                            {
                                run.admission_backoff_until = Some(deadline);
                            }
                            run.instance_event(
                                "spawn_backoff",
                                place,
                                Some(format!("rate limited; retrying in {delay}s")),
                                Value::Null,
                            );
                        }
                        streak >= BACKOFF_MAX_ATTEMPTS
                    } else {
                        run.instance_event(
                            "spawn_deferred",
                            place,
                            Some(format!("rate limited at admission: {last_error}")),
                            Value::Null,
                        );
                        false
                    }
                };
                if exhausted {
                    self.apply_instance_failure(
                        cell,
                        place,
                        format!("spawn admission failed: {last_error}"),
                        /*retry*/ false,
                    )
                    .await;
                    return Admission::Failed;
                }
                return Admission::Deferred;
            }
        };
        let stopped = {
            let mut run = locked(cell);
            run.instance_mut(place).child_id = Some(child_id.clone());
            if run.state == RunState::Running {
                let instance = run.instance_mut(place);
                instance.spawned_at = Some(Instant::now());
                instance.status = Status::Running;
                instance.rate_limit_streak = 0;
                let attempt = instance.attempt;
                run.spawn_count += 1;
                run.instance_event(
                    "spawned",
                    place,
                    None,
                    json!({"attempt": attempt, "child": child_id, "name": child_name}),
                );
            }
            run.state != RunState::Running
        };
        if !stopped {
            return Admission::Admitted;
        }
        // A stop ran while the spawn was in flight and could not see this
        // child: delete it here, or it would outlive the stopped run.
        let deleted = self.delete_child(&child_id).await;
        let mut run = locked(cell);
        Self::record_cancellation(&mut run, place, &child_id, deleted);
        run.instance_mut(place).status = Status::Cancelled;
        Admission::Stopped
    }

    /// The ledger rows of one child's cancellation. A failed delete leaves
    /// the child with the supervisor, but its slot is released either way,
    /// so the cancellation is recorded next to the failure.
    pub(super) fn record_cancellation(
        run: &mut Run,
        place: Place,
        child_id: &str,
        deleted: anyhow::Result<Value>,
    ) {
        match deleted {
            Ok(_) => {
                run.instance_event("cancelled", place, None, json!({ "child": child_id }));
            }
            Err(error) => {
                run.instance_event(
                    "cancel_failed",
                    place,
                    None,
                    json!({"child": child_id, "error": format!("{error:#}")}),
                );
                run.instance_event(
                    "cancelled",
                    place,
                    Some("slot released despite the failed delete".to_string()),
                    json!({ "child": child_id }),
                );
            }
        }
    }

    /// Start one python state call. It holds a `max_parallel` slot but is
    /// no child: it never counts toward `max_children`.
    async fn start_python(&self, cell: &RunCell, place: Place) -> Admission {
        let started = {
            let mut run = locked(cell);
            run.instance_mut(place).attempt += 1;
            let empty = serde_json::Map::new();
            let target = field(&run.states[place.state].spec, "python")
                .as_object()
                .unwrap_or(&empty);
            let inputs = run.instance(place).inputs.as_ref().unwrap_or(&empty);
            PythonCall::start(target, inputs, &self.shared.cwd)
        };
        let process = match started {
            Ok(process) => process,
            Err(error) => {
                self.apply_instance_failure(
                    cell,
                    place,
                    format!("python state could not start: {error}"),
                    /*retry*/ false,
                )
                .await;
                return Admission::Failed;
            }
        };
        let stopped = {
            let mut run = locked(cell);
            if run.state == RunState::Running {
                let limit = Self::python_limit(&run, place.state);
                let instance = run.instance_mut(place);
                let now = Instant::now();
                instance.call = Some(process);
                instance.spawned_at = Some(now);
                instance.deadline = Some(now + limit);
                instance.status = Status::Running;
                let attempt = instance.attempt;
                run.instance_event("started", place, None, json!({ "attempt": attempt }));
                None
            } else {
                // A stop ran while the process was starting.
                run.instance_mut(place).status = Status::Cancelled;
                run.instance_event("cancelled", place, None, Value::Null);
                Some(process)
            }
        };
        match stopped {
            Some(process) => {
                process.stop().await;
                Admission::Stopped
            }
            None => Admission::Admitted,
        }
    }

    /// A python process's time limit: the state's `budget_ms`, else the
    /// run's, else the default.
    fn python_limit(run: &Run, state: usize) -> Duration {
        Duration::from_millis(
            field(&run.states[state].spec, "budget_ms")
                .as_u64()
                .or(run.budget_ms)
                .unwrap_or(PYTHON_STATE_TIMEOUT_MS_DEFAULT),
        )
    }

    /// Settle one python process that exited or ran past its time limit.
    async fn apply_python_exit(&self, cell: &RunCell, place: Place) {
        let (process, limit) = {
            let mut run = locked(cell);
            let limit = Self::python_limit(&run, place.state);
            let instance = run.instance_mut(place);
            // Claimed before any wait, like a child being cancelled.
            let Some(process) = instance.call.take() else {
                return;
            };
            instance.duration_ms = instance
                .spawned_at
                .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX));
            (process, limit)
        };
        let Some(exit_code) = process.exit_code() else {
            // Past its time limit: the limit is spent, so no retry.
            process.stop().await;
            self.apply_instance_failure(
                cell,
                place,
                format!("python state time limit {}ms exceeded", limit.as_millis()),
                /*retry*/ false,
            )
            .await;
            return;
        };
        let (stdout, stderr) = process.finish();
        let outputs = {
            let mut run = locked(cell);
            run.settle_count += 1;
            if exit_code == 0 {
                python_outputs(&run.states[place.state].spec, &stdout)
            } else if stderr.is_empty() {
                Err(format!("python state exited with code {exit_code}"))
            } else {
                Err(format!(
                    "python state exited with code {exit_code}: {stderr}"
                ))
            }
        };
        let outputs = match outputs {
            Ok(outputs) => outputs,
            Err(reason) => {
                self.apply_instance_failure(cell, place, reason, /*retry*/ true)
                    .await;
                return;
            }
        };
        let mut run = locked(cell);
        let answer = capped(
            &Value::Object(outputs.clone()).to_string(),
            ANSWER_BINDING_CAP,
        );
        let instance = run.instance_mut(place);
        instance.status = Status::Done;
        instance.answer = Some(answer.clone());
        let duration_ms = instance.duration_ms;
        run.instance_event(
            "settled",
            place,
            None,
            json!({"status": "done", "duration_ms": duration_ms}),
        );
        Self::record_answer(&mut run, place, &answer);
        let entry = &mut run.states[place.state].entries[place.entry];
        entry.answer = Some(answer);
        entry.outputs = outputs;
        entry.output_errors = serde_json::Map::new();
        entry.status = Status::Done;
        run.queue_settle(place.state, place.entry);
    }

    /// The `answer_captured` ledger row: a compact preview of an answer
    /// that arrived.
    pub(super) fn record_answer(run: &mut Run, place: Place, answer: &str) {
        let event = run.instance_event(
            "answer_captured",
            place,
            None,
            json!({ "answer": capped(answer, ANSWER_CAPTURE_CAP) }),
        );
        run.events[event].insert("stage".to_string(), json!("arrived"));
    }

    // -- completion, control loop --------------------------------------------

    async fn finalize(&self, cell: &RunCell) {
        let (kind, detail) = {
            let mut run = locked(cell);
            if run.state != RunState::Running {
                // A stop or a failure policy owns the final state.
                return;
            }
            let errored: Vec<&str> = run
                .states
                .iter()
                .filter(|state| {
                    state
                        .entries
                        .iter()
                        .any(|entry| entry.status == Status::Error)
                })
                .map(|state| state.state_id.as_str())
                .collect();
            if errored.is_empty() {
                let residents = run
                    .states
                    .iter()
                    .filter(|state| {
                        state.is_resident()
                            && state
                                .entries
                                .iter()
                                .any(|entry| entry.status == Status::Running)
                    })
                    .count();
                let mut detail = format!(
                    "run complete: {} state(s), {} transition(s) fired",
                    run.states.len(),
                    run.transitions_fired
                );
                if residents > 0 {
                    let _ = write!(
                        detail,
                        "; {residents} resident state(s) still running (stop with (factory-stop \"{}\"))",
                        run.id
                    );
                }
                run.state = RunState::Done;
                ("finished", detail)
            } else {
                let detail = format!("completed with state error(s): {}", errored.join(", "));
                run.state = RunState::Failed;
                ("failed", detail)
            }
        };
        self.milestone(cell, kind, detail, None).await;
    }

    /// Drive one run until it is quiescent, paused, stopped or failed. A
    /// host failure must not wedge the run silently: it fails the run with
    /// the reason in the ledger, the children staying with the supervisor.
    async fn control_loop(&self, cell: RunCell, generation: u64) {
        let Err(error) = self.loop_body(&cell, generation).await else {
            return;
        };
        let reason = format!("{error:#}");
        let failed = {
            let mut run = locked(&cell);
            run.event(
                "executor_error",
                About::default(),
                None,
                json!({ "error": reason }),
            );
            let running = run.state == RunState::Running;
            if running {
                run.state = RunState::Failed;
            }
            running
        };
        if failed {
            self.cancel_python(&cell).await;
            self.milestone(&cell, "failed", format!("executor error: {reason}"), None)
                .await;
        }
    }

    async fn loop_body(&self, cell: &RunCell, generation: u64) -> anyhow::Result<()> {
        while Self::alive(cell, generation) {
            let (children, python_calls, python_deadline, resident_only) = {
                let run = locked(cell);
                let in_flight: Vec<Place> = run
                    .places()
                    .into_iter()
                    .filter(|place| run.instance(*place).status == Status::Running)
                    .collect();
                let python_calls: Vec<Place> = in_flight
                    .iter()
                    .copied()
                    .filter(|place| run.instance(*place).call.is_some())
                    .collect();
                (
                    in_flight
                        .iter()
                        .filter_map(|place| Some((*place, run.instance(*place).child_id.clone()?)))
                        .filter(|(place, _)| run.instance(*place).call.is_none())
                        .collect::<Vec<_>>(),
                    python_calls
                        .iter()
                        .map(|place| {
                            (
                                *place,
                                run.instance(*place)
                                    .call
                                    .as_ref()
                                    .map(PythonCall::exit_watch),
                            )
                        })
                        .collect::<Vec<_>>(),
                    python_calls
                        .iter()
                        .filter_map(|place| run.instance(*place).deadline)
                        .min(),
                    in_flight
                        .iter()
                        .all(|place| run.states[place.state].is_resident()),
                )
            };
            // A child poll must not postpone a python call's deadline.
            let wait = python_deadline.map_or(POLL_TIMEOUT, |deadline| {
                POLL_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()))
            });
            if !children.is_empty() {
                let targets: Vec<String> = children
                    .iter()
                    .map(|(_, child_id)| child_id.clone())
                    .collect();
                let results = self.collect(&targets, wait).await?;
                for (place, child_id) in &children {
                    // A resume mid-batch drops the rest: its loop collects
                    // the children again, so no settle is applied twice. A
                    // pause this batch caused does not drop the rest.
                    if locked(cell).loop_generation != generation {
                        return Ok(());
                    }
                    if let Some(result) = results
                        .iter()
                        .find(|row| row.settled && row.child_id == *child_id)
                    {
                        self.apply_settlement(cell, *place, result.clone()).await?;
                    }
                }
            } else if !python_calls.is_empty() {
                let exits = python_calls
                    .iter()
                    .filter_map(|(_, exit)| exit.clone())
                    .map(|mut exit| {
                        Box::pin(async move {
                            // An error means the call's waiter is gone.
                            let _ = exit.wait_for(Option::is_some).await;
                        })
                    });
                let first_exit = futures::future::select_all(exits);
                // A timeout only means no call exited in this slice.
                let _ = tokio::time::timeout(wait, first_exit).await;
            }
            for (place, _) in &python_calls {
                let due = {
                    let run = locked(cell);
                    if run.loop_generation != generation {
                        return Ok(());
                    }
                    let instance = run.instance(*place);
                    instance.status == Status::Running
                        && instance.call.as_ref().is_some_and(|call| {
                            call.exit_code().is_some()
                                || instance
                                    .deadline
                                    .is_some_and(|deadline| Instant::now() >= deadline)
                        })
                };
                if due {
                    self.apply_python_exit(cell, *place).await;
                }
            }
            // A stop can land while the collect was in flight, and a
            // stopped run must never finalize as done.
            if !Self::alive(cell, generation) {
                return Ok(());
            }
            self.evaluate_settles(cell).await;
            if !Self::alive(cell, generation) {
                return Ok(());
            }
            let (complete, over_budget) = {
                let run = locked(cell);
                (run.is_complete(), run.budget_exceeded())
            };
            if complete {
                self.finalize(cell).await;
                return Ok(());
            }
            if over_budget {
                self.pause_for_budget(cell).await;
                return Ok(());
            }
            let started = self
                .spawn_ready(cell, generation, /*allow_backoff*/ true)
                .await;
            if !Self::alive(cell, generation) {
                return Ok(());
            }
            let (backoff, stall) = {
                let run = locked(cell);
                let backoff = run
                    .admission_backoff_until
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .filter(|remaining| !remaining.is_zero());
                let idle = run.pending_evaluations.is_empty();
                let stall = if backoff.is_some() {
                    None
                } else if idle && run.resident_cap_starved() {
                    // Residents never settle, so the queued work would
                    // wait forever behind them.
                    Some(format!(
                        "control loop stalled: resident instances hold every max_parallel {} slot; \
                         queued instances can never be admitted",
                        run.max_parallel
                    ))
                } else if idle && resident_only && started.is_empty() && !run.has_pending_instance()
                {
                    // Nothing in flight (or only residents, which can
                    // unblock nothing), nothing admitted, nothing pending:
                    // the one reachable shape is a pending entry whose
                    // input source never settled.
                    let stuck: Vec<String> = run
                        .states
                        .iter()
                        .flat_map(|state| {
                            state
                                .entries
                                .iter()
                                .filter(|entry| {
                                    entry.status == Status::Pending && entry.instances.is_empty()
                                })
                                .map(|_| repr_str(&state.state_id))
                        })
                        .collect();
                    Some(if stuck.is_empty() {
                        "control loop stalled: no in-flight or pending work".to_string()
                    } else {
                        format!(
                            "control loop stalled: pending entry of state {} \
                             is waiting for an input source that never settled",
                            stuck.join(", ")
                        )
                    })
                } else {
                    None
                };
                (backoff, stall)
            };
            if let Some(remaining) = backoff {
                // Wait out the deadline in bounded slices: children that
                // settle meanwhile are collected on the next pass.
                tokio::time::sleep(remaining.min(POLL_TIMEOUT)).await;
                continue;
            }
            if let Some(reason) = stall {
                // End the run instead of polling a dead end.
                {
                    let mut run = locked(cell);
                    run.event(
                        "executor_error",
                        About::default(),
                        None,
                        json!({ "error": reason }),
                    );
                    run.state = RunState::Failed;
                }
                self.milestone(cell, "failed", reason, None).await;
                return Ok(());
            }
            // A host that settles instantly must not starve other tasks.
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}
