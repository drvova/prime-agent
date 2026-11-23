//! One shell command as a job of its session's kernel: the process, its
//! captured output, and the tasks that watch it to its end.
//!
//! The command runs under the kernel's shell in its own session (so its own
//! process group), with the non-interactive environment the Python kernel's
//! `bash()` used. On Unix the shell's stdin is one end of a socket pair that
//! serves twice. It is the start gate: the shell reads a line from it before
//! it runs anything, written only once the process is in the orphan journal
//! and has a job slot, so a kernel that dies in between leaves a shell that
//! exits on the closed socket. And it is the status channel: after the
//! command the shell prints a one-time fence into its output and its exit
//! status on the socket, then waits for whatever the command left in the
//! background. The result is the output up to the fence with that status, so
//! `sleep 60 & echo started` answers at once, while the job stays running
//! until its whole group has ended.
//!
//! A shell that ends without a status (`exit`, `exec`, a fatal signal) is
//! settled by its exit: output is read until the pipe closes or goes quiet,
//! and whatever is left of the group is killed.
//!
//! A job's group is killed however its session ends. The task that watches a
//! job holds its group ([`GroupHold`]): dropped before the group is known to
//! be gone (its runtime shut down under it, as when a hosted session crashes
//! or is abandoned), it kills the whole group, where the shell's own
//! `kill_on_drop` would end the shell alone and leave what it started.
//!
//! A saved session also keeps a list of its running commands beside its
//! other artifacts ([`RUNNING_FILE`]): a command still listed when the
//! session's next kernel starts was killed with the kernel before it, and
//! the session is told so once.
//!
//! On Windows there is no status channel: the shell is held at a gate on its
//! stdin until it is inside a kill-on-close job object, its result is its
//! exit, and ending the job ends every process in it.
//!
//! A session whose commands run on the built-in shell has no shell process:
//! [`spawn`] hands the command to [`super::engine`], which runs it inside
//! this process and starts only the programs it names. Such a job shares
//! everything here but the process: its capture, its slot, its result.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use tokio::sync::watch;

use super::persistent::Lasting;
use super::Session;
use crate::kernel::orphan_journal::record_orphan_process_state;
use crate::kernel::spill::{SpillCopy, HANDLE_PREFIX};
use crate::platform::{self, Signal};

/// Bytes of the output's start that are always kept.
pub(super) const HEAD_CAP: usize = 512 * 1024;
/// Bytes of the output's end that are kept; what falls between is dropped.
pub(super) const TAIL_CAP: usize = 3 * 512 * 1024;
/// How long output is still read after a shell ends without the fence: a
/// process holding the inherited pipe must not hold the result open.
const EXIT_PIPE_GRACE: Duration = Duration::from_millis(500);
/// Finished jobs the kernel still answers for, oldest dropped first.
const HISTORY_CAP: usize = 64;
/// Output longer than this is also kept in the session's spill store, so the
/// result names a handle the `ctx-*` readers page.
const SPILL_OVER_CHARS: usize = 8_192;

/// The environment every command gets over the session's own and its
/// settings': nothing may open an editor, a pager, or a prompt, since no one
/// can answer it.
const CHILD_ENV: [(&str, &str); 16] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("CLICOLOR", "0"),
    ("FORCE_COLOR", "0"),
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// What a command gets unless the session's environment or its settings
/// name the variable: no progress bars redrawn into a capture nobody
/// watches.
const QUIET_ENV: [(&str, &str); 2] = [("PIP_PROGRESS_BAR", "off"), ("TQDM_DISABLE", "1")];

/// The character type a command gets when nothing names a locale
/// (`LC_ALL`, `LC_CTYPE` and `LANG` all unset or empty), as Python's own
/// C-locale coercion picks it: UTF-8 text without changing the messages,
/// the collation or the number formats of the C locale. macOS has no
/// `C.UTF-8`; its UTF-8 character type is named `UTF-8`.
#[cfg(target_os = "macos")]
const UTF8_CTYPE: &str = "UTF-8";
#[cfg(not(target_os = "macos"))]
const UTF8_CTYPE: &str = "C.UTF-8";

/// The variables a command gets over the session's environment, later over
/// earlier: [`QUIET_ENV`] and a UTF-8 `LC_CTYPE` where neither the session
/// nor `settings` set their own, then `settings`, then [`CHILD_ENV`].
pub(super) fn child_env(settings: &[(String, String)]) -> Vec<(String, String)> {
    let value = |name: &str| {
        settings
            .iter()
            .rev()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.clone())
            .or_else(|| pa_types::session_scope::var(name).ok())
    };
    let defaults = QUIET_ENV
        .into_iter()
        .filter(|(name, _)| value(name).is_none())
        .chain(
            ["LC_ALL", "LC_CTYPE", "LANG"]
                .into_iter()
                .all(|name| value(name).is_none_or(|locale| locale.is_empty()))
                .then_some(("LC_CTYPE", UTF8_CTYPE)),
        );
    defaults
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .chain(settings.iter().cloned())
        .chain(
            CHILD_ENV
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        )
        .collect()
}

/// What opens a held shell's start gate: its end of the status socket on
/// Unix, the writing end of its stdin pipe elsewhere.
#[cfg(unix)]
type Gate = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
type Gate = std::io::PipeWriter;

/// Runs once with a job's result, before anyone waiting on it sees it.
pub(super) type FinishHook = Box<dyn FnOnce(&Finished) + Send>;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// End process group `pid` for good: true when it is gone or the kill was
/// delivered, false when it could not be, so the group stays in the orphan
/// journal for the host to reap. Where there are no process groups nothing
/// can be delivered.
pub(super) fn end_group(pid: u32) -> bool {
    match platform::process::process_group_exists(pid as i32) {
        Ok(false) => true,
        Ok(true) | Err(_) => platform::process::signal_process_group(pid as i32, Signal::Kill),
    }
}

/// The file, in a saved session's artifact directory, listing the commands
/// its kernel has running: one JSON line when a command starts (`job`,
/// `pid`, `command`, `running` true) and one when its group has ended
/// (`job`, `running` false). The file is removed whenever nothing runs.
pub(super) const RUNNING_FILE: &str = "bash-running.jsonl";

/// Add `record` to the session's list of running commands. Best-effort, as
/// the orphan journal is: a command never fails because it could not be
/// listed, and a command that is not listed is only not announced as lost.
fn list_running(list: &std::path::Path, record: &serde_json::Value) {
    // What is left of an ended session lifetime writes nothing.
    if pa_types::session_scope::ensure_live().is_err() {
        return;
    }
    // A session that has stored nothing yet has no artifact directory.
    if let Some(dir) = list.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    platform::perms::set_private_mode(&mut options);
    if let Ok(mut file) = options.open(list) {
        let _ = file.write_all(format!("{record}\n").as_bytes());
    }
}

/// Held by the task that watches a job, until the job's group is known to be
/// gone. Dropped before then, it kills the whole group.
pub(super) struct GroupHold(pub(super) Arc<Job>);

impl Drop for GroupHold {
    fn drop(&mut self) {
        if !lock(&self.0.state).reaped && self.0.destroy() {
            record_orphan_process_state(self.0.pid as i32, false);
        }
    }
}

/// A command's captured output: its first [`HEAD_CAP`] bytes and a rolling
/// [`TAIL_CAP`]-byte tail, with the middle dropped and counted.
#[derive(Default)]
struct BoundedBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl BoundedBuffer {
    fn write(&mut self, chunk: &[u8]) {
        let take = (HEAD_CAP - self.head.len()).min(chunk.len());
        self.head.extend_from_slice(&chunk[..take]);
        self.tail.extend(&chunk[take..]);
        let excess = self.tail.len().saturating_sub(TAIL_CAP);
        self.tail.drain(..excess);
        self.dropped += excess;
    }

    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            // Decoded as one run so a character split at the head's end survives.
            return String::from_utf8_lossy(&[self.head.as_slice(), &tail].concat()).into_owned();
        }
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&tail);
        format!("{head}\n... [{} bytes dropped] ...\n{tail}", self.dropped)
    }

    /// Bytes held (what was dropped does not count).
    fn size(&self) -> usize {
        self.head.len() + self.tail.len()
    }
}

/// What has been read from a command's output pipe.
#[derive(Default)]
pub(super) struct Capture {
    buffer: BoundedBuffer,
    /// Bytes held back because they may be the start of the fence.
    pending: Vec<u8>,
    /// The output as it stood when the fence arrived: the command's result.
    pub(super) fenced: Option<String>,
    /// The fence arrived, or the output will never carry one.
    pub(super) settled: bool,
    /// The pipe closed: every process that held it has ended.
    eof: bool,
    /// When the command last wrote anything, for its `[bash-quiet]` notice.
    pub(super) last_output: Option<Instant>,
    /// The output up to its settling, for the session's spill store (only
    /// when the session has one), and the bytes of a character split across
    /// two reads.
    copy: Option<SpillCopy>,
    carry: Vec<u8>,
}

impl Capture {
    /// Keep `bytes` as output of the command itself.
    fn keep(&mut self, bytes: &[u8]) {
        self.buffer.write(bytes);
        let Some(copy) = self.copy.as_mut() else {
            return;
        };
        self.carry.extend_from_slice(bytes);
        let whole = match std::str::from_utf8(&self.carry) {
            // Cut off mid-character: the rest arrives with the next read.
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Ok(_) | Err(_) => self.carry.len(),
        };
        let text = String::from_utf8_lossy(&self.carry[..whole]).into_owned();
        copy.push(&text, text.chars().count());
        self.carry.drain(..whole);
    }

    /// Take one read of the pipe, watching for `fence`. True when this read
    /// settled the output.
    fn consume(&mut self, fence: &[u8], chunk: &[u8]) -> bool {
        self.last_output = Some(Instant::now());
        if self.settled {
            // Written after the fence (a background job, an EXIT trap): kept
            // for `bash-output`, outside the result.
            self.buffer.write(chunk);
            return false;
        }
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        if let Some(at) = data.windows(fence.len()).position(|window| window == fence) {
            self.keep(&data[..at]);
            self.fenced = Some(self.buffer.text());
            self.settled = true;
            self.buffer.write(&data[at + fence.len()..]);
            return true;
        }
        let held = (1..fence.len().min(data.len() + 1))
            .rev()
            .find(|size| data.ends_with(&fence[..*size]))
            .unwrap_or(0);
        let (ready, rest) = data.split_at(data.len() - held);
        self.keep(ready);
        self.pending = rest.to_vec();
        false
    }

    /// No fence will come: what was held back is plain output.
    pub(super) fn settle(&mut self) {
        if !self.settled {
            let pending = std::mem::take(&mut self.pending);
            self.keep(&pending);
            self.settled = true;
        }
    }
}

/// A finished command's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Finished {
    pub(super) exit_code: i32,
    pub(super) output: String,
    /// The output outgrew the buffer and lost its middle.
    pub(super) truncated: bool,
    pub(super) duration: Duration,
    /// The spill handle of the output (`out:<id>`), when it was large and the
    /// session keeps a store.
    pub(super) handle: Option<String>,
}

/// Where a job stands with its `[bash-done]` notice.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Notice {
    /// Its end is to be announced, unless a cell reads its result first.
    #[default]
    Due,
    /// A cell waited it to its end, killed it, or read its result: nothing
    /// is to be announced, and what was has been withdrawn.
    Waived,
    /// The host took the notice; a later read withdraws it.
    Sent,
}

/// Where a job stands with its `[bash-quiet]` notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Quiet {
    /// Named in this many so far.
    Named(u32),
    /// Someone asked for it to be killed: it is named in no more.
    KillRequested,
}

impl Default for Quiet {
    fn default() -> Self {
        Self::Named(0)
    }
}

/// What changes about a job while it lives.
#[derive(Default)]
pub(super) struct State {
    /// Held at its start gate until a job slot frees.
    pub(super) queued: bool,
    pub(super) finished: Option<Finished>,
    /// The shell and everything left of its group have ended.
    pub(super) reaped: bool,
    pub(super) notice: Notice,
    /// It was killed by its call's `timeout-ms`.
    pub(super) timed_out: bool,
    /// Its test-run summary, once a wait parsed one.
    pub(super) tests: Option<serde_json::Value>,
    /// Why a persistent shell's result could not be read.
    pub(super) failure: Option<String>,
    pub(super) quiet: Quiet,
}

/// One command the kernel started.
pub(super) struct Job {
    /// The kernel's own name for it: pids are reused, this is not.
    pub(super) id: String,
    pub(super) command: String,
    pub(super) pid: u32,
    pub(super) started: Instant,
    pub(super) started_at: String,
    /// The cell that started it (the kernel's count of cells).
    pub(super) cell: u64,
    /// A persistent shell's files and identity; `None` for every other job.
    pub(super) lasting: Option<Lasting>,
    pub(super) state: Mutex<State>,
    pub(super) capture: Mutex<Capture>,
    /// The output reader is moving a chunk from the pipe into the buffer.
    transferring: AtomicBool,
    pub(super) changed: watch::Sender<()>,
    gate: Mutex<Option<Gate>>,
    /// What starts a held command of the built-in shell.
    pub(super) start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The programs a command of the built-in shell has started; `None` for
    /// a command that is a shell process of its own.
    pub(super) engine: Option<Arc<super::engine::Programs>>,
    pub(super) on_finished: Mutex<Option<FinishHook>>,
    /// A second handle on the output pipe, to ask how much is unread.
    #[cfg(unix)]
    probe: Mutex<Option<std::io::PipeReader>>,
    #[cfg(windows)]
    container: Mutex<Option<super::winjob::JobObject>>,
}

impl Job {
    /// A job record for process `pid`, not yet watched by anything.
    pub(super) fn new(
        session: &Session,
        command: &str,
        pid: u32,
        id: String,
        lasting: Option<Lasting>,
    ) -> Self {
        Job {
            id,
            command: command.to_string(),
            pid,
            started: Instant::now(),
            started_at: crate::session::manager::format_iso_now(),
            cell: lock(&session.cells).current,
            lasting,
            state: Mutex::new(State::default()),
            capture: Mutex::new(Capture {
                copy: session.host.spill.as_ref().map(|_| SpillCopy::default()),
                ..Capture::default()
            }),
            transferring: AtomicBool::new(false),
            changed: watch::Sender::new(()),
            gate: Mutex::new(None),
            start: Mutex::new(None),
            engine: None,
            on_finished: Mutex::new(None),
            #[cfg(unix)]
            probe: Mutex::new(None),
            #[cfg(windows)]
            container: Mutex::new(None),
        }
    }

    /// Resolve once `ready` holds, re-checked on every change to the job.
    pub(super) async fn until(&self, ready: impl Fn(&Job) -> bool) {
        let mut changes = self.changed.subscribe();
        while !ready(self) {
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// Record a change `update` makes to the job's state and wake its waiters.
    pub(super) fn update(&self, update: impl FnOnce(&mut State)) {
        update(&mut lock(&self.state));
        self.changed.send_replace(());
    }

    /// Everything the command has written so far, the part after its result
    /// included.
    ///
    /// # Errors
    ///
    /// A persistent shell's output file could not be read.
    pub(super) fn text(&self) -> anyhow::Result<String> {
        match &self.lasting {
            Some(lasting) => lasting.text(&self.id),
            None => Ok(lock(&self.capture).buffer.text()),
        }
    }

    /// Let a held shell run its command.
    pub(super) fn open_gate(&self) {
        lock(&self.state).queued = false;
        if let Some(mut gate) = lock(&self.gate).take() {
            // A failed write means the shell already died; its exit reports that.
            let _ = gate.write_all(b"\n");
        }
        if let Some(start) = lock(&self.start).take() {
            start();
        }
    }

    /// Kill what is left of the job's processes at once. True when they are
    /// gone or the kill was delivered, and the job's entry in the orphan
    /// journal can go. A command of the built-in shell answers false: its
    /// programs keep their own entries, and take them out themselves.
    pub(super) fn destroy(&self) -> bool {
        if let Some(programs) = &self.engine {
            programs.end();
            return false;
        }
        #[cfg(unix)]
        return end_group(self.pid);
        #[cfg(windows)]
        return {
            let mut held = lock(&self.container);
            match held.take() {
                // Dropped here: closing the ended job's handle.
                Some(container) if container.terminate() => true,
                Some(container) => {
                    let killed = platform::kill_process_group_or_pid(self.pid as i32);
                    if !killed {
                        // Kept: closing its handle is the last way to end the job.
                        *held = Some(container);
                    }
                    killed
                }
                // An earlier call ended the job through its job object.
                None => true,
            }
        };
        #[cfg(not(any(unix, windows)))]
        return false;
    }

    /// Signal the job's whole process group. `Signal::Term` is followed by a
    /// kill of whatever is still there after `grace`. A job that has already
    /// ended, or a persistent shell whose pid no longer provably names it, is
    /// left alone.
    pub(super) fn kill(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        signal: Signal,
        grace: Duration,
    ) {
        let identified = self
            .lasting
            .as_ref()
            .is_none_or(|lasting| lasting.identity(self.pid) == Some(true));
        if lock(&self.state).reaped || !identified {
            return;
        }
        lock(&self.state).quiet = Quiet::KillRequested;
        if let Some(programs) = &self.engine {
            // The script stops at its next command either way; its programs
            // get the signal, and what a `Term` leaves is killed after `grace`.
            programs.stop(signal);
            if signal == Signal::Term {
                let job = Arc::clone(self);
                runtime.spawn(async move {
                    let ended = job.until(|job| lock(&job.state).reaped);
                    if tokio::time::timeout(grace, ended).await.is_err() {
                        job.destroy();
                    }
                });
            }
            return;
        }
        #[cfg(unix)]
        {
            // Undelivered means the group is already gone.
            let _ = platform::process::signal_process_group(self.pid as i32, signal);
            if signal == Signal::Term {
                let job = Arc::clone(self);
                runtime.spawn(async move {
                    let ended = job.until(|job| lock(&job.state).reaped);
                    if tokio::time::timeout(grace, ended).await.is_err() {
                        match &job.lasting {
                            Some(lasting) => {
                                lasting.reap_group(job.pid, /*leader_exit_witnessed*/ false);
                            }
                            None => {
                                job.destroy();
                            }
                        }
                    }
                });
            }
        }
        #[cfg(not(unix))]
        {
            // No signals: every stop ends the job's whole process tree.
            let _ = (runtime, signal, grace);
            self.destroy();
        }
    }
}

/// The session's jobs: every one it still answers for, and which of them
/// hold a slot or wait for one.
#[derive(Default)]
pub(super) struct Table {
    pub(super) jobs: HashMap<String, Arc<Job>>,
    /// Ids in the order the jobs started.
    pub(super) order: Vec<String>,
    /// Ids of ended jobs, oldest first.
    finished: VecDeque<String>,
    /// Jobs holding a slot.
    running: usize,
    /// Jobs held at their gate, first in line first.
    queue: VecDeque<Arc<Job>>,
    /// The kernel was killed: nothing more may start.
    pub(super) closed: bool,
}

impl Table {
    /// Start `job` now if it is waiting for a slot, over the limit.
    pub(super) fn promote(&mut self, job: &Arc<Job>) -> bool {
        let Some(at) = self.queue.iter().position(|held| Arc::ptr_eq(held, job)) else {
            return false;
        };
        self.queue.remove(at);
        self.running += 1;
        true
    }

    /// `job` ended: file it under the finished jobs, dropping the oldest
    /// over [`HISTORY_CAP`].
    pub(super) fn retire(&mut self, job: &Job) {
        self.finished.push_back(job.id.clone());
        while self.finished.len() > HISTORY_CAP {
            if let Some(oldest) = self.finished.pop_front() {
                self.jobs.remove(&oldest);
                self.order.retain(|id| *id != oldest);
            }
        }
    }
}

/// Read `output` into `job`'s capture, watching for `fence`, on a thread
/// of its own: reading blocks, and a process the command leaves behind can
/// hold the pipe open long after its result, so that process never blocks on
/// a full pipe.
///
/// # Errors
///
/// The thread could not be started.
pub(super) fn read_output(
    job: &Arc<Job>,
    mut output: std::io::PipeReader,
    fence: Vec<u8>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let reading = Arc::clone(job);
    std::thread::Builder::new()
        .name("scheme-bash-output".to_string())
        .spawn(pa_types::session_scope::propagate(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match output.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        reading.transferring.store(true, Ordering::SeqCst);
                        let settled = lock(&reading.capture).consume(&fence, &chunk[..read]);
                        reading.transferring.store(false, Ordering::SeqCst);
                        if settled {
                            reading.changed.send_replace(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let mut capture = lock(&reading.capture);
            capture.settle();
            capture.eof = true;
            drop(capture);
            reading.changed.send_replace(());
        }))
}

/// Put `job` in the session's table and its list of running commands, with
/// a slot or, when `may_queue` and the session is at its limit, in line for
/// one. Returns whether it is held, or `None` when the kernel was killed and
/// nothing more may start.
pub(super) fn enlist(session: &Session, job: &Arc<Job>, may_queue: bool) -> Option<bool> {
    let queued = {
        let mut table = lock(&session.table);
        if table.closed {
            return None;
        }
        table.jobs.insert(job.id.clone(), Arc::clone(job));
        table.order.push(job.id.clone());
        // Under the table's lock, so the list's lines are in the jobs' order.
        if let Some(list) = &session.running_list {
            list_running(
                list,
                &serde_json::json!({
                    "job": job.id,
                    "pid": job.pid,
                    "command": super::capped(&job.command),
                    "running": true,
                }),
            );
        }
        let queued = may_queue && table.running >= session.host.settings.max_jobs;
        if queued {
            table.queue.push_back(Arc::clone(job));
        } else {
            table.running += 1;
        }
        queued
    };
    lock(&job.state).queued = queued;
    Some(queued)
}

/// Start `command` as a job of `session`. It runs at once when a job slot
/// is free; over the session's limit it is held at its gate (`queued`) until
/// one frees or a caller starts it. `on_finished` runs with its result.
///
/// # Errors
///
/// No usable shell or working directory, a failed spawn, Windows job
/// containment that could not be established, or a kernel already killed.
/// Nothing the command would have done has happened in any of these.
pub(super) fn spawn(
    session: &Arc<Session>,
    command: &str,
    on_finished: Option<FinishHook>,
) -> anyhow::Result<Arc<Job>> {
    if !session.cwd.is_dir() {
        bail!(
            "the working directory does not exist: {}",
            session.cwd.display()
        );
    }
    let settings = &session.host.settings;
    match settings.engine {
        platform::KernelShell::BuiltIn => {
            return super::engine::spawn(session, command, on_finished);
        }
        platform::KernelShell::System => {}
    }
    let shell =
        platform::resolve_kernel_bash_shell(settings.shell.as_deref())?.ok_or_else(|| {
            anyhow!(
                "no shell program was found (on Windows, install Git Bash in its default location, or drop \"shell\": \"system\" from settings to use the built-in shell)"
            )
        })?;
    let command_line = match &settings.command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };
    // Split in two in the script, so the command's own text (a traced or
    // echoed script) never contains the fence.
    let (first, second) = (
        uuid::Uuid::new_v4().simple().to_string(),
        uuid::Uuid::new_v4().simple().to_string(),
    );
    let fence = format!("\x1eprime-agent-complete:{first}{second}\x1f").into_bytes();
    // One pipe carries stdout and stderr, so their order is the command's own.
    let (output, output_end) = std::io::pipe().context("could not open the output pipe")?;
    let mut process = pa_types::session_scope::tokio_command(&shell);
    process
        .arg("-c")
        .current_dir(&session.cwd)
        .envs(child_env(&settings.env))
        .stdout(
            output_end
                .try_clone()
                .context("could not open the output pipe")?,
        )
        .stderr(output_end)
        .kill_on_drop(true);
    #[cfg(unix)]
    let (gate, status) = {
        // `printf` by its path on the system's default PATH: a function or an
        // alias the command defined cannot swallow the fence.
        let emit = ["/usr/bin/printf", "/bin/printf"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
            .map_or_else(
                || "\\command -p printf".to_string(),
                |path| format!("'{path}'"),
            );
        // The control descriptors are closed for the command itself, so a
        // background process it leaves holds neither.
        process.arg(format!(
            "exec 9>&0 8>&1 0</dev/null\n\
             read -r _prime_agent_gate <&9 || exit 127\n\
             {{\n{command_line}\n}} 8>&- 9>&-\n\
             __prime_status=$?\n\
             \\set +x\n\
             {emit} '\\036prime-agent-complete:%s%s\\037' '{first}' '{second}' >&8 || exit \"$__prime_status\"\n\
             {emit} '%s\\n' \"$__prime_status\" >&9\n\
             exec 8>&- 9>&-\n\
             wait\n\
             exit \"$__prime_status\"\n"
        ));
        let (ours, theirs) =
            std::os::unix::net::UnixStream::pair().context("could not open the status channel")?;
        process.stdin(std::os::fd::OwnedFd::from(theirs));
        platform::process::set_new_session(process.as_std_mut());
        let gate = ours
            .try_clone()
            .context("could not open the status channel")?;
        (gate, ours)
    };
    #[cfg(not(unix))]
    let gate = {
        process.arg(format!(
            "read -r _prime_agent_gate || exit 127\nexec 0</dev/null\n{command_line}\n"
        ));
        let (gate_end, gate) = std::io::pipe().context("could not open the start gate")?;
        process.stdin(gate_end);
        platform::set_new_process_group(process.as_std_mut());
        platform::set_no_window(process.as_std_mut());
        gate
    };
    let context = session.runtime.enter();
    let mut child = process
        .spawn()
        .with_context(|| format!("could not start {shell}"))?;
    drop(context);
    // `process` holds this process's copies of the pipe's writing end: the
    // output can only end once they are closed.
    drop(process);
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("the shell ended before it had a pid"))?;
    // The shell is still held at its gate: stopping it here undoes the start.
    let abandon = |child: &mut tokio::process::Child, reason: anyhow::Error| {
        // A shell that already died needs no kill.
        let _ = child.start_kill();
        record_orphan_process_state(pid as i32, false);
        reason
    };
    record_orphan_process_state(pid as i32, true);
    let job = Job::new(
        session,
        command,
        pid,
        uuid::Uuid::new_v4().simple().to_string(),
        /*lasting*/ None,
    );
    *lock(&job.gate) = Some(gate);
    *lock(&job.on_finished) = on_finished;
    #[cfg(unix)]
    {
        *lock(&job.probe) = output.try_clone().ok();
    }
    #[cfg(windows)]
    {
        // Fail closed: a shell outside its job could leak its children.
        let entered = super::winjob::JobObject::create().filter(|object| {
            child
                .raw_handle()
                .is_some_and(|process| object.assign(process))
        });
        let Some(container) = entered else {
            return Err(abandon(
                &mut child,
                anyhow!("Windows job containment could not be established"),
            ));
        };
        *lock(&job.container) = Some(container);
    }
    let job = Arc::new(job);
    let reader = read_output(&job, output, fence);
    if let Err(error) = reader {
        return Err(abandon(
            &mut child,
            anyhow!("could not start the output reader: {error}"),
        ));
    }
    // Windows starts every command at once: its gate only covers the
    // moment before the shell is inside its job object.
    let Some(queued) = enlist(session, &job, /*may_queue*/ cfg!(unix)) else {
        return Err(abandon(&mut child, anyhow!("the kernel was shut down")));
    };
    // The hold exists before the task is first polled: a runtime shut down
    // before then still kills the group.
    session.runtime.spawn(supervise(
        Arc::clone(session),
        GroupHold(Arc::clone(&job)),
        child,
        #[cfg(unix)]
        status,
    ));
    if !queued {
        job.open_gate();
    }
    Ok(job)
}

/// Watch one job to its end: settle its result, wait for its shell, kill
/// what is left of its group, and pass its slot on.
async fn supervise(
    session: Arc<Session>,
    hold: GroupHold,
    mut child: tokio::process::Child,
    #[cfg(unix)] status: std::os::unix::net::UnixStream,
) {
    let job = Arc::clone(&hold.0);
    let mut exited = None;
    // The status the shell reported after its command, when it got that far.
    #[cfg(unix)]
    let delivered: Option<i32> = match status.set_nonblocking(true).and_then(|()| {
        let direct = status.try_clone()?;
        Ok((direct, tokio::net::UnixStream::from_std(status)?))
    }) {
        // Unreadable: the shell's exit settles the job instead.
        Err(_) => None,
        Ok((mut direct, stream)) => {
            let mut line = Vec::new();
            loop {
                let mut chunk = [0u8; 64];
                // Status bytes first: the shell writes them before it exits,
                // so they are already in the socket when its exit is seen.
                // The exit can be seen before the runtime has heard that the
                // socket is readable, and `try_read` answers from what the
                // runtime has heard: there the socket itself is read. A
                // status missed here would settle the output without its
                // fence, which then shows in the result.
                let read = tokio::select! {
                    biased;
                    ready = stream.readable() => match ready {
                        Ok(()) => match stream.try_read(&mut chunk) {
                            Ok(read) => read,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                            Err(_) => 0,
                        },
                        Err(_) => 0,
                    },
                    status = child.wait() => {
                        exited = Some(status);
                        direct.read(&mut chunk).unwrap_or(0)
                    }
                };
                line.extend_from_slice(&chunk[..read]);
                if read == 0 || line.contains(&b'\n') || exited.is_some() {
                    break;
                }
            }
            line.split(|byte| *byte == b'\n')
                .next()
                .filter(|_| line.contains(&b'\n'))
                .and_then(|text| std::str::from_utf8(text).ok())
                .and_then(|text| text.trim().parse().ok())
        }
    };
    #[cfg(not(unix))]
    let delivered: Option<i32> = None;
    let (exit_code, fenced) = if let Some(code) = delivered {
        // The fence precedes the status in the shell's script.
        job.until(|job| lock(&job.capture).settled).await;
        (code, lock(&job.capture).fenced.clone())
    } else {
        let status = match exited.take() {
            Some(status) => status,
            None => child.wait().await,
        };
        let code = status
            .as_ref()
            .ok()
            .and_then(platform::normalized_exit_code)
            .unwrap_or(-1);
        exited = Some(status);
        lock(&job.capture).settle();
        (code, None)
    };
    if fenced.is_none() {
        // Read on until the pipe closes or nothing more is arriving.
        let deadline = Instant::now() + EXIT_PIPE_GRACE;
        let mut size = lock(&job.capture).buffer.size();
        while Instant::now() < deadline {
            let closed = job.until(|job| lock(&job.capture).eof);
            if tokio::time::timeout(Duration::from_millis(50), closed)
                .await
                .is_ok()
            {
                break;
            }
            #[cfg(unix)]
            let unread = lock(&job.probe).as_ref().is_some_and(|pipe| {
                use std::os::fd::AsRawFd;
                let mut waiting: libc::c_int = 0;
                // SAFETY: FIONREAD writes one int through a valid pointer, for
                // a descriptor this job keeps open.
                let asked =
                    unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD, &raw mut waiting) };
                asked == 0 && waiting > 0
            });
            #[cfg(not(unix))]
            let unread = false;
            let current = lock(&job.capture).buffer.size();
            if !unread && !job.transferring.load(Ordering::SeqCst) && current == size {
                break;
            }
            size = current;
        }
    }
    settle(&session, &job, exit_code, fenced);
    if exited.is_none() {
        // The shell is waiting for what the command left in the background.
        // Only its end matters now: the result is already settled.
        let _ = child.wait().await;
    }
    let gone = job.destroy();
    drop(lock(&job.gate).take());
    #[cfg(unix)]
    drop(lock(&job.probe).take());
    retire(&session, &job);
    if gone {
        record_orphan_process_state(job.pid as i32, false);
    }
}

/// Give `job` its result: the command's own output (`fenced`, or everything
/// captured when it ended without the fence) with `exit_code`.
pub(super) fn settle(session: &Session, job: &Job, exit_code: i32, fenced: Option<String>) {
    let finished = {
        let mut capture = lock(&job.capture);
        let truncated = capture.buffer.dropped > 0;
        let handle = capture
            .copy
            .take()
            .filter(|copy| truncated || copy.total_chars() > SPILL_OVER_CHARS)
            .zip(session.host.spill.as_ref())
            .map(|(copy, store)| format!("{HANDLE_PREFIX}{}", store.spill(copy).id));
        Finished {
            exit_code,
            output: fenced.unwrap_or_else(|| capture.buffer.text()),
            truncated,
            duration: job.started.elapsed(),
            handle,
        }
    };
    if let Some(hook) = lock(&job.on_finished).take() {
        hook(&finished);
    }
    job.update(|state| state.finished = Some(finished));
}

/// Everything of `job` has ended: mark it so, pass its slot on, and take it
/// off the session's list of running commands.
pub(super) fn retire(session: &Session, job: &Arc<Job>) {
    job.update(|state| state.reaped = true);
    let started = {
        let mut table = lock(&session.table);
        let mut started = Vec::new();
        // A job that ended while still held never had a slot: taking it out
        // of the line counts one, which its end gives back.
        table.promote(job);
        table.running = table.running.saturating_sub(1);
        while table.running < session.host.settings.max_jobs {
            let Some(next) = table.queue.pop_front() else {
                break;
            };
            table.running += 1;
            started.push(next);
        }
        table.retire(job);
        // A killed kernel's commands stay listed: its session's next kernel
        // tells the session they were lost.
        if let (false, Some(list)) = (table.closed, &session.running_list) {
            if table.running == 0 && table.queue.is_empty() {
                // Nothing is left to list (a missing file is the same).
                let _ = std::fs::remove_file(list);
            } else {
                list_running(
                    list,
                    &serde_json::json!({ "job": job.id, "running": false }),
                );
            }
        }
        started
    };
    for next in started {
        next.open_gate();
    }
}
