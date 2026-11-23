//! `bash` and its jobs: shell commands run in the session's working
//! directory, in the foreground of their cell or in the background.
//!
//! A command runs on a shell process of its own (`bash_job.rs`) or on the
//! built-in shell inside this process (`bash_engine.rs`), as the session's
//! settings say; everything below holds for both.
//!
//! - `(bash command [options])` starts the command and waits for it. It
//!   returns its result: `'running` `#f`, `'exit-code`, `'output` (stdout and
//!   stderr together, in the order written), `'truncated`, `'timed-out`,
//!   `'duration-ms`, `'cached`, `'job`, `'pid`, `'handle` (the output's
//!   `out:<id>` spill handle when it is large and the session keeps a store,
//!   for the `ctx-*` readers), and `'tests` (a test run's summary, also
//!   printed). A failing command is a result, not an error.
//!
//!   In a session that delivers `[bash-done]` notices the wait has a window
//!   (10 s unless `PRIME_AGENT_BASH_WAIT_MS` says otherwise, at most 600 s,
//!   and ending before the cell's own time limit). A command that outlives it
//!   keeps running in the background and the call returns a snapshot instead:
//!   `'running` `#t`, `'exit-code` `#f`, `'output` (its newest 20 lines),
//!   `'output-truncated`, `'duration-ms`, `'job`, `'pid`, `'queued`,
//!   `'command`, `'note`. Its result arrives with the `[bash-done]` notice or
//!   through `bash-wait`. Without notices the wait has no window.
//!
//!   Options: `'wait-ms` (this call's window, in any session; `#f` for
//!   none), `'timeout-ms` (kill the command after this long, and wait for it
//!   until then without a window; default none),
//!   `'background` (`#t`: do not wait, return the job: `'job`, `'pid`,
//!   `'command`, `'running`, `'queued`, `'persistent`), `'persistent` (`#t`:
//!   a shell that outlives the kernel, see `bash_persistent.rs`).
//! - `(bash-wait job [seconds])` waits for a job, for `seconds` or the
//!   session's window, and returns its result or a snapshot.
//! - `(bash-poll job)` returns a finished job's result, or `#f`.
//! - `(bash-tail job [lines])` and `(bash-output job)` return what a job has
//!   written so far (50 lines by default), output after its result included.
//! - `(bash-kill job [options])` signals the job's whole process group and
//!   returns whether it was still running. Options: `'signal` (`'term`, the
//!   default, or `'kill`), `'grace-ms` (how long after a `'term` the group is
//!   killed; default 5000).
//! - `(bash-running? job)` is whether anything of the job still runs.
//! - `(bash-jobs)` lists the kernel's jobs, and `(bash-persistent-shells)`
//!   its persistent shells, re-attached ones included.
//!
//! A job argument is a job id or any hash these functions return.
//!
//! A command the call waits for is owned by its cell while the wait lasts:
//! a cell that is aborted or times out kills it. A background job, a
//! snapshot's job, and a persistent shell are not: they outlive their cell.
//! Over the session's job limit (`PRIME_AGENT_BASH_MAX_JOBS`) a background
//! command is held `queued` until a slot frees; waiting for it from the cell
//! that started it starts it at once. Killing the kernel, or dropping it,
//! kills every job but the persistent shells.
//!
//! A job that ends after its cell, unread, is announced to the session
//! through the `bash.completed` host request (the `[bash-done]` notice);
//! reading its result later withdraws the notice through `bash.consumed`.
//! A command that was still running when its kernel ended (the kernel was
//! killed, or the process hosting the session died) is announced by the
//! session's next kernel, once, through the same request with `lost` set
//! (the `[bash-lost]` notice); only a saved session keeps the list for it.

#[path = "bash_cache.rs"]
mod cache;
#[path = "bash_engine.rs"]
mod engine;
#[path = "bash_job.rs"]
mod job;
#[path = "bash_persistent.rs"]
mod persistent;
#[path = "bash_summary.rs"]
mod summary;
#[cfg(test)]
#[path = "bash_tests.rs"]
mod tests;
#[cfg(windows)]
#[path = "bash_winjob.rs"]
mod winjob;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};
use tokio::sync::watch;

use self::job::{lock, FinishHook, Finished, Job, Notice, Table};
use crate::kernel::orphan_journal::record_orphan_process_state;
use crate::kernel::scheme::cell::{self, CellRecord};
use crate::kernel::scheme::session::{BackgroundWork, LiveWork};
use crate::kernel::scheme::SessionEnvironment;
use crate::kernel::shared::{
    HostRequestHandlers, HostRequestPayload, KernelCommandFailure, BASH_COMPLETED_REQUEST_TYPE,
};
use crate::kernel::spill::SpillStore;
use crate::platform::Signal;
use crate::tools::tool_definition::AbortSignal;

/// The Scheme side of `bash` and its jobs, loaded after the native bridge.
pub(super) const SCHEME: &str = r#"
(define (%bash-job who job)
  (cond [(string? job) job]
        [(and (hash? job) (hash-contains? job 'job) (string? (hash-ref job 'job))) (hash-ref job 'job)]
        [else (%native-raise (string-append who ": expected a job id or a hash with a 'job"))]))

;; A newly parsed test summary prints with the cell's output.
(define (%bash-settled result)
  (if (and (hash? result) (hash-contains? result 'announce))
      (begin (displayln (hash-ref result 'announce)) (hash-remove result 'announce))
      result))

(define (bash command [options (hash)])
  (%bash-settled (%native "bash" (hash-insert options 'command command))))
(define (bash-wait job [seconds #f])
  (%bash-settled (%native "bash-wait" (hash 'job (%bash-job "bash-wait" job) 'seconds seconds))))
(define (bash-poll job) (%native "bash-poll" (hash 'job (%bash-job "bash-poll" job))))
(define (bash-tail job [lines 50])
  (%native "bash-tail" (hash 'job (%bash-job "bash-tail" job) 'lines lines)))
(define (bash-output job) (%native "bash-output" (hash 'job (%bash-job "bash-output" job))))
(define (bash-kill job [options (hash)])
  (%native "bash-kill" (hash-insert options 'job (%bash-job "bash-kill" job))))
(define (bash-running? job) (%native "bash-running" (hash 'job (%bash-job "bash-running?" job))))
(define (bash-jobs) (%native "bash-jobs" (hash)))
(define (bash-persistent-shells) (%native "bash-persistent-shells" (hash)))

;; Wait inside the turn until `check` holds: {status, value, checks,
;; waited-ms}. `check` is a procedure of no arguments (it holds when it
;; returns anything but #f, and that is the value) or a shell command (it
;; holds when it exits 0; the value is its last result, so a timeout still
;; carries what it printed). It is checked at once, then every #:every
;; seconds until #:timeout seconds (default 60, at most 3600) have passed;
;; status is "met" or "timed_out". The wait prints a line when it starts,
;; every 30 seconds, and when it ends, and the cell's yield window does not
;; run while it waits (`%cell` "hold"). Defined here, after `bash`, which its
;; shell-command form runs.
(define (wait-until check #:timeout [timeout 60] #:every [every 1] #:label [label #f])
  (unless (or (procedure? check) (string? check))
    (error "wait-until: check is a procedure of no arguments or a shell command string"))
  (unless (and (number? timeout) (> timeout 0) (<= timeout 3600))
    (error "wait-until: #:timeout is more than 0 and at most 3600 seconds"))
  (unless (and (number? every) (>= every 0.05))
    (error "wait-until: #:every is at least 0.05 seconds"))
  (let* ([test (if (string? check)
                   (lambda ()
                     (let ([run (bash check)])
                       (cons (equal? (hash-ref run 'exit-code) 0) run)))
                   (lambda () (let ([value (check)]) (cons value value))))]
         [what (cond [(string? label) label] [(string? check) check] [else "the condition"])]
         [start (current-inexact-milliseconds)]
         [end (+ start (* timeout 1000))]
         [seconds (lambda ()
                    (number->string
                     (exact (round (/ (- (current-inexact-milliseconds) start) 1000)))))])
    (displayln (string-append "waiting up to " (number->string timeout) " s for " what))
    ;; The last check may start at the timeout: a command's own wait is 10 s.
    (%cell "hold" (+ timeout 15))
    (let loop ([checks 1] [said start])
      (let* ([checked (test)]
             [met (car checked)]
             [now (current-inexact-milliseconds)])
        (cond
          [(or met (>= now end))
           (let ([status (if met "met" "timed_out")])
             (%cell "release" status)
             (displayln (string-append what (if met ": met after " ": timed out after ")
                                       (seconds) " s"))
             (hash 'status status
                   'value (cdr checked)
                   'checks checks
                   'waited-ms (exact (round (- now start)))))]
          [else
           (let ([said (if (>= (- now said) 30000)
                           (begin (displayln (string-append "still waiting for " what ": "
                                                            (seconds) " s"))
                                  now)
                           said)])
             (time/sleep-ms (exact (round (min (* every 1000) (- end now)))))
             (loop (+ checks 1) said))])))))
"#;

/// The wait window of a session that delivers completion notices.
const DEFAULT_WINDOW: Duration = Duration::from_secs(10);
/// The longest window any wait gets.
const MAX_WINDOW: Duration = Duration::from_secs(600);
/// Headroom between a wait's yield and its cell's time limit: the snapshot
/// must be back before the cell is interrupted.
const CELL_MARGIN: Duration = Duration::from_secs(2);
/// How long a stopped command has to end on SIGTERM before SIGKILL, and how
/// long its group may then take to be confirmed gone.
const TERM_GRACE: Duration = Duration::from_millis(500);
const KILL_WAIT: Duration = Duration::from_secs(2);
/// How much of its newest output a snapshot or a notice carries.
const TAIL_LINES: usize = 20;
const TAIL_CHARS: usize = 2000;
/// How much of a command a notice or a display record carries.
const COMMAND_CAP: usize = 1000;
/// The most bytes one reply to the user's look at the commands may take on
/// the wire, and how long a command the user stops has to end before it is
/// killed.
const ACTIVITY_REPLY_BYTES: usize = 16_384;
const ACTIVITY_KILL_GRACE: Duration = Duration::from_secs(5);
/// How long a wait gives the host to store a test run's outputs before it
/// returns the summary without handles.
const HANDLE_WAIT: Duration = Duration::from_secs(2);

/// The session settings commands run under.
struct BashSettings {
    /// Whether commands run on the built-in shell or a shell process.
    engine: crate::platform::KernelShell,
    /// The absolute path of the session's shell process; the platform's
    /// default one without it.
    shell: Option<String>,
    /// Shell text run before each command.
    command_prefix: Option<String>,
    /// What every command gets on top of the process's own environment.
    env: Vec<(String, String)>,
    /// `PRIME_AGENT_BASH_MAX_JOBS`: commands running at once.
    max_jobs: usize,
    /// `PRIME_AGENT_BASH_WAIT_MS`: the default wait window.
    wait: Option<Duration>,
    /// `RLM_SESSION_ID`: the saved session persistent shells are assigned to.
    session_id: Option<String>,
    /// Where persistent shells live.
    agent_dir: Option<PathBuf>,
}

/// What a session gives its commands.
struct BashHost {
    /// The session's host requests: `bash.completed` and `bash.consumed`
    /// (notices), `command_cache.fact` and `tests.summary`.
    handlers: HostRequestHandlers,
    /// The session's spill store, when it is saved: large outputs get a
    /// handle there.
    spill: Option<SpillStore>,
    settings: BashSettings,
}

/// The kernel's cells, as far as commands care.
#[derive(Default)]
struct Cells {
    /// How many cells have started: the running cell's number.
    current: u64,
    /// When the running cell is interrupted, if it has a time limit.
    deadline: Option<Instant>,
}

/// One session's commands: its jobs, cells, cache, and host.
pub(super) struct Session {
    runtime: tokio::runtime::Handle,
    cwd: PathBuf,
    host: BashHost,
    table: Mutex<Table>,
    cells: Mutex<Cells>,
    /// The number of the last cell that ended.
    cell_ended: watch::Sender<u64>,
    cache: Arc<Mutex<cache::CommandCache>>,
    /// The session's count of live background work: a command is in it from
    /// its start until its completion notice is handed over or not due.
    background: BackgroundWork,
    /// Cancelled when the kernel is killed.
    killed: AbortSignal,
    /// A saved session's list of its running commands ([`job::RUNNING_FILE`]).
    running_list: Option<PathBuf>,
}

/// The kernel's hold on its session's commands. Dropping it kills them, as
/// [`BashJobs::kill_all`] does: a kernel never leaves a job behind.
pub(in crate::kernel::scheme) struct BashJobs(Arc<Session>);

impl BashJobs {
    /// A cell starts on the engine; it is interrupted at `deadline` when it
    /// has a time limit that stops it.
    pub(in crate::kernel::scheme) fn cell_started(&self, deadline: Option<Instant>) {
        let mut cells = lock(&self.0.cells);
        cells.current += 1;
        cells.deadline = deadline;
    }

    /// The running cell ended: jobs it left unread are announced from now on.
    pub(in crate::kernel::scheme) fn cell_finished(&self) {
        let mut cells = lock(&self.0.cells);
        cells.deadline = None;
        self.0.cell_ended.send_replace(cells.current);
    }

    /// Kill every job but the persistent shells, and refuse new ones.
    pub(in crate::kernel::scheme) fn kill_all(&self) {
        let jobs: Vec<Arc<Job>> = {
            let mut table = lock(&self.0.table);
            table.closed = true;
            table.jobs.values().cloned().collect()
        };
        self.0.killed.cancel();
        for job in jobs {
            if job.lasting.is_none() && !lock(&job.state).reaped && job.destroy() {
                record_orphan_process_state(job.pid as i32, false);
            }
        }
    }
}

impl BashJobs {
    /// Answer the user's look at the session's commands, off the engine: the
    /// reply of [`crate::kernel::scheme::SchemeKernelProvisioner::bash_activity`],
    /// whose arguments it has checked. A reply stays under
    /// [`ACTIVITY_REPLY_BYTES`]: a list drops finished commands first, then
    /// the oldest; a tail drops its oldest text.
    ///
    /// # Errors
    ///
    /// `tail` or `kill` names a command the kernel does not have, or a
    /// persistent shell's output could not be read.
    pub(in crate::kernel::scheme) fn activity(
        &self,
        action: &str,
        activity_id: Option<&str>,
        lines: usize,
    ) -> anyhow::Result<Value> {
        let fits = |reply: &Value| reply.to_string().len() <= ACTIVITY_REPLY_BYTES;
        let Some(id) = activity_id else {
            let mut rows: Vec<Value> = {
                let table = lock(&self.0.table);
                table
                    .order
                    .iter()
                    .filter_map(|id| table.jobs.get(id))
                    .map(|job| {
                        let state = lock(&job.state);
                        let duration = state
                            .finished
                            .as_ref()
                            .map_or_else(|| job.started.elapsed(), |finished| finished.duration);
                        json!({
                            "id": job.id,
                            "command": job.command.chars().take(512).collect::<String>(),
                            "pid": job.pid,
                            "startedAt": job.started_at,
                            "durationMs": duration.as_millis() as u64,
                            "status": if state.reaped { "finished" } else { "running" },
                            "exitCode": state.finished.as_ref().map(|finished| finished.exit_code),
                            "persistent": job.lasting.is_some(),
                        })
                    })
                    .collect()
            };
            while rows.len() > 1 && !fits(&json!({ "activities": rows })) {
                let dropped = rows
                    .iter()
                    .position(|row| row["status"] != json!("running"))
                    .unwrap_or(0);
                rows.remove(dropped);
            }
            return Ok(json!({ "activities": rows }));
        };
        let job = lock(&self.0.table)
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown kernel bash activity"))?;
        if action == "kill" {
            let running = !lock(&job.state).reaped;
            if running {
                job.kill(&self.0.runtime, Signal::Term, ACTIVITY_KILL_GRACE);
            }
            return Ok(json!({ "activityId": id, "killed": running }));
        }
        let text = job.text()?;
        let all: Vec<&str> = text.lines().collect();
        let mut tail = all[all.len().saturating_sub(lines)..].join("\n");
        loop {
            let size = json!({ "activityId": id, "tail": tail }).to_string().len();
            if size <= ACTIVITY_REPLY_BYTES {
                break;
            }
            // Escaping can make one character six bytes: drop the oldest
            // sixth of what is over, at least one character, and look again.
            let dropped = ((size - ACTIVITY_REPLY_BYTES) / 6 + 1).min(tail.chars().count());
            tail = tail.chars().skip(dropped).collect();
        }
        Ok(json!({ "activityId": id, "tail": tail }))
    }
}

impl Drop for BashJobs {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// How long a wait may last before it yields a snapshot.
enum Window {
    /// The session's own: a window only where notices are delivered.
    Session,
    Unbounded,
    For(Duration),
}

/// Whether a wait's cell owns the command: a stopped cell kills an owned one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Owned,
    Shared,
}

/// How a wait ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Finished,
    Yielded,
    TimedOut,
    CellStopped,
}

/// Put `record` on the card of the cell this thread runs, if it runs one.
fn show(record: CellRecord) {
    if let Some(cell) = cell::current() {
        cell.record(record);
    }
}

/// `command` as a notice or a failure record carries it.
fn capped(command: &str) -> String {
    if command.chars().count() <= COMMAND_CAP {
        return command.to_string();
    }
    let kept: String = command.chars().take(COMMAND_CAP).collect();
    format!("{kept}\n... [command truncated]")
}

/// The newest output a snapshot or a notice carries, and whether older
/// output was cut.
fn output_tail(output: &str) -> (String, bool) {
    let lines: Vec<&str> = output.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let chars = tail.chars().count();
    (
        tail.chars()
            .skip(chars.saturating_sub(TAIL_CHARS))
            .collect(),
        lines.len() > TAIL_LINES || chars > TAIL_CHARS,
    )
}

/// A job as the model holds it.
fn handle(job: &Job) -> Value {
    let state = lock(&job.state);
    json!({
        "job": job.id,
        "pid": job.pid,
        "command": job.command,
        "running": !state.reaped,
        "queued": state.queued,
        "persistent": job.lasting.is_some(),
    })
}

/// A finished command's result as the model reads it.
fn result(job: Option<&Job>, finished: &Finished, timed_out: bool, tests: Option<&Value>) -> Value {
    json!({
        "running": false,
        "exit-code": finished.exit_code,
        "output": finished.output,
        "truncated": finished.truncated,
        "timed-out": timed_out,
        "duration-ms": finished.duration.as_millis() as u64,
        "cached": job.is_none(),
        "job": job.map(|job| job.id.as_str()),
        "pid": job.map_or(0, |job| job.pid),
        "handle": finished.handle,
        "tests": tests,
    })
}

/// A job's result once it has one.
///
/// # Errors
///
/// A persistent shell ended and its files could not be read.
fn settled(job: &Job) -> anyhow::Result<Option<Value>> {
    let state = lock(&job.state);
    if let Some(failure) = &state.failure {
        bail!("persistent shell {}: {failure}", job.id);
    }
    Ok(state
        .finished
        .as_ref()
        .map(|finished| result(Some(job), finished, state.timed_out, state.tests.as_ref())))
}

impl Session {
    /// The job a call names.
    fn job(&self, fields: &Map<String, Value>) -> anyhow::Result<Arc<Job>> {
        let id = fields
            .get("job")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the job must be a job id"))?;
        lock(&self.table).jobs.get(id).cloned().ok_or_else(|| {
            anyhow!(
                "unknown job {id} (the kernel keeps its running jobs and the last 64 that ended)"
            )
        })
    }

    /// Start `job` now if it is waiting for a slot.
    fn start_now(&self, job: &Arc<Job>) {
        if lock(&self.table).promote(job) {
            job.open_gate();
        }
    }

    /// A cell read `job`'s result: no notice is due for it, and one the host
    /// already took is withdrawn before this cell ends.
    fn consume(&self, job: &Job) {
        let withdraw = {
            let mut state = lock(&job.state);
            if state.finished.is_none() {
                return;
            }
            std::mem::replace(&mut state.notice, Notice::Waived) == Notice::Sent
        };
        if let (true, Some(handler)) = (withdraw, self.host.handlers.get("bash.consumed")) {
            // The reply never matters: a withdrawal the host refuses leaves a
            // notice for a result the model has by now read.
            let _ = self.runtime.block_on(tokio::time::timeout(
                HANDLE_WAIT,
                handler(HostRequestPayload {
                    data: json!({ "pid": job.pid, "command": capped(&job.command) }),
                }),
            ));
        }
    }

    /// The window of a wait that asked for `requested`.
    fn window(&self, requested: &Window) -> Option<Duration> {
        let wired = self
            .host
            .handlers
            .get(BASH_COMPLETED_REQUEST_TYPE)
            .is_some();
        let window = match requested {
            Window::For(window) => *window,
            // Nothing would wake a session that ended its turn on a yielded
            // command, so only a session with notices yields on its own.
            Window::Session if wired => self.host.settings.wait.unwrap_or(DEFAULT_WINDOW),
            Window::Session | Window::Unbounded => return None,
        }
        .min(MAX_WINDOW);
        Some(match lock(&self.cells).deadline {
            Some(deadline) => window.min(
                deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(CELL_MARGIN),
            ),
            None => window,
        })
    }

    /// The kernel wrote a file itself: no cached result outlives it.
    pub(super) fn wrote(&self) {
        lock(&self.cache).invalidate();
    }
}

/// The commands of `environment`'s session, answered from `handlers`: their
/// state, and the kernel's hold on it. The session's persistent shells are
/// re-attached. Must be called from within the tokio runtime `runtime` names.
pub(super) fn open(
    runtime: tokio::runtime::Handle,
    handlers: HostRequestHandlers,
    environment: &SessionEnvironment,
) -> (Arc<Session>, BashJobs) {
    // The session's own environment first, then the process's, as the Python
    // kernel's process saw them.
    let set = |name: &str| {
        environment
            .env
            .iter()
            .rev()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.clone())
            .or_else(|| pa_types::session_scope::var(name).ok())
            .filter(|value| !value.is_empty())
    };
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let session = Arc::new(Session {
        runtime,
        cwd: environment.cwd.clone(),
        host: BashHost {
            handlers,
            spill: environment
                .artifact_dir
                .as_deref()
                .map(SpillStore::for_session_dir),
            settings: BashSettings {
                // `PRIME_AGENT_BASH_SHELL` (`builtin` or `system`) overrides
                // the session's setting, for one session or one test run.
                engine: match set("PRIME_AGENT_BASH_SHELL").as_deref() {
                    Some("builtin") => crate::platform::KernelShell::BuiltIn,
                    Some("system") => crate::platform::KernelShell::System,
                    Some(_) | None => environment.shell,
                },
                shell: environment
                    .shell_path
                    .as_deref()
                    .map(|shell| shell.to_string_lossy().into_owned()),
                command_prefix: environment.command_prefix.clone(),
                // The Python kernel's ceiling: two commands a core.
                max_jobs: set("PRIME_AGENT_BASH_MAX_JOBS")
                    .and_then(|limit| limit.parse::<usize>().ok())
                    .map_or((cores * 2).clamp(4, 32), |limit| limit.clamp(1, 256)),
                wait: set("PRIME_AGENT_BASH_WAIT_MS")
                    .and_then(|window| window.parse().ok())
                    .map(Duration::from_millis),
                // Only the session's own: one found in the process's
                // environment was inherited from another session.
                session_id: environment
                    .env
                    .iter()
                    .rev()
                    .find(|(variable, value)| variable == "RLM_SESSION_ID" && !value.is_empty())
                    .map(|(_, value)| value.clone()),
                agent_dir: environment.agent_dir.clone(),
                env: environment.env.clone(),
            },
        },
        table: Mutex::new(Table::default()),
        cells: Mutex::new(Cells::default()),
        cell_ended: watch::Sender::new(0),
        cache: Arc::new(Mutex::new(cache::CommandCache::default())),
        background: environment.background.clone(),
        killed: AbortSignal::new(),
        running_list: environment
            .artifact_dir
            .as_ref()
            .map(|dir| dir.join(job::RUNNING_FILE)),
    });
    // What the session's last kernel left listed as running was killed with
    // it (or with the process that hosted it): the session hears of each
    // once, as it hears of a command that ended.
    if let Some(list) = &session.running_list {
        let mut lost: Vec<(String, Value)> = Vec::new();
        for record in std::fs::read_to_string(list)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            let Some(id) = record.get("job").and_then(Value::as_str) else {
                continue;
            };
            lost.retain(|(listed, _)| listed != id);
            if record.get("running") == Some(&Value::Bool(true)) {
                lost.push((id.to_string(), record));
            }
        }
        // Taken before anything is told, so a notice is never repeated.
        let _ = std::fs::remove_file(list);
        if let Some(handler) = session.host.handlers.get(BASH_COMPLETED_REQUEST_TYPE) {
            let handler = handler.clone();
            session.runtime.spawn(async move {
                for (_, record) in lost {
                    // A refused notice is lost with the command it names.
                    let _ = handler(HostRequestPayload {
                        data: json!({
                            "pid": record.get("pid"),
                            "command": record.get("command"),
                            "lost": true,
                        }),
                    })
                    .await;
                }
            });
        }
    }
    for job in persistent::rehydrate(&session) {
        let live = session.background.enter();
        session
            .runtime
            .spawn(announce(Arc::clone(&session), job, live));
    }
    let jobs = BashJobs(Arc::clone(&session));
    (session, jobs)
}

/// Tell the session about a job that ended after its cell, unread: the
/// `[bash-done]` notice. The job is live background work (`_live`) until
/// this returns.
async fn announce(session: Arc<Session>, job: Arc<Job>, _live: LiveWork) {
    tokio::select! {
        () = job.until(|job| {
            let state = lock(&job.state);
            state.finished.is_some() && state.reaped
        }) => {}
        // A persistent shell runs on without its kernel, which hears no more of it.
        () = session.killed.cancelled() => return,
    }
    // The cell may do other work before it waits for this job: it is not
    // left behind until that whole cell has ended.
    let mut ended = session.cell_ended.subscribe();
    while *ended.borrow_and_update() < job.cell {
        if ended.changed().await.is_err() {
            return;
        }
    }
    let finished = {
        let state = lock(&job.state);
        if state.notice != Notice::Due {
            return;
        }
        state.finished.clone()
    };
    // A killed kernel's session hears nothing more of its jobs.
    let (Some(finished), Some(handler), false) = (
        finished,
        session.host.handlers.get(BASH_COMPLETED_REQUEST_TYPE),
        lock(&session.table).closed,
    ) else {
        return;
    };
    let (tail, cut) = output_tail(&finished.output);
    let command = capped(&job.command);
    let accepted = handler(HostRequestPayload {
        data: json!({
            "pid": job.pid,
            "command": command,
            "exitCode": finished.exit_code,
            "output": tail,
            "outputTruncated": cut,
        }),
    })
    .await;
    if accepted.is_err() {
        // A refused notice loses nothing: the result stays on the job for
        // `bash-poll`.
        return;
    }
    // Marked sent only now, so a withdrawal can never overtake its notice;
    // a result read while the notice was on its way is withdrawn here.
    let withdraw = {
        let mut state = lock(&job.state);
        let read_meanwhile = state.notice == Notice::Waived;
        if !read_meanwhile {
            state.notice = Notice::Sent;
        }
        read_meanwhile
    };
    if let (true, Some(handler)) = (withdraw, session.host.handlers.get("bash.consumed")) {
        // As in `Session::consume`: the reply never matters.
        let _ = handler(HostRequestPayload {
            data: json!({ "pid": job.pid, "command": command }),
        })
        .await;
    }
}

/// Wait for `job` from its cell: until it finishes, `window` passes (the
/// job runs on and the call returns a snapshot), or `limit` passes (the job
/// is killed). A cell stopped meanwhile kills a job it owns.
///
/// # Errors
///
/// The cell was stopped, the job would not stop, or a persistent shell's
/// result could not be read.
fn wait(
    session: &Arc<Session>,
    job: &Arc<Job>,
    window: Option<Duration>,
    limit: Option<Duration>,
    ownership: Ownership,
) -> anyhow::Result<Value> {
    // Outside a cell nothing stops the wait but its own ends.
    let cell = cell::current().map_or_else(AbortSignal::new, |cell| cell.signal());
    let after = |delay: Option<Duration>| async move {
        match delay {
            Some(delay) => tokio::time::sleep(delay).await,
            None => std::future::pending().await,
        }
    };
    let end = session.runtime.block_on(async {
        tokio::select! {
            biased;
            () = cell.cancelled() => End::CellStopped,
            () = job.until(|job| lock(&job.state).finished.is_some()) => End::Finished,
            () = after(limit) => End::TimedOut,
            () = after(window) => End::Yielded,
        }
    });
    match (end, ownership) {
        (End::Yielded, _) => {
            show(CellRecord::BashWait { completed: false });
            let waited = window.unwrap_or_default().as_secs_f64();
            let wired = session
                .host
                .handlers
                .get(BASH_COMPLETED_REQUEST_TYPE)
                .is_some();
            let (output, cut) = output_tail(&job.text()?);
            return Ok(json!({
                "running": true,
                "exit-code": null,
                "output": output,
                "output-truncated": cut,
                "duration-ms": job.started.elapsed().as_millis() as u64,
                "job": job.id,
                "pid": job.pid,
                "queued": lock(&job.state).queued,
                "command": job.command,
                "note": format!(
                    "still running after {waited}s: this is a snapshot of its newest output. \
                     Wait longer with (bash-wait job seconds){}",
                    if wired {
                        ", or end the turn: a [bash-done] notification arrives when it finishes."
                    } else {
                        "."
                    }
                ),
            }));
        }
        (End::CellStopped, Ownership::Shared) => bail!(
            "the wait ended with its cell; job {} is still running",
            job.id
        ),
        (End::CellStopped | End::TimedOut, Ownership::Owned)
        | (End::TimedOut, Ownership::Shared) => {
            // SIGTERM, a short grace, SIGKILL to the whole group, then a
            // bounded wait for the group's end: nothing the command does can
            // land after this call returns.
            job.kill(&session.runtime, Signal::Term, TERM_GRACE);
            let stopped = session.runtime.block_on(tokio::time::timeout(
                TERM_GRACE + KILL_WAIT,
                job.until(|job| lock(&job.state).reaped),
            ));
            job.update(|state| {
                state.notice = Notice::Waived;
                state.timed_out = end == End::TimedOut;
            });
            if end == End::CellStopped {
                bail!("the command was killed: its cell was stopped");
            }
            if stopped.is_err() {
                bail!("the command did not stop at its timeout (pid {})", job.pid);
            }
        }
        (End::Finished, _) => show(CellRecord::BashWait { completed: true }),
    }
    let finished = lock(&job.state)
        .finished
        .clone()
        .ok_or_else(|| anyhow!("the command ended without a result"))?;
    session.consume(job);
    if finished.exit_code != 0 {
        show(CellRecord::CommandFailure(KernelCommandFailure {
            command: capped(&job.command),
            exit_code: i64::from(finished.exit_code),
            output: finished.output.clone(),
        }));
    }
    let parsed = (lock(&job.state).tests.is_none() && summary::looks_like_test_run(&job.command))
        .then(|| summary::summarize(&finished.output))
        .flatten();
    let mut announced = None;
    if let Some(parsed) = parsed {
        // The host stores the whole output and each failure's section (and
        // counts the run). One that does not answer in time, or a session
        // without a store, leaves the summary without handles.
        let reply = session
            .host
            .handlers
            .get("tests.summary")
            .and_then(|handler| {
                let stored = handler(HostRequestPayload {
                    data: json!({
                        "runner": parsed.runner,
                        "failed": parsed.failed,
                        "output": finished.output,
                        "failures": parsed
                            .failures
                            .iter()
                            .map(|failure| failure.output.as_str())
                            .collect::<Vec<_>>(),
                    }),
                });
                session
                    .runtime
                    .block_on(tokio::time::timeout(HANDLE_WAIT, stored))
                    .ok()?
                    .ok()
            })
            .unwrap_or(Value::Null);
        let failure_handles: Vec<Option<String>> = reply
            .get("failures")
            .and_then(Value::as_array)
            .filter(|handles| handles.len() == parsed.failures.len())
            .map(|handles| {
                handles
                    .iter()
                    .map(|handle| handle.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let tests = parsed.value(
            reply.get("output").and_then(Value::as_str),
            &failure_handles,
        );
        announced = tests.get("text").cloned();
        job.update(|state| state.tests = Some(tests));
    }
    let mut value = settled(job)?.ok_or_else(|| anyhow!("the command ended without a result"))?;
    if let Some(text) = announced {
        value["announce"] = text;
    }
    Ok(value)
}

/// Run one of the `bash` operations for the cell that called it.
///
/// # Errors
///
/// Bad options, an unknown job, a command that could not be started, or a
/// wait that ended with its cell.
pub(super) fn call(
    session: &Arc<Session>,
    operation: &str,
    options: &Value,
) -> anyhow::Result<Value> {
    let fields = options
        .as_object()
        .ok_or_else(|| anyhow!("options must be a hash"))?;
    let accepted: &[&str] = match operation {
        "bash" => &["timeout-ms", "wait-ms", "background", "persistent"],
        "bash-wait" => &["seconds"],
        "bash-tail" => &["lines"],
        "bash-kill" => &["signal", "grace-ms"],
        "bash-poll" | "bash-output" | "bash-running" | "bash-jobs" | "bash-persistent-shells" => {
            &[]
        }
        unknown => bail!("unknown native tool: {unknown}"),
    };
    if let Some(unknown) = fields
        .keys()
        .find(|key| !matches!(key.as_str(), "command" | "job") && !accepted.contains(&key.as_str()))
    {
        return Err(crate::kernel::scheme::contracts::undeclared(
            operation,
            crate::kernel::scheme::contracts::Spelling::HashKey,
            unknown,
            accepted,
        )
        .into());
    }
    match operation {
        "bash" => {
            let script = fields
                .get("command")
                .and_then(Value::as_str)
                .filter(|script| !script.trim().is_empty())
                .ok_or_else(|| anyhow!("the command must be a non-empty string"))?;
            let flag = |name: &str| match fields.get(name) {
                None => Ok(false),
                Some(Value::Bool(set)) => Ok(*set),
                Some(_) => Err(anyhow!("{name} must be #t or #f")),
            };
            let (background, persistent) = (flag("background")?, flag("persistent")?);
            let limit = match fields.get("timeout-ms") {
                None | Some(Value::Bool(false)) => None,
                Some(value) => Some(Duration::from_millis(
                    value.as_u64().filter(|limit| *limit > 0).ok_or_else(|| {
                        anyhow!("timeout-ms must be a positive integer, or #f for none")
                    })?,
                )),
            };
            if limit.is_some() && (background || persistent) {
                bail!("timeout-ms only applies to a command its call waits for and owns");
            }
            let requested = match fields.get("wait-ms") {
                Some(_) if limit.is_some() => bail!(
                    "timeout-ms and wait-ms cannot be combined: a command with a timeout is waited for until it ends or is killed"
                ),
                // A command with a timeout is never handed to the background:
                // nothing would kill it there.
                None if limit.is_some() => Window::Unbounded,
                None => Window::Session,
                Some(Value::Bool(false)) => Window::Unbounded,
                Some(value) => {
                    Window::For(Duration::from_millis(value.as_u64().ok_or_else(|| {
                        anyhow!("wait-ms must be a non-negative integer, or #f for no window")
                    })?))
                }
            };
            if cell::current().is_some_and(|cell| cell.signal().is_cancelled()) {
                bail!("the cell was stopped before the command started");
            }
            // An allowlisted read-only command whose inputs are unchanged
            // answers from the cache; a waited one is remembered when its
            // inputs held while it ran.
            let cacheable = (!background && !persistent)
                .then(|| cache::plan(script))
                .flatten()
                .and_then(|plan| {
                    cache::fingerprint(&session.cwd, &plan).map(|stamps| (plan, stamps))
                });
            let mut on_finished: Option<FinishHook> = None;
            if let Some((plan, stamps)) = cacheable {
                let hit = lock(&session.cache).lookup(script, &stamps);
                if let Some(handler) = session.host.handlers.get("command_cache.fact") {
                    let counted = handler(HostRequestPayload {
                        data: json!({ "source": "bash", "hit": hit.is_some() }),
                    });
                    // Adoption telemetry: nothing waits for it, and a count
                    // the host refuses changes nothing here.
                    session.runtime.spawn(async move {
                        let _ = counted.await;
                    });
                }
                if let Some(hit) = hit {
                    show(CellRecord::BashCommand {
                        command: script.to_string(),
                    });
                    return Ok(result(
                        /*job*/ None, &hit, /*timed_out*/ false, /*tests*/ None,
                    ));
                }
                let (kept, cwd, command) = (
                    Arc::clone(&session.cache),
                    session.cwd.clone(),
                    script.to_string(),
                );
                let epoch = lock(&session.cache).epoch;
                on_finished = Some(Box::new(move |finished: &Finished| {
                    if cache::fingerprint(&cwd, &plan).as_ref() == Some(&stamps) {
                        lock(&kept).remember(
                            &command,
                            stamps,
                            epoch,
                            Finished {
                                duration: Duration::ZERO,
                                ..finished.clone()
                            },
                        );
                    }
                }));
            }
            let job = if persistent {
                persistent::spawn(session, script)?
            } else {
                job::spawn(session, script, on_finished)?
            };
            show(CellRecord::BashCommand {
                command: script.to_string(),
            });
            // Counted before the call returns: the cell can end right after it.
            let live = session.background.enter();
            session
                .runtime
                .spawn(announce(Arc::clone(session), Arc::clone(&job), live));
            if background {
                return Ok(handle(&job));
            }
            // A command its call waits for never queues behind background jobs.
            session.start_now(&job);
            let ownership = if persistent {
                Ownership::Shared
            } else {
                Ownership::Owned
            };
            wait(session, &job, session.window(&requested), limit, ownership)
        }
        "bash-wait" => {
            let job = session.job(fields)?;
            let requested = match fields.get("seconds") {
                None | Some(Value::Bool(false)) => Window::Session,
                Some(value) => Window::For(
                    value
                        .as_f64()
                        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                        .ok_or_else(|| anyhow!("the seconds must be a non-negative number"))?,
                ),
            };
            if lock(&session.cells).current == job.cell {
                session.start_now(&job);
            }
            wait(
                session,
                &job,
                session.window(&requested),
                /*limit*/ None,
                Ownership::Shared,
            )
        }
        "bash-poll" => {
            let job = session.job(fields)?;
            session.consume(&job);
            Ok(settled(&job)?.unwrap_or(Value::Null))
        }
        "bash-tail" | "bash-output" => {
            let job = session.job(fields)?;
            let text = job.text()?;
            session.consume(&job);
            let Some(lines) = fields.get("lines") else {
                return Ok(Value::String(text));
            };
            let wanted = lines
                .as_u64()
                .and_then(|lines| usize::try_from(lines).ok())
                .ok_or_else(|| anyhow!("the lines must be a non-negative integer"))?;
            let all: Vec<&str> = text.lines().collect();
            Ok(Value::String(
                all[all.len().saturating_sub(wanted)..].join("\n"),
            ))
        }
        "bash-kill" => {
            let job = session.job(fields)?;
            let signal = match fields.get("signal").and_then(Value::as_str) {
                None | Some("term") => Signal::Term,
                Some("kill") => Signal::Kill,
                Some(other) => bail!("unknown signal '{other}' (accepted: term, kill)"),
            };
            let grace = match fields.get("grace-ms") {
                None => Duration::from_secs(5),
                Some(value) => Duration::from_millis(
                    value
                        .as_u64()
                        .ok_or_else(|| anyhow!("grace-ms must be a non-negative integer"))?,
                ),
            };
            let running = !lock(&job.state).reaped;
            job.kill(&session.runtime, signal, grace);
            Ok(Value::Bool(running))
        }
        "bash-running" => Ok(Value::Bool(!lock(&session.job(fields)?.state).reaped)),
        "bash-jobs" | "bash-persistent-shells" => {
            let table = lock(&session.table);
            let jobs = table.order.iter().filter_map(|id| table.jobs.get(id));
            if operation == "bash-persistent-shells" {
                return Ok(jobs
                    .filter(|job| job.lasting.is_some())
                    .map(|job| handle(job))
                    .collect());
            }
            Ok(jobs
                .map(|job| {
                    let state = lock(&job.state);
                    let duration = state
                        .finished
                        .as_ref()
                        .map_or_else(|| job.started.elapsed(), |finished| finished.duration);
                    json!({
                        "job": job.id,
                        "command": job.command.chars().take(512).collect::<String>(),
                        "pid": job.pid,
                        "started-at": job.started_at,
                        "duration-ms": duration.as_millis() as u64,
                        "status": if state.reaped {
                            "finished"
                        } else if state.queued {
                            "queued"
                        } else {
                            "running"
                        },
                        "exit-code": state.finished.as_ref().map(|finished| finished.exit_code),
                        "persistent": job.lasting.is_some(),
                    })
                })
                .collect())
        }
        unknown => bail!("unknown native tool: {unknown}"),
    }
}
