//! Edit-then-check: after an edit, the cheapest applicable check for the
//! touched file runs, and its diagnostics ride the edit's result, so a broken
//! edit is seen without another round.
//!
//! The check per file: Python gets `ruff check` limited to the fatal rules
//! (syntax errors, undefined names) when ruff is installed, else
//! `python -m py_compile`; Rust gets `cargo check -p <crate>` for the file's
//! own crate; TypeScript gets `tsc --noEmit` on the file's project; Go gets
//! `go vet` on the file's package. Files outside the session's working
//! directory, and other languages, get none.
//!
//! The edit waits for its check at most the check's learned budget. A check
//! that ends in time attaches its diagnostics (or `clean`) to the edit
//! result; a slower one lets the edit return at once and, if it finds
//! anything, its diagnostics arrive through the session's `bash.completed`
//! notice.
//!
//! What each check costs and finds is learned per project (decaying
//! averages, kept under the agent dir in the file the Python kernel's
//! `rlm.edit_check` wrote). A check whose cost exceeds its benefit (the model
//! round its diagnostics save, weighted by how often it finds any) is
//! dropped, and re-probed now and then so a project that changes can win it
//! back. Each outcome is reported to the session's `edit_check.fact` handler
//! without waiting for it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload, BASH_COMPLETED_REQUEST_TYPE};
use crate::platform;
use crate::tools::tool_definition::AbortSignal;

/// Decay of the learned averages per observation (the newest weighs the rest).
const DECAY: f64 = 0.7;
/// Runs before a check's average decides anything.
const LEARNING_RUNS: u64 = 3;
/// What a found diagnostic is worth: about one model round saved, in milliseconds.
const ROUND_MS: f64 = 20_000.0;
/// A dropped check still runs every this-many skipped edits, so its averages keep learning.
const REPROBE_EVERY: u64 = 8;
/// The wait inside the edit result: the learned cost with headroom, within these bounds.
const COLD_BUDGET_MS: f64 = 1_500.0;
const MIN_BUDGET_MS: f64 = 250.0;
const MAX_BUDGET_MS: f64 = 3_000.0;
const BUDGET_HEADROOM: f64 = 1.5;
/// A check still running after this is killed and counted as costly and fruitless.
const HARD_LIMIT: Duration = Duration::from_secs(120);
/// Bounds of the diagnostics shown.
const SHOWN_LINES: usize = 40;
const SHOWN_CHARS: usize = 4_000;
/// Bytes of a check's output that are kept; the rest is read and dropped.
const OUTPUT_CAP: usize = 256 * 1024;
/// How long a late notice may wait for the session to take it.
const NOTICE_WAIT: Duration = Duration::from_secs(5);
const SCHEMA: u32 = 1;
/// The fatal ruff rules: syntax errors, undefined names, invalid comparisons and statements.
const RUFF_FATAL: &str = "E9,F63,F7,F82";
/// The request each check outcome is reported with.
const FACT_REQUEST: &str = "edit_check.fact";

/// One check to run: its kind (the key its averages are learned under), its
/// command line, and where it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    kind: &'static str,
    argv: Vec<String>,
    cwd: PathBuf,
}

/// One check kind's learned averages in one project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Stats {
    cost_ms: f64,
    found: f64,
    runs: u64,
    skips: u64,
}

impl Stats {
    fn observe(&mut self, cost_ms: f64, found: bool) {
        let found = if found { 1.0 } else { 0.0 };
        if self.runs == 0 {
            self.cost_ms = cost_ms;
            self.found = found;
        } else {
            self.cost_ms = self.cost_ms * DECAY + cost_ms * (1.0 - DECAY);
            self.found = self.found * DECAY + found * (1.0 - DECAY);
        }
        self.runs += 1;
        self.skips = 0;
    }

    fn worth_running(&self) -> bool {
        self.runs < LEARNING_RUNS
            || self.cost_ms <= self.found * ROUND_MS
            || self.skips + 1 >= REPROBE_EVERY
    }

    fn budget(&self) -> Duration {
        let millis = if self.runs == 0 {
            COLD_BUDGET_MS
        } else {
            (self.cost_ms * BUDGET_HEADROOM).clamp(MIN_BUDGET_MS, MAX_BUDGET_MS)
        };
        Duration::from_secs_f64(millis / 1000.0)
    }
}

/// The learned state as stored: the Python kernel's file, field for field.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Stored {
    schema: u32,
    checks: BTreeMap<String, Stats>,
}

/// A project's learned stats and the file they are kept in.
#[derive(Debug, Default)]
struct Learned {
    path: Option<PathBuf>,
    checks: BTreeMap<String, Stats>,
}

/// One check run; `pid` is 0 and `exit_code` -1 when it could not start or
/// was killed at the hard limit.
#[derive(Debug)]
struct Outcome {
    pid: u32,
    exit_code: i32,
    output: String,
    elapsed: Duration,
}

impl Outcome {
    fn found(&self) -> bool {
        self.exit_code != 0 && !self.output.trim().is_empty()
    }
}

/// The file one project's learned stats are kept in under `state_dir`: the
/// name the Python kernel gave it, from a digest of the project's root.
fn stats_file(state_dir: &Path, root: &Path) -> PathBuf {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let name = digest[..8].iter().fold(String::new(), |mut name, byte| {
        let _ = write!(name, "{byte:02x}");
        name
    });
    state_dir.join(format!("{name}.json"))
}

/// A program on `PATH`.
fn find_program(name: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::split_paths(&pa_types::session_scope::var_os("PATH")?)
        .flat_map(|directory| {
            suffixes
                .iter()
                .map(move |suffix| directory.join(format!("{name}{suffix}")))
        })
        .find(|candidate| candidate.is_file() && platform::is_executable(candidate))
}

/// The nearest directory from `start` up to `root` holding the file `name`.
fn nearest(start: &Path, root: &Path, name: &str) -> Option<PathBuf> {
    let mut directory = start;
    loop {
        if directory.join(name).is_file() {
            return Some(directory.to_path_buf());
        }
        if directory == root {
            return None;
        }
        directory = directory.parent()?;
    }
}

/// The cheapest applicable check for `path` (inside `root`), or none.
fn choose(path: &Path, root: &Path) -> Option<Check> {
    let directory = path.parent()?;
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let check = |kind, program: &Path, arguments: &[&str], cwd: &Path| Check {
        kind,
        argv: std::iter::once(text(program))
            .chain(arguments.iter().map(ToString::to_string))
            .collect(),
        cwd: cwd.to_path_buf(),
    };
    let file = text(path);
    match path.extension()?.to_str()? {
        "py" => Some(match find_program("ruff") {
            Some(ruff) => check(
                "ruff",
                &ruff,
                &[
                    "check",
                    "--isolated",
                    "--quiet",
                    "--output-format",
                    "concise",
                    "--select",
                    RUFF_FATAL,
                    &file,
                ],
                root,
            ),
            None => check(
                "py_compile",
                &find_program("python3").or_else(|| find_program("python"))?,
                &["-m", "py_compile", &file],
                root,
            ),
        }),
        "rs" => {
            let manifest = nearest(directory, root, "Cargo.toml")?.join("Cargo.toml");
            let cargo = find_program("cargo")?;
            let manifest: toml::Value =
                toml::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
            let name = manifest.get("package")?.get("name")?.as_str()?;
            Some(check(
                "cargo",
                &cargo,
                &["check", "--quiet", "--message-format", "short", "-p", name],
                root,
            ))
        }
        "ts" | "tsx" | "mts" | "cts" => {
            let project = nearest(directory, root, "tsconfig.json")?;
            let tsc = [&project, root]
                .into_iter()
                .map(|base| base.join("node_modules").join(".bin").join("tsc"))
                .find(|local| local.is_file())
                .or_else(|| find_program("tsc"))?;
            Some(check(
                "tsc",
                &tsc,
                &["--noEmit", "--pretty", "false", "-p", &text(&project)],
                root,
            ))
        }
        "go" => {
            let module = nearest(directory, root, "go.mod")?;
            let go = find_program("go")?;
            let package: Vec<String> = directory
                .strip_prefix(&module)
                .ok()?
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            let package = if package.is_empty() {
                "./.".to_string()
            } else {
                format!("./{}", package.join("/"))
            };
            Some(check("go_vet", &go, &["vet", &package], &module))
        }
        _ => None,
    }
}

/// Run one check to its end, or to the hard limit.
async fn run(check: Check) -> Outcome {
    let started = Instant::now();
    let failed = |started: Instant| Outcome {
        pid: 0,
        exit_code: -1,
        output: String::new(),
        elapsed: started.elapsed(),
    };
    // One pipe carries stdout and stderr, so their order is the check's own.
    let Ok((mut output, output_end)) = std::io::pipe() else {
        return failed(started);
    };
    let Ok(error_end) = output_end.try_clone() else {
        return failed(started);
    };
    let mut command = pa_types::session_scope::tokio_command(&check.argv[0]);
    command
        .args(&check.argv[1..])
        .current_dir(&check.cwd)
        .stdin(Stdio::null())
        .stdout(output_end)
        .stderr(error_end)
        .kill_on_drop(true);
    #[cfg(unix)]
    platform::process::set_new_session(command.as_std_mut());
    #[cfg(not(unix))]
    {
        platform::set_new_process_group(command.as_std_mut());
        platform::set_no_window(command.as_std_mut());
    }
    let Ok(mut child) = command.spawn() else {
        return failed(started);
    };
    // `command` holds this process's copies of the pipe's writing end: the
    // output can only end once they are closed.
    drop(command);
    let pid = child.id().unwrap_or(0);
    let reading = tokio::task::spawn_blocking(move || {
        let mut kept = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match output.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let take = (OUTPUT_CAP - kept.len()).min(read);
                    kept.extend_from_slice(&buffer[..take]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        kept
    });
    if let Ok(Ok(status)) = tokio::time::timeout(HARD_LIMIT, child.wait()).await {
        return Outcome {
            pid,
            exit_code: platform::normalized_exit_code(&status).unwrap_or(-1),
            output: String::from_utf8_lossy(&reading.await.unwrap_or_default()).into_owned(),
            elapsed: started.elapsed(),
        };
    }
    #[cfg(unix)]
    let _ = platform::process::signal_process_group(pid as i32, platform::Signal::Kill);
    #[cfg(not(unix))]
    if !platform::kill_process_group_or_pid(pid as i32) {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
    failed(started)
}

/// The diagnostics shown: the first lines of `output`, bounded, and how many
/// lines were left out.
fn shown(output: &str) -> String {
    let lines: Vec<&str> = output.trim().lines().collect();
    let text: String = lines
        .iter()
        .take(SHOWN_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(SHOWN_CHARS)
        .collect();
    let hidden = lines.len().saturating_sub(text.lines().count());
    if hidden > 0 {
        format!("{text}\n[{hidden} more lines]")
    } else {
        text
    }
}

/// One session kernel's checks after edits.
#[derive(Clone)]
pub(super) struct EditChecks {
    runtime: tokio::runtime::Handle,
    /// The session's handlers: `edit_check.fact` hears each outcome and
    /// `bash.completed` takes a late check's diagnostics, where registered.
    handlers: HostRequestHandlers,
    /// Where learned stats are kept (`<agent dir>/edit-checks`); without one
    /// they last as long as the kernel.
    state_dir: Option<PathBuf>,
    learned: Arc<Mutex<Option<Learned>>>,
}

impl EditChecks {
    pub(super) fn new(
        runtime: tokio::runtime::Handle,
        handlers: HostRequestHandlers,
        state_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            runtime,
            handlers,
            state_dir,
            learned: Arc::default(),
        }
    }

    /// Count one check outcome on the session, without waiting for it.
    fn note(&self, outcome: &'static str, kind: &'static str) {
        if let Some(handler) = self.handlers.get(FACT_REQUEST) {
            let sent = handler(HostRequestPayload {
                data: json!({"type": FACT_REQUEST, "outcome": outcome, "kind": kind}),
            });
            self.runtime.spawn(async move {
                if let Err(error) = sent.await {
                    tracing::debug!("edit check fact was refused: {error:#}");
                }
            });
        }
    }

    /// Learn from a finished run, keep what was learned (a small file
    /// written here: call it off the async runtime), and count the run.
    fn finish(&self, check: &Check, outcome: &Outcome) {
        let stored = {
            let mut learned = self.learned.lock().unwrap_or_else(PoisonError::into_inner);
            let learned = learned.get_or_insert_with(Learned::default);
            learned
                .checks
                .entry(check.kind.to_string())
                .or_default()
                .observe(outcome.elapsed.as_secs_f64() * 1000.0, outcome.found());
            learned.path.clone().map(|path| {
                (
                    path,
                    Stored {
                        schema: SCHEMA,
                        checks: learned.checks.clone(),
                    },
                )
            })
        };
        if let Some((path, stored)) = stored {
            // A failed write is replaced by the next one.
            let written = serde_json::to_vec(&stored)
                .map_err(std::io::Error::other)
                .and_then(|body| {
                    if let Some(directory) = path.parent() {
                        std::fs::create_dir_all(directory)?;
                    }
                    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
                    std::fs::write(&temp, body)?;
                    std::fs::rename(&temp, &path)
                });
            if let Err(error) = written {
                tracing::debug!("edit check stats were not saved: {error}");
            }
        }
        self.note(
            match (outcome.pid, outcome.found()) {
                (0, _) => "failed",
                (_, true) => "found",
                (_, false) => "clean",
            },
            check.kind,
        );
    }

    /// Run the check of the file at `path`, just edited, waiting at most its
    /// learned budget (or until `cell` stops): the line(s) to attach to the
    /// edit's result, or none when the file gets no check, its check was
    /// dropped, or it could not start.
    pub(super) fn after_edit(&self, cwd: &Path, path: &Path, cell: &AbortSignal) -> Option<String> {
        let root = cwd.canonicalize().ok()?;
        let target = path.canonicalize().ok()?;
        if !target.is_file() || !target.starts_with(&root) {
            return None;
        }
        let check = choose(&target, &root)?;
        let budget = {
            let mut learned = self.learned.lock().unwrap_or_else(PoisonError::into_inner);
            let learned = learned.get_or_insert_with(|| {
                let path = self
                    .state_dir
                    .as_ref()
                    .map(|directory| stats_file(directory, &root));
                let checks = path
                    .as_ref()
                    .and_then(|path| std::fs::read(path).ok())
                    .and_then(|body| serde_json::from_slice::<Stored>(&body).ok())
                    .filter(|stored| stored.schema == SCHEMA)
                    .map(|stored| stored.checks)
                    .unwrap_or_default();
                Learned { path, checks }
            });
            let stats = learned.checks.entry(check.kind.to_string()).or_default();
            if stats.worth_running() {
                Some(stats.budget())
            } else {
                stats.skips += 1;
                None
            }
        };
        let Some(budget) = budget else {
            self.note("skipped", check.kind);
            return None;
        };
        let label = check.argv.join(" ");
        let mut running = self.runtime.spawn(run(check.clone()));
        let in_time = self.runtime.block_on(async {
            tokio::select! {
                biased;
                joined = &mut running => Some(joined),
                () = tokio::time::sleep(budget) => None,
                () = cell.cancelled() => None,
            }
        });
        let Some(joined) = in_time else {
            self.note("late", check.kind);
            let notices = self.handlers.get(BASH_COMPLETED_REQUEST_TYPE).cloned();
            let delivery = if notices.is_some() {
                "any diagnostics arrive as a [bash-done] notice"
            } else {
                "this session takes no late notices, so its diagnostics are dropped"
            };
            let edited = target
                .strip_prefix(&root)
                .unwrap_or(&target)
                .to_string_lossy()
                .replace('\\', "/");
            let late = self.clone();
            let command = format!("{label}  # check after editing {edited}");
            self.runtime.spawn(async move {
                let finished = match running.await {
                    Ok(outcome) => {
                        tokio::task::spawn_blocking(move || {
                            late.finish(&check, &outcome);
                            outcome
                        })
                        .await
                    }
                    Err(error) => Err(error),
                };
                let outcome = match finished {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::debug!("a late edit check did not finish: {error}");
                        return;
                    }
                };
                let (true, Some(notify)) = (outcome.found(), notices) else {
                    return;
                };
                let notice = notify(HostRequestPayload {
                    data: json!({
                        "type": BASH_COMPLETED_REQUEST_TYPE,
                        "pid": outcome.pid,
                        "command": command,
                        "exitCode": outcome.exit_code,
                        "output": shown(&outcome.output),
                        "outputTruncated": outcome.output.trim().lines().count() > SHOWN_LINES,
                    }),
                });
                match tokio::time::timeout(NOTICE_WAIT, notice).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::debug!("a late edit check's notice was refused: {error:#}");
                    }
                    Err(_) => tracing::debug!("a late edit check's notice was not taken in time"),
                }
            });
            return Some(format!(
                "[check: {label} · still running after {:.1} s; {delivery}]",
                budget.as_secs_f64()
            ));
        };
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::debug!("an edit check did not finish: {error}");
                return None;
            }
        };
        self.finish(&check, &outcome);
        let seconds = outcome.elapsed.as_secs_f64();
        match (outcome.pid, outcome.found()) {
            (0, _) => None,
            (_, false) => Some(format!("[check: {label} · clean · {seconds:.1} s]")),
            (_, true) => Some(format!(
                "[check: {label} · exit {} · {seconds:.1} s]\n{}",
                outcome.exit_code,
                shown(&outcome.output)
            )),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A check that costs more than it finds is dropped, and probed again on
    /// its eighth skipped edit; what the probe learns replaces what was kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_costly_fruitless_check_is_dropped_then_reprobed_and_what_it_learns_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(root.join("app.ts"), "let total = 1\n").unwrap();
        let bin = root.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        // A child process writes the script, so no thread of this one can
        // hold it open for writing while it is being started.
        let written = pa_types::session_scope::std_command("sh")
            .arg("-c")
            .arg("printf '#!/bin/sh\\nexit 0\\n' > tsc && chmod +x tsc")
            .current_dir(&bin)
            .status()
            .unwrap();
        assert!(written.success());
        let kept = stats_file(state.path(), &root);
        std::fs::write(
            &kept,
            r#"{"schema": 1, "checks": {"tsc": {"cost_ms": 60000.0, "found": 0.0, "runs": 5, "skips": 6}}}"#,
        )
        .unwrap();
        let checks = EditChecks::new(
            tokio::runtime::Handle::current(),
            HostRequestHandlers::new(),
            Some(state.path().to_path_buf()),
        );
        let file = root.join("app.ts");
        let (dropped, probed) = tokio::task::spawn_blocking(move || {
            let cell = AbortSignal::new();
            let dropped = checks.after_edit(&root, &file, &cell);
            (dropped, checks.after_edit(&root, &file, &cell))
        })
        .await
        .unwrap();
        let stored: Stored = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
        let learned = stored.checks["tsc"];
        assert_eq!(
            (
                dropped,
                probed.is_some_and(|check| check.contains(" · clean · ")),
                stored.schema,
                learned.runs,
                learned.skips,
            ),
            (None, true, 1, 6, 0)
        );
    }
}
