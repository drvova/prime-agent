//! Parallel hypotheses: `(rlm-hypotheses task approaches #:check ...)`.
//!
//! Each approach gets its own worktree and one child spawned in it. Once the
//! children settle (or the time runs out, and the stragglers are aborted),
//! the acceptance check runs in every checkout and the approaches are
//! ranked: passing first, then the ones whose child finished, then the
//! smaller diff. The losing worktrees are removed; the winner's stays for
//! the model to bring home with `worktree-merge`.
//!
//! A run whose check is a shell command is one host call, so a cell that is
//! aborted or times out midway still stops the children it started and
//! removes their worktrees before the call returns. A check that is a Scheme
//! procedure runs in the cell between two calls (`start`, then `rank`).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use super::worktree::{patch_files, Tree, Worktrees};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::platform;
use crate::tools::tool_definition::AbortSignal;

const APPROACHES_MIN: usize = 2;
const APPROACHES_MAX: usize = 6;
const APPROACH_CHARS_MAX: usize = 2_000;
const TASK_CHARS_MAX: usize = 20_000;
/// How long one checkout's check may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(900);
/// Characters of a check's output kept, from its end.
const CHECK_TAIL_CHARS: usize = 2_000;

/// Send `data` as the host request `request_type` and wait for its reply,
/// giving up as soon as `stop` is cancelled.
///
/// # Errors
///
/// No handler answers `request_type`, the handler failed, or `stop` was
/// cancelled first.
async fn request(
    handlers: &HostRequestHandlers,
    stop: &AbortSignal,
    request_type: &str,
    mut data: Value,
) -> anyhow::Result<Value> {
    let handler = handlers
        .get(request_type)
        .ok_or_else(|| anyhow!("unknown host request: {request_type}"))?;
    // Some handlers read their type from the payload.
    if let Some(fields) = data.as_object_mut() {
        fields.insert("type".to_string(), Value::from(request_type));
    }
    tokio::select! {
        biased;
        () = stop.cancelled() => bail!("stopped with its cell"),
        reply = handler(HostRequestPayload { data }) => reply,
    }
}

/// One approach while its run is in flight.
struct Lane {
    approach: String,
    tree: Tree,
    /// The child's id once it was spawned.
    child: Option<String>,
}

/// A session's hypotheses runs.
struct Hypotheses {
    handlers: HostRequestHandlers,
    worktrees: Arc<Worktrees>,
}

impl Hypotheses {
    /// Make the worktrees, spawn the children, wait for them, and (with a
    /// shell `check`) run the check from every checkout's root: one unranked
    /// row per approach. A check and everything it started are killed when
    /// it runs past [`CHECK_TIMEOUT`] or `stop` is cancelled. `lanes` holds
    /// what was started, for the caller to clean up.
    async fn started(
        &self,
        stop: &AbortSignal,
        options: &Value,
        lanes: &mut Vec<Lane>,
    ) -> anyhow::Result<Vec<Value>> {
        let task = options["task"]
            .as_str()
            .filter(|task| !task.trim().is_empty())
            .ok_or_else(|| anyhow!("task must be a non-empty string"))?;
        if task.chars().count() > TASK_CHARS_MAX {
            bail!(
                "task is at most {TASK_CHARS_MAX} characters (got {})",
                task.chars().count()
            );
        }
        let approaches: Vec<&str> = options["approaches"]
            .as_array()
            .map(|approaches| approaches.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if options["approaches"]
            .as_array()
            .is_none_or(|given| given.len() != approaches.len())
        {
            bail!("approaches must be a list of strings");
        }
        if !(APPROACHES_MIN..=APPROACHES_MAX).contains(&approaches.len()) {
            bail!(
                "approaches must hold {APPROACHES_MIN} to {APPROACHES_MAX} entries (got {})",
                approaches.len()
            );
        }
        for (index, approach) in approaches.iter().enumerate() {
            if approach.trim().is_empty() {
                bail!("every approach must be a non-empty string");
            }
            if approach.chars().count() > APPROACH_CHARS_MAX {
                bail!(
                    "an approach is at most {APPROACH_CHARS_MAX} characters (got {})",
                    approach.chars().count()
                );
            }
            if approaches[..index].contains(approach) {
                bail!("approaches must be distinct");
            }
        }
        let check = match &options["check"] {
            Value::Null => None,
            Value::String(command) if !command.trim().is_empty() => Some(command.as_str()),
            Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => bail!(
                "#:check must be a shell command string or a procedure taking the checkout path"
            ),
        };
        let timeout_ms = options["timeout_ms"]
            .as_u64()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| anyhow!("#:timeout-ms must be a positive integer"))?;
        let better = options["better"] == true;

        // A session at its depth bound refuses before any worktree is made.
        let session = request(&self.handlers, stop, "settings.session", json!({})).await?;
        let host = request(&self.handlers, stop, "settings.host", json!({})).await?;
        let depth = session["rlmDepth"].as_u64().unwrap_or(0);
        let max_depth = host["rlmMaxDepth"]["maxDepth"].as_u64().unwrap_or(0);
        if max_depth > 0 && depth + 1 > max_depth {
            bail!(
                "rlm-hypotheses needs one level of children, but this session is at rlm depth {depth} of max {max_depth}"
            );
        }

        let run = &uuid::Uuid::new_v4().simple().to_string()[..4];
        for (index, approach) in approaches.iter().enumerate() {
            let name = format!("hyp-{run}-{}", index + 1);
            let tree = self.worktrees.create(stop, &name, "HEAD", true).await?;
            lanes.push(Lane {
                approach: (*approach).to_string(),
                tree,
                child: None,
            });
        }
        for lane in lanes.iter_mut() {
            let path = lane.tree.path.to_string_lossy();
            let shown = check.map_or_else(
                || "a check the parent runs".to_string(),
                |command| format!("`{command}`"),
            );
            let mut brief = format!(
                "{task}\n\nApproach to take: {}\n\nWork only in this checkout: {path} (an isolated git worktree; the parent merges it if this approach wins). The acceptance check, run from the checkout root, is {shown}. Make it pass with this approach and no other; do not commit. Answer with one line on what you changed.",
                lane.approach
            );
            if better {
                let checked = request(
                    &self.handlers,
                    stop,
                    "better.run",
                    json!({ "task": brief, "mode": "brief", "cwd": path }),
                )
                .await?;
                brief = checked["rewrite"]
                    .as_str()
                    .ok_or_else(|| anyhow!("better.run returned no rewrite"))?
                    .to_string();
            }
            let handle = request(
                &self.handlers,
                stop,
                "rlm.run",
                json!({ "prompt": brief, "kwargs": { "name": lane.tree.name, "cwd": path } }),
            )
            .await?;
            lane.child = Some(
                handle["rlm_child_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("rlm.run returned no rlm_child_id"))?
                    .to_string(),
            );
        }
        let children: Vec<&str> = lanes
            .iter()
            .filter_map(|lane| lane.child.as_deref())
            .collect();
        let collected = request(
            &self.handlers,
            stop,
            "rlm.collect",
            json!({ "targets": children, "timeout_ms": timeout_ms }),
        )
        .await?;
        let mut rows = Vec::with_capacity(lanes.len());
        for lane in lanes.iter() {
            let settled = collected["results"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|result| result["rlm_child_id"].as_str() == lane.child.as_deref());
            let mut status = settled
                .and_then(|result| result["status"].as_str())
                .unwrap_or("missing");
            if settled.is_some_and(|result| result["settled"] != true) {
                request(
                    &self.handlers,
                    stop,
                    "rlm.abort",
                    json!({ "target": lane.child }),
                )
                .await?;
                status = "timeout";
            }
            let patch = self.worktrees.diff(stop, &json!(lane.tree.name)).await?;
            let files = patch_files(&patch);
            let line_count = patch.matches('\n').count();
            let mut row = json!({
                "approach": lane.approach,
                "name": lane.tree.name,
                "child_status": status,
                // Absent values are #f in Scheme, never null.
                "answer_preview": settled
                    .and_then(|result| result["answer_preview"].as_str())
                    .map_or(json!(false), Value::from),
                "diff": {
                    "name": lane.tree.name,
                    "files": files,
                    "lines": line_count,
                    "text": patch,
                    "label": if patch.is_empty() {
                        format!("[diff:{} · empty]", lane.tree.name)
                    } else {
                        format!("[diff:{} · {} files · {line_count} lines]", lane.tree.name, files.len())
                    },
                },
                "worktree": lane.tree,
            });
            // A child that ran out of time fails without its check running.
            let verdict = match check {
                Some(command) if status != "timeout" => {
                    let shell = platform::resolve_kernel_bash_shell(None)?.ok_or_else(|| {
                        anyhow!(
                            "no POSIX shell was found (on Windows, install Git Bash in its default location)"
                        )
                    })?;
                    let mut process = tokio::process::Command::new(&shell);
                    process
                        // The check's stderr joins its stdout before the check
                        // is read, so even its syntax errors land in the tail.
                        .args(["-c", "exec 2>&1; eval \"$1\"", "check", command])
                        .current_dir(&lane.tree.path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);
                    #[cfg(unix)]
                    platform::process::set_new_session(process.as_std_mut());
                    #[cfg(not(unix))]
                    {
                        platform::set_new_process_group(process.as_std_mut());
                        platform::set_no_window(process.as_std_mut());
                    }
                    let child = process
                        .spawn()
                        .with_context(|| format!("could not start {shell}"))?;
                    let pid = child.id();
                    let finished = tokio::select! {
                        biased;
                        () = stop.cancelled() => None,
                        output = tokio::time::timeout(CHECK_TIMEOUT, child.wait_with_output()) => Some(output),
                    };
                    match finished {
                        Some(Ok(output)) => {
                            let output = output.context("could not wait for the check")?;
                            let text = String::from_utf8_lossy(&output.stdout);
                            let skipped = text.chars().count().saturating_sub(CHECK_TAIL_CHARS);
                            json!({
                                "passed": output.status.success(),
                                "check_exit": platform::normalized_exit_code(&output.status)
                                    .map_or(json!(false), Value::from),
                                "check_tail": text.chars().skip(skipped).collect::<String>(),
                            })
                        }
                        unfinished @ (Some(Err(_)) | None) => {
                            // The dropped wait killed the shell; whatever the
                            // check started goes with it.
                            if let Some(pid) = pid {
                                #[cfg(unix)]
                                let _ = platform::process::signal_process_group(
                                    pid as i32,
                                    platform::Signal::Kill,
                                );
                                #[cfg(not(unix))]
                                let _ = platform::kill_process_group_or_pid(pid as i32);
                            }
                            if unfinished.is_none() {
                                bail!("the check was stopped with its cell");
                            }
                            json!({
                                "passed": false,
                                "check_exit": false,
                                "check_tail": format!("check timed out after {}s", CHECK_TIMEOUT.as_secs()),
                            })
                        }
                    }
                }
                Some(_) | None => json!({ "passed": false, "check_exit": false, "check_tail": "" }),
            };
            for (field, value) in verdict.as_object().into_iter().flatten() {
                row[field] = value.clone();
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// Rank checked rows into the table, remove every worktree but the
    /// winner's, and report the run's counts to the host.
    async fn ranked(&self, task: &Value, mut rows: Vec<Value>) -> anyhow::Result<Value> {
        // Passing first, then a child that finished, then the smaller diff;
        // the sort is stable, so ties keep the order the approaches came in.
        rows.sort_by_key(|row| {
            (
                row["passed"] != true,
                row["child_status"] != "done",
                row["diff"]["lines"].as_u64().unwrap_or(0),
            )
        });
        let passed = rows.iter().filter(|row| row["passed"] == true).count();
        let won = rows.first().is_some_and(|row| row["passed"] == true);
        let mut removal = Ok(());
        let mut lines = vec![String::new()];
        for (index, row) in rows.iter_mut().enumerate() {
            row["rank"] = json!(index + 1);
            if index > 0 || !won {
                // Losers go whatever they hold; the cell's end does not stop
                // the cleanup.
                let removed = self
                    .worktrees
                    .remove(&AbortSignal::new(), &row["name"], true)
                    .await;
                removal = removal.and(removed);
                row["worktree"] = json!(false);
            }
            let verdict = if row["passed"] == true {
                "pass".to_string()
            } else {
                format!(
                    "fail({})",
                    row["check_exit"]
                        .as_i64()
                        .map_or_else(|| "none".to_string(), |code| code.to_string())
                )
            };
            let approach = row["approach"].as_str().unwrap_or_default();
            let approach = if approach.chars().count() <= 48 {
                approach.to_string()
            } else {
                format!("{}…", approach.chars().take(47).collect::<String>())
            };
            lines.push(format!(
                "{}. {}  {verdict}  child:{}  {}  {approach}",
                index + 1,
                row["name"].as_str().unwrap_or_default(),
                row["child_status"].as_str().unwrap_or_default(),
                row["diff"]["label"].as_str().unwrap_or_default(),
            ));
        }
        removal?;
        let winner = rows.first().filter(|_| won).cloned();
        lines[0] = format!(
            "[hypotheses] {} approaches · {passed} passed · winner: {}",
            rows.len(),
            winner.as_ref().map_or_else(
                || "none".to_string(),
                |row| format!("#1 {}", row["name"].as_str().unwrap_or_default())
            )
        );
        if let Some(handler) = self.handlers.get("hypotheses.record") {
            let data = json!({
                "approaches": rows.len(),
                "passed": passed,
                "winner": winner.is_some(),
            });
            // Adoption counts never fail the run they count.
            if let Err(error) = handler(HostRequestPayload { data }).await {
                tracing::warn!("hypotheses.record failed: {error:#}");
            }
        }
        Ok(json!({
            "task": task,
            "rows": rows,
            "winner": winner.unwrap_or(json!(false)),
            "text": lines.join("\n"),
        }))
    }

    /// Start a run; with a shell check, also rank it. A run that fails or is
    /// stopped midway aborts the children it started and removes its
    /// worktrees before the error is returned.
    async fn run(&self, stop: &AbortSignal, options: &Value) -> anyhow::Result<Value> {
        let mut lanes = Vec::new();
        let rows = match self.started(stop, options, &mut lanes).await {
            Ok(rows) => rows,
            Err(error) => {
                // The cell's end must not stop the cleanup of what it began.
                let cleanup = AbortSignal::new();
                for lane in &lanes {
                    if let Some(child) = &lane.child {
                        // The run's own error is the one to surface.
                        let _ = request(
                            &self.handlers,
                            &cleanup,
                            "rlm.abort",
                            json!({ "target": child }),
                        )
                        .await;
                    }
                    if let Err(left) = self
                        .worktrees
                        .remove(&cleanup, &json!(lane.tree.name), true)
                        .await
                    {
                        tracing::warn!("worktree {} was not removed: {left:#}", lane.tree.name);
                    }
                }
                return Err(error);
            }
        };
        if options["check"].is_null() {
            return Ok(json!({ "task": options["task"], "rows": rows }));
        }
        self.ranked(&options["task"], rows).await
    }
}

/// Define `%hypotheses-call` in a session's engine: `(operation
/// payload-json)` to the reply JSON its Scheme wrapper decodes. Must be
/// called from within a tokio runtime.
pub(super) fn register(
    engine: &mut Engine,
    handlers: HostRequestHandlers,
    worktrees: Arc<Worktrees>,
) {
    let runtime = tokio::runtime::Handle::current();
    let hypotheses = Hypotheses {
        handlers,
        worktrees,
    };
    engine.register_fn(
        "%hypotheses-call",
        move |operation: String, payload: String| -> String {
            // Outside a cell there is nothing to stop the call.
            let stop = crate::kernel::scheme::cell::current()
                .map_or_else(AbortSignal::new, |cell| cell.signal());
            let outcome = serde_json::from_str::<Value>(&payload)
                .map_err(|error| anyhow!("the payload is not JSON: {error}"))
                .and_then(|options| {
                    runtime.block_on(async {
                        match operation.as_str() {
                            "run" => hypotheses.run(&stop, &options).await,
                            "rank" => {
                                let rows = options["rows"].as_array().cloned().unwrap_or_default();
                                hypotheses.ranked(&options["task"], rows).await
                            }
                            unknown => Err(anyhow!("unknown hypotheses operation: {unknown}")),
                        }
                    })
                });
            super::reply_json("rlm-hypotheses", outcome)
        },
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use tokio::sync::mpsc::UnboundedSender;

    use super::super::worktree::tests::{evaluate, repository};
    use super::*;
    use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
    use crate::kernel::shared::host_handler;
    use crate::tools::kernel_cell::{
        ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
    };

    /// The approach number at the end of a lane's name (`hyp-ab12-3`) or of
    /// its child's id.
    fn lane_number(name: &Value) -> Value {
        json!(name.as_str().unwrap().rsplit('-').next().unwrap())
    }

    /// A host at rlm depth 1 of `max_depth` whose children do their approach
    /// at once: one that says "marker" writes `fixed.txt` into its checkout,
    /// one that says "big" also writes three more lines. Children whose
    /// approach says "slow" are reported unsettled. With `waiting`, the
    /// collect announces itself there and never answers. Returns the
    /// handlers and what they received, each reduced to what identifies it.
    fn host(
        max_depth: u64,
        waiting: Option<UnboundedSender<()>>,
    ) -> (HostRequestHandlers, Arc<Mutex<Vec<Value>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let slow = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "settings.session",
            host_handler(|_payload: HostRequestPayload| async { Ok(json!({"rlmDepth": 1})) }),
        );
        handlers.register(
            "settings.host",
            host_handler(move |_payload: HostRequestPayload| async move {
                Ok(json!({"rlmMaxDepth": {"maxDepth": max_depth, "source": "default"}}))
            }),
        );
        let (heard, slow_children) = (Arc::clone(&received), Arc::clone(&slow));
        handlers.register(
            "rlm.run",
            host_handler(move |payload: HostRequestPayload| {
                let prompt = payload.data["prompt"].as_str().unwrap().to_string();
                let name = payload.data["kwargs"]["name"].as_str().unwrap().to_string();
                let checkout = Path::new(payload.data["kwargs"]["cwd"].as_str().unwrap());
                let approach = prompt
                    .lines()
                    .find_map(|line| line.strip_prefix("Approach to take: "))
                    .unwrap()
                    .to_string();
                if approach.contains("marker") {
                    std::fs::write(checkout.join("fixed.txt"), "fixed\n").unwrap();
                }
                if approach.contains("big") {
                    std::fs::write(checkout.join("more.txt"), "a\nb\nc\n").unwrap();
                }
                if approach.contains("slow") {
                    slow_children.lock().unwrap().push(format!("child-{name}"));
                }
                heard.lock().unwrap().push(json!({
                    "type": "rlm.run",
                    "lane": lane_number(&json!(name)),
                    "approach": approach,
                    "names its checkout": prompt.contains(&format!("checkout: {}", checkout.display())),
                }));
                async move { Ok(json!({"rlm_child_id": format!("child-{name}"), "name": name})) }
            }),
        );
        handlers.register(
            "rlm.collect",
            host_handler(move |payload: HostRequestPayload| {
                let (waiting, slow) = (waiting.clone(), Arc::clone(&slow));
                async move {
                    if let Some(waiting) = waiting {
                        waiting.send(()).unwrap();
                        std::future::pending::<()>().await;
                    }
                    let slow = slow.lock().unwrap().clone();
                    let results: Vec<Value> = payload.data["targets"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|child| {
                            let unsettled = slow.iter().any(|slow| child == slow);
                            json!({
                                "rlm_child_id": child,
                                "status": if unsettled { "running" } else { "done" },
                                "settled": !unsettled,
                                "answer_preview": if unsettled { Value::Null } else { json!("did it") },
                            })
                        })
                        .collect();
                    Ok(json!({"results": results}))
                }
            }),
        );
        let heard = Arc::clone(&received);
        handlers.register(
            "rlm.abort",
            host_handler(move |payload: HostRequestPayload| {
                heard.lock().unwrap().push(
                    json!({"type": "rlm.abort", "lane": lane_number(&payload.data["target"])}),
                );
                async { Ok(json!({"outcome": "aborted"})) }
            }),
        );
        let heard = Arc::clone(&received);
        handlers.register(
            "hypotheses.record",
            host_handler(move |payload: HostRequestPayload| {
                heard.lock().unwrap().push(payload.data);
                async { Ok(json!({})) }
            }),
        );
        (handlers, received)
    }

    async fn kernel_in(root: &Path, handlers: HostRequestHandlers) -> Box<dyn KernelExecutor> {
        SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: repository(root),
            agent_dir: Some(root.join("agent")),
            artifact_dir: Some(root.join("sessions").join("s1")),
            ..SchemeKernelOptions::default()
        })
        .ensure(None, None)
        .await
        .unwrap()
    }

    /// A table reduced to what a run decides: each row's rank, approach,
    /// verdict, child status, check exit, changed files, answer, and whether
    /// its worktree was kept, then the winner's approach and the kept
    /// worktrees.
    fn decided(run: &str) -> String {
        format!(
            "(let ([table {run}])
               (list (map (lambda (row)
                            (list (hash-ref row 'rank)
                                  (hash-ref row 'approach)
                                  (hash-ref row 'passed)
                                  (hash-ref row 'child_status)
                                  (hash-ref row 'check_exit)
                                  (hash-ref (hash-ref row 'diff) 'files)
                                  (hash-ref row 'answer_preview)
                                  (if (hash-ref row 'worktree) #t #f)))
                          (hash-ref table 'rows))
                     (if (hash-ref table 'winner) (hash-ref (hash-ref table 'winner) 'approach) #f)
                     (length (worktree-list))
                     (length (split-many (hash-ref table 'text) \"\\n\"))))"
        )
    }

    /// Passing approaches rank first and the smaller diff wins among them; a
    /// child that ran out of time is aborted and fails unchecked, whatever
    /// its checkout holds; only the winner's worktree is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn approaches_are_ranked_by_a_shell_check_and_only_the_winner_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (handlers, received) = host(3, None);
        let kernel = kernel_in(&root, handlers).await;
        let table = evaluate(
            kernel.as_ref(),
            &decided(
                r#"(rlm-hypotheses "make the check pass"
                                   (list "change nothing" "big marker" "slow marker" "marker")
                                   #:check "test -f fixed.txt"
                                   #:timeout-ms 5000)"#,
            ),
        )
        .await;
        assert_eq!(
            (table, received.lock().unwrap().clone()),
            (
                Ok(json!([
                    [
                        [1, "marker", true, "done", 0, ["fixed.txt"], "did it", true],
                        [
                            2,
                            "big marker",
                            true,
                            "done",
                            0,
                            ["fixed.txt", "more.txt"],
                            "did it",
                            false
                        ],
                        [3, "change nothing", false, "done", 1, [], "did it", false],
                        [
                            4,
                            "slow marker",
                            false,
                            "timeout",
                            false,
                            ["fixed.txt"],
                            false,
                            false
                        ],
                    ],
                    "marker",
                    1,
                    5,
                ])),
                vec![
                    json!({"type": "rlm.run", "lane": "1", "approach": "change nothing", "names its checkout": true}),
                    json!({"type": "rlm.run", "lane": "2", "approach": "big marker", "names its checkout": true}),
                    json!({"type": "rlm.run", "lane": "3", "approach": "slow marker", "names its checkout": true}),
                    json!({"type": "rlm.run", "lane": "4", "approach": "marker", "names its checkout": true}),
                    json!({"type": "rlm.abort", "lane": "3"}),
                    json!({"approaches": 4, "passed": 2, "winner": true}),
                ],
            )
        );
    }

    /// A check given as a procedure runs in the cell on each checkout path;
    /// with nothing passing there is no winner and no worktree is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_procedure_check_decides_each_checkout_and_no_pass_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (handlers, received) = host(3, None);
        let kernel = kernel_in(&root, handlers).await;
        let mut tables = Vec::new();
        for check in [
            r#"(lambda (path) (pair? (hash-ref (fs-glob "fixed.txt" (hash 'path path)) 'entries)))"#,
            r#"(lambda (path) (if (pair? (hash-ref (fs-glob "fixed.txt" (hash 'path path)) 'entries)) 3 (error "no marker")))"#,
        ] {
            let run = format!(
                r#"(rlm-hypotheses "make the check pass" (list "change nothing" "marker") #:check {check})"#
            );
            tables.push(evaluate(kernel.as_ref(), &decided(&run)).await);
        }
        let records: Vec<Value> = received
            .lock()
            .unwrap()
            .iter()
            .filter(|payload| payload.get("type").is_none())
            .cloned()
            .collect();
        assert_eq!(
            (tables, records),
            (
                vec![
                    Ok(json!([
                        [
                            [1, "marker", true, "done", 0, ["fixed.txt"], "did it", true],
                            [2, "change nothing", false, "done", 1, [], "did it", false],
                        ],
                        "marker",
                        1,
                        3,
                    ])),
                    Ok(json!([
                        [
                            [
                                1,
                                "change nothing",
                                false,
                                "done",
                                false,
                                [],
                                "did it",
                                false
                            ],
                            [
                                2,
                                "marker",
                                false,
                                "done",
                                3,
                                ["fixed.txt"],
                                "did it",
                                false
                            ],
                        ],
                        false,
                        1,
                        3,
                    ])),
                ],
                vec![
                    json!({"approaches": 2, "passed": 1, "winner": true}),
                    json!({"approaches": 2, "passed": 0, "winner": false}),
                ],
            )
        );
    }

    /// The engine cannot interrupt the wait for the children, so the run
    /// ends on the cell's own signal, and not before it has stopped the
    /// children it started and removed their worktrees.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_run_aborts_its_children_and_removes_their_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (waiting, mut collecting) = tokio::sync::mpsc::unbounded_channel();
        let (handlers, received) = host(3, Some(waiting));
        let kernel = kernel_in(&root, handlers).await;
        let signal = AbortSignal::new();
        let running = kernel.execute(
            r#"(rlm-hypotheses "make the check pass" (list "marker" "big marker") #:check "test -f fixed.txt")"#,
            KernelExecuteOptions {
                signal: Some(signal.clone()),
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        );
        let abort = async {
            collecting.recv().await.unwrap();
            signal.cancel();
        };
        let (stopped, ()) = tokio::join!(running, abort);
        let aborted: Vec<Value> = received
            .lock()
            .unwrap()
            .iter()
            .filter(|payload| payload["type"] == "rlm.abort")
            .cloned()
            .collect();
        assert_eq!(
            (
                stopped.unwrap().status,
                aborted,
                evaluate(kernel.as_ref(), "(worktree-list)").await,
            ),
            (
                ExecuteStatus::Aborted,
                vec![
                    json!({"type": "rlm.abort", "lane": "1"}),
                    json!({"type": "rlm.abort", "lane": "2"}),
                ],
                Ok(json!([])),
            )
        );
    }

    /// A session at its depth bound, or a malformed run, is refused before
    /// any worktree is made or child spawned.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_that_cannot_have_children_is_refused_before_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (handlers, received) = host(1, None);
        let kernel = kernel_in(&root, handlers).await;
        let mut refused = Vec::new();
        for run in [
            r#"(rlm-hypotheses "task" (list "one way" "another") #:check "true")"#,
            r#"(rlm-hypotheses "task" (list "one way" "one way") #:check "true")"#,
            r#"(rlm-hypotheses "task" (list "one way" "another"))"#,
        ] {
            refused.push(evaluate(kernel.as_ref(), run).await);
        }
        refused.push(evaluate(kernel.as_ref(), "(worktree-list)").await);
        let raised = |reason: &str| Err(format!("Error: Generic:  rlm-hypotheses: {reason}"));
        assert_eq!(
            (refused, received.lock().unwrap().clone()),
            (
                vec![
                    raised("rlm-hypotheses needs one level of children, but this session is at rlm depth 1 of max 1"),
                    raised("approaches must be distinct"),
                    raised("#:check must be a shell command string or a procedure taking the checkout path"),
                    Ok(json!([])),
                ],
                Vec::<Value>::new(),
            )
        );
    }
}
