//! A command on the built-in shell (`pa-shell`): the script runs inside this
//! process, on a thread of its own, and only the programs it names are
//! processes.
//!
//! Such a job has no shell process, so what stands in for one differs:
//!
//! - Its `pid` is a job number in a range no process id reaches. Notices
//!   and the list of running commands name the job by it.
//! - Each program the script starts is its own session and process group
//!   (on Windows all of them share one kill-on-close job object).
//!   [`Programs`] holds them: stopping the job signals every group, and so
//!   does the job's end, whatever ended it.
//! - A program is listed in the orphan journal as it starts, so the next
//!   host reaps one that a dying process leaves behind, and taken out within
//!   [`JOURNAL_SWEEP`] of its end. Its records are not synced to disk, as a
//!   shell process's are: a script may start thousands, and a record only
//!   has to outlive this process, not the machine.
//! - The script's result is its shell's exit code, reported when the script
//!   ends; this side then writes the fence into the output itself, so the
//!   result is the output up to there, as for a shell process. The job stays
//!   running while anything the script left in the background runs.
//! - A stopped script ends at its next command, and a utility at its next
//!   line. One that does neither within [`ABANDON_AFTER`] (a builtin stuck
//!   in a read no one will feed) is abandoned: its job ends, its thread is
//!   left to end with its pipe, and whatever it does afterwards it does
//!   after `stop` was requested, so it starts nothing.

use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::json;

use super::job::{self, lock, FinishHook, GroupHold, Job, CHILD_ENV};
use super::Session;
use crate::kernel::orphan_journal::{note_orphan_process_state, OrphanState};
use crate::kernel::shared::HostRequestPayload;
use crate::platform::{self, Signal};

/// Where the job numbers of the built-in shell's commands start: above
/// every process id, below the ids of hosted sessions.
const NUMBER_BASE: u32 = 0x2000_0000;
/// How often the orphan journal is told which programs have ended.
const JOURNAL_SWEEP: Duration = Duration::from_millis(250);
/// How long a stopped script has to end before its thread is abandoned.
const ABANDON_AFTER: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Running {
    /// The pid of each program the script has running, which is also its
    /// process group. Each is in the orphan journal.
    programs: Vec<u32>,
    /// The job has ended: nothing more may start.
    ended: bool,
}

/// The programs of one command of the built-in shell.
pub(super) struct Programs {
    stop: pa_shell::Stop,
    /// The exit code of a script that was stopped: that of a shell ended
    /// by the first signal the job was sent.
    stopped_with: std::sync::OnceLock<i32>,
    running: Mutex<Running>,
    /// The session the job belongs to: the shell's threads carry no scope,
    /// and the orphan journal is the session's.
    scope: Option<Arc<pa_types::session_scope::SessionScope>>,
    #[cfg(windows)]
    container: super::winjob::JobObject,
}

impl pa_shell::Children for Programs {
    fn spawn(&self, mut command: std::process::Command) -> std::io::Result<tokio::process::Child> {
        #[cfg(unix)]
        platform::process::set_new_session(&mut command);
        #[cfg(not(unix))]
        platform::set_new_process_group(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let Some(pid) = child.id() else {
            return Ok(child);
        };
        // Fail closed: a program outside the job's containment could outlive it.
        #[cfg(windows)]
        if !child
            .raw_handle()
            .is_some_and(|process| self.container.assign(process))
        {
            // A program that already ended needs no kill.
            let _ = child.start_kill();
            return Err(std::io::Error::other(
                "Windows job containment could not be established",
            ));
        }
        let mut running = lock(&self.running);
        if running.ended {
            drop(running);
            let _ = child.start_kill();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the command was stopped",
            ));
        }
        running.programs.push(pid);
        drop(running);
        let _scope = pa_types::session_scope::enter(self.scope.clone());
        note_orphan_process_state(pid as i32, OrphanState::Active);
        Ok(child)
    }
}

impl Programs {
    /// Whether anything of program `pid` still runs.
    fn alive(pid: u32) -> bool {
        #[cfg(unix)]
        return !matches!(
            platform::process::process_group_exists(pid as i32),
            Ok(false)
        );
        #[cfg(not(unix))]
        return pa_types::platform::process::is_process_alive(pid).unwrap_or(true);
    }

    /// Bring the orphan journal up to date: take out (and forget) the
    /// programs that have ended.
    fn journal(&self) {
        let _scope = pa_types::session_scope::enter(self.scope.clone());
        let mut running = lock(&self.running);
        running.programs.retain(|pid| {
            let alive = Self::alive(*pid);
            if !alive {
                note_orphan_process_state(*pid as i32, OrphanState::Ended);
            }
            alive
        });
    }

    /// Tell the script to stop and signal every program it has running.
    pub(super) fn stop(&self, signal: Signal) {
        self.stopped_with.get_or_init(|| match signal {
            Signal::Term => 128 + 15,
            Signal::Kill => 128 + 9,
        });
        self.stop.request();
        #[cfg(unix)]
        for pid in &lock(&self.running).programs {
            // Undelivered means the group is already gone.
            let _ = platform::process::signal_process_group(*pid as i32, signal);
        }
        #[cfg(windows)]
        {
            // No signals: every stop ends the job's whole process tree.
            let _ = signal;
            self.container.terminate();
        }
    }

    /// End the job for good: the script stops, every program is killed, and
    /// nothing more may start. Safe to call again.
    pub(super) fn end(&self) {
        self.stop(Signal::Kill);
        let _scope = pa_types::session_scope::enter(self.scope.clone());
        let mut running = lock(&self.running);
        running.ended = true;
        // What could not be confirmed gone stays in the journal for the
        // host to reap.
        running.programs.retain(|pid| {
            let gone = !Self::alive(*pid)
                || cfg!(windows)
                || platform::process::signal_process_group(*pid as i32, Signal::Kill);
            if gone {
                note_orphan_process_state(*pid as i32, OrphanState::Ended);
            }
            !gone
        });
    }
}

/// Start `command` as a job of `session` on the built-in shell, as
/// [`job::spawn`] starts one on a shell process.
///
/// # Errors
///
/// No working directory, an output pipe or a thread that could not be
/// opened, Windows job containment that could not be established, or a
/// kernel already killed. Nothing the command would have done has happened
/// in any of these.
pub(super) fn spawn(
    session: &Arc<Session>,
    command: &str,
    on_finished: Option<FinishHook>,
) -> anyhow::Result<Arc<Job>> {
    static NEXT_NUMBER: AtomicU32 = AtomicU32::new(0);

    if !session.cwd.is_dir() {
        bail!(
            "the working directory does not exist: {}",
            session.cwd.display()
        );
    }
    let settings = &session.host.settings;
    let source = match &settings.command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };
    // The session's environment as a shell process of it would inherit it,
    // later names over earlier ones.
    let mut env: Vec<(String, String)> = pa_types::session_scope::vars();
    for (name, value) in settings
        .env
        .iter()
        .cloned()
        .chain(
            CHILD_ENV
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
        )
        // How a script, or a program it starts, can tell which shell runs it.
        .chain([("PRIME_AGENT_SHELL".to_string(), "builtin".to_string())])
    {
        env.retain(|(existing, _)| *existing != name);
        env.push((name, value));
    }
    let fence = format!(
        "\x1eprime-agent-complete:{}{}\x1f",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
    .into_bytes();
    let (output, output_end) = std::io::pipe().context("could not open the output pipe")?;
    let programs = Arc::new(Programs {
        stop: pa_shell::Stop::default(),
        stopped_with: std::sync::OnceLock::new(),
        running: Mutex::new(Running::default()),
        scope: pa_types::session_scope::current(),
        #[cfg(windows)]
        container: super::winjob::JobObject::create()
            .ok_or_else(|| anyhow::anyhow!("Windows job containment could not be established"))?,
    });
    let mut job = Job::new(
        session,
        command,
        NUMBER_BASE + NEXT_NUMBER.fetch_add(1, Ordering::Relaxed) % NUMBER_BASE,
        uuid::Uuid::new_v4().simple().to_string(),
        /*lasting*/ None,
    );
    job.engine = Some(Arc::clone(&programs));
    *lock(&job.on_finished) = on_finished;
    // The shell starts when the job has a slot; the task that watches the
    // job is handed the running script then.
    let (started, starting) = tokio::sync::oneshot::channel();
    let script = pa_shell::Script {
        source,
        id: job.pid,
        cwd: session.cwd.clone(),
        env,
        output: output_end
            .try_clone()
            .context("could not open the output pipe")?,
    };
    let (stop, children) = (programs.stop.clone(), Arc::clone(&programs));
    *lock(&job.start) = Some(Box::new(move || {
        // Unheard means the job ended while it was held.
        let _ = started.send(pa_shell::start(script, stop, children));
    }));
    let job = Arc::new(job);
    job::read_output(&job, output, fence.clone()).context("could not start the output reader")?;
    let Some(queued) = job::enlist(session, &job, /*may_queue*/ true) else {
        bail!("the kernel was shut down");
    };
    // The hold exists before the task is first polled: a runtime shut down
    // before then still ends the job.
    session.runtime.spawn(supervise(
        Arc::clone(session),
        GroupHold(Arc::clone(&job)),
        programs,
        starting,
        output_end,
        fence,
    ));
    if !queued {
        job.open_gate();
    }
    Ok(job)
}

/// Watch one command of the built-in shell to its end: settle its result,
/// wait for what it left in the background, end its programs, and pass its
/// slot on.
async fn supervise(
    session: Arc<Session>,
    hold: GroupHold,
    programs: Arc<Programs>,
    starting: tokio::sync::oneshot::Receiver<std::io::Result<pa_shell::Running>>,
    mut output_end: std::io::PipeWriter,
    fence: Vec<u8>,
) {
    let job = Arc::clone(&hold.0);
    // Resolves once the script is stopped and has had its time to end.
    let abandoned = async {
        programs.stop.wait().await;
        tokio::time::sleep(ABANDON_AFTER).await;
    };
    tokio::pin!(abandoned);
    let started = tokio::select! {
        started = starting => started.ok(),
        // Ended while it was held: nothing ran.
        () = &mut abandoned => None,
    };
    let mut running = match started {
        Some(Ok(running)) => Some(running),
        Some(Err(error)) => {
            // The shell's own messages go where the command's output goes;
            // a closed output has no reader left to tell.
            let _ = output_end.write_all(
                format!("bash: the built-in shell could not start: {error}\n").as_bytes(),
            );
            None
        }
        None => None,
    };
    let mut journal = tokio::time::interval(JOURNAL_SWEEP);
    let ended = match &mut running {
        Some(running) => loop {
            tokio::select! {
                ended = running.ended() => break Some(ended),
                () = &mut abandoned => break None,
                _ = journal.tick() => programs.journal(),
            }
        },
        None => None,
    };
    // The fence follows the script's last output in the pipe, so the result
    // is everything before it however far the reader has got.
    let fenced = output_end.write_all(&fence).is_ok();
    drop(output_end);
    if fenced {
        job.until(|job| lock(&job.capture).settled).await;
    } else {
        lock(&job.capture).settle();
    }
    let fenced_output = lock(&job.capture).fenced.clone();
    let exit_code = match (programs.stopped_with.get(), &ended, running.is_some()) {
        (Some(stopped), _, _) => *stopped,
        (None, Some(ended), _) => ended.exit_code,
        (None, None, true) => pa_shell::STOPPED_EXIT_CODE,
        (None, None, false) => 126,
    };
    job::settle(&session, &job, exit_code, fenced_output);
    if let (Some(ended), Some(handler)) = (&ended, session.host.handlers.get("shell.fact")) {
        let counted = handler(HostRequestPayload {
            data: json!({
                "inProcess": ended.counts.in_process,
                "spawned": ended.counts.spawned,
                "handedOver": ended.counts.handed_over,
                "unavailable": ended.counts.unavailable,
            }),
        });
        // Adoption telemetry: nothing waits for it, and a count the host
        // refuses changes nothing here.
        session.runtime.spawn(async move {
            let _ = counted.await;
        });
    }
    if let (Some(running), Some(_)) = (&mut running, &ended) {
        loop {
            tokio::select! {
                () = running.gone() => break,
                () = &mut abandoned => break,
                _ = journal.tick() => programs.journal(),
            }
        }
    }
    programs.end();
    // A command that never started still holds its end of the output.
    drop(lock(&job.start).take());
    job::retire(&session, &job);
}
