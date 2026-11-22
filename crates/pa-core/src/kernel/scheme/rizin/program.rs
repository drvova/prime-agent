//! The rizin program a session drives: how one is started, and what the
//! registry can do to a started one. The installed `rizin` is behind
//! [`Program`] so tests can put a scripted one in its place.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::platform;

/// Why a rizin could not be started.
#[derive(Debug)]
pub(super) enum LaunchError {
    /// No `rizin` on the session's `PATH`.
    NotInstalled,
    /// It is there and did not start.
    Failed(String),
}

/// A started rizin: its command pipe, its output, and its process.
pub(super) struct Running {
    /// Commands, one per line.
    pub(super) input: Box<dyn Write + Send>,
    /// Stdout and stderr together, in the order written.
    pub(super) output: Box<dyn Read + Send>,
    pub(super) process: Arc<dyn Process>,
}

/// Starts the rizin of one session. An implementation starts a new,
/// independent program on every call, with `arguments` as its command line,
/// and returns at once: it never waits for the program to be ready. It
/// answers [`LaunchError::NotInstalled`] only when there is no program to
/// start, so the caller can tell the user how to install it.
pub(super) trait Program: Send + Sync {
    /// Start a rizin with `arguments`.
    fn launch(&self, arguments: &[String]) -> Result<Running, LaunchError>;
}

/// A started rizin's process, as the session that owns it sees it. Every
/// method may be called from any thread, any number of times, and none but
/// [`Process::wait`] may block: `interrupt` and `kill` are called from a
/// stopping cell and from a session's teardown.
pub(super) trait Process: Send + Sync {
    /// The process id, for the model to see (`dp`, an attached debugger).
    fn pid(&self) -> u32;
    /// Ask the command that is running to stop, as Ctrl-C in a terminal
    /// would: rizin ends the command and stays usable. Where the platform
    /// cannot ask, this does nothing and the caller's grace ends in `kill`.
    fn interrupt(&self);
    /// End the program and everything it started (a debuggee, a shell
    /// command) for good. Its output then ends.
    fn kill(&self);
    /// Wait for the program to end and say how (`exit status: 0`,
    /// `signal: 9 (SIGKILL)`). Called once, after its output has ended.
    fn wait(&self) -> String;
}

/// The `rizin` on the session's `PATH`, run in the session's working
/// directory with the session's environment.
pub(super) struct Installed {
    pub(super) cwd: PathBuf,
    /// What the session adds to the process's own environment.
    pub(super) env: Vec<(String, String)>,
}

impl Program for Installed {
    fn launch(&self, arguments: &[String]) -> Result<Running, LaunchError> {
        let failed =
            |what: &str, error: std::io::Error| LaunchError::Failed(format!("{what}: {error}"));
        // One pipe carries stdout and stderr, so a command's errors arrive
        // before the reply that ends it.
        let (output, output_end) =
            std::io::pipe().map_err(|error| failed("could not open the output pipe", error))?;
        let mut command = pa_types::session_scope::std_command("rizin");
        command
            .args(arguments)
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .env("TERM", "dumb")
            .stdin(Stdio::piped())
            .stdout(
                output_end
                    .try_clone()
                    .map_err(|error| failed("could not open the output pipe", error))?,
            )
            .stderr(output_end);
        if self.cwd.is_dir() {
            command.current_dir(&self.cwd);
        }
        // Its own session: a signal to its group reaches the debuggee and
        // the shell commands it started, and nothing else.
        #[cfg(unix)]
        platform::process::set_new_session(&mut command);
        #[cfg(not(unix))]
        {
            platform::set_new_process_group(&mut command);
            platform::set_no_window(&mut command);
        }
        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LaunchError::NotInstalled,
            _ => failed("rizin could not be started", error),
        })?;
        let input = child.stdin.take().ok_or_else(|| {
            LaunchError::Failed("rizin started without its command pipe".to_string())
        })?;
        Ok(Running {
            input: Box::new(input),
            output: Box::new(output),
            process: Arc::new(Spawned {
                pid: child.id(),
                child: Mutex::new(Some(child)),
                reaped: AtomicBool::new(false),
            }),
        })
    }
}

/// A spawned rizin, leader of its own process group.
struct Spawned {
    pid: u32,
    /// The child until [`Process::wait`] takes it to reap it.
    child: Mutex<Option<Child>>,
    /// Set once the child is reaped: its pid then names nothing of ours.
    reaped: AtomicBool,
}

impl Process for Spawned {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn interrupt(&self) {
        // To the whole group: rizin waiting on a running debuggee returns
        // only when the debuggee itself is signalled.
        #[cfg(unix)]
        if !self.reaped.load(Ordering::SeqCst) {
            // Already gone is the only way this fails.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGINT,
            );
        }
    }

    fn kill(&self) {
        if !self.reaped.load(Ordering::SeqCst) {
            // Already gone is the only way this fails.
            let _ = platform::kill_process_group_or_pid(self.pid as i32);
        }
    }

    fn wait(&self) -> String {
        let child = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut child) = child else {
            return "already waited for".to_string();
        };
        // What rizin left in its group (a debuggee, a shell command) ends
        // with it: nothing of a session outlives its rizin. Before the
        // reaping, while its pid still names its group; a rizin that has
        // already exited keeps the status it exited with.
        self.kill();
        let ended = child.wait();
        self.reaped.store(true, Ordering::SeqCst);
        match ended {
            Ok(status) => status.to_string(),
            Err(error) => format!("its exit could not be read: {error}"),
        }
    }
}

impl Drop for Spawned {
    /// A rizin whose output nobody read to its end is still killed and
    /// reaped: its group first, then the child itself.
    fn drop(&mut self) {
        self.kill();
        if let Some(mut child) = self
            .child
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
