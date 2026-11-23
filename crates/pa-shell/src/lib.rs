// too_many_lines: style gate only; a utility parses its options and does its
// work in one function. Casts: 64-bit targets; narrowing sits at bounded
// OS boundaries. struct_excessive_bools: a utility's options are flags.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools
)]
//! Prime Agent's own shell: a bash-compatible interpreter and a set of
//! command-line utilities that run inside the process that embeds them.
//!
//! One [`start`] is one script on one shell. The shell's working directory,
//! variables, functions and options belong to that shell alone: nothing here
//! reads or changes the process's working directory or environment, so any
//! number of scripts of different sessions run side by side in one process.
//!
//! What a script names is, in order: a shell function, a shell builtin, one
//! of the utilities in [`UTILITIES`] (run on a thread of the shell, against
//! the shell's working directory and streams), or a program on the shell's
//! `PATH`, which the embedder starts through [`Children`]. A utility asked
//! for something it does not implement hands the command to the program of
//! the same name when the machine has one, and fails with a message that
//! says so when it has none.

mod engine;
mod shims;
#[cfg(test)]
mod tests;
mod util;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use engine::start;
pub use util::UTILITIES;

/// Stack of the thread a script runs on and of each of the shell's worker
/// threads: the interpreter recurses with the script's nesting.
const STACK_BYTES: usize = 64 * 1024 * 1024;

/// What starts the programs a script runs, and owns them from then on.
///
/// The shell composes each command completely (program, arguments, working
/// directory, a cleared and refilled environment, standard streams) and
/// hands it over; it never starts a process itself. An implementation
/// decides how the process is contained (its process group or job), and
/// ends it when the script is stopped: the shell only waits for the child
/// it is given. It is called from the shell's threads, several at once for
/// the stages of a pipeline.
pub trait Children: Send + Sync + 'static {
    /// Start `command`.
    ///
    /// # Errors
    ///
    /// The process could not be started; `std::io::ErrorKind::NotFound`
    /// reports a missing program.
    fn spawn(&self, command: std::process::Command) -> std::io::Result<tokio::process::Child>;
}

/// The request to stop a script, shared between the embedder and the shell.
/// Once requested it stays requested.
#[derive(Clone, Default)]
pub struct Stop(Arc<StopState>);

#[derive(Default)]
struct StopState {
    requested: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Stop {
    /// Tell the script to stop: the interpreter starts no further command,
    /// a utility ends at its next line or file, and the run reports exit
    /// code 130. Ending the programs the script started is up to the
    /// [`Children`] that started them.
    pub fn request(&self) {
        self.0.requested.store(true, Ordering::SeqCst);
        self.0.wake.notify_waiters();
    }

    /// Whether [`Stop::request`] was called.
    #[must_use]
    pub fn requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// Resolve once the stop is requested.
    pub async fn wait(&self) {
        let woken = self.0.wake.notified();
        tokio::pin!(woken);
        // Registered before the flag is read, so a request in between is seen.
        woken.as_mut().enable();
        if !self.requested() {
            woken.await;
        }
    }
}

/// One script and what it runs in.
pub struct Script {
    pub source: String,
    /// What the script sees as the shell's process id (`$$`). The shell has
    /// no process of its own, and the id of the process it runs in is kept
    /// from scripts: pass a number that names no process.
    pub id: u32,
    /// The shell's working directory at its start.
    pub cwd: PathBuf,
    /// The shell's whole environment: nothing is taken from the process's.
    pub env: Vec<(String, String)>,
    /// Where the script's standard output and standard error go, together.
    /// The run holds it until everything the script started in the shell has
    /// ended.
    pub output: std::io::PipeWriter,
}

/// How many commands of a script ran where.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Utilities run inside the process.
    pub in_process: u64,
    /// Programs started as processes.
    pub spawned: u64,
    /// Utilities that handed their command to the program of the same name,
    /// because they do not implement what was asked (counted in `spawned`
    /// too).
    pub handed_over: u64,
    /// Utilities that could do neither: no such program on this machine.
    pub unavailable: u64,
}

/// A script's own end: its exit code, as a shell reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub exit_code: i32,
    pub counts: Counts,
}

/// A started script.
pub struct Running {
    ended: tokio::sync::oneshot::Receiver<Ended>,
    gone: tokio::sync::oneshot::Receiver<()>,
}

/// The exit code of a script that was stopped (a shell's 128 + SIGINT).
pub const STOPPED_EXIT_CODE: i32 = 130;
/// The exit code of a script whose shell failed on an internal error.
pub const INTERNAL_ERROR_EXIT_CODE: i32 = 70;

impl Running {
    /// The script's result, once the script itself has ended: what it left
    /// running in the background (`command &`) may still run.
    pub async fn ended(&mut self) -> Ended {
        // A sender dropped unsent is a shell that could not report at all.
        (&mut self.ended).await.unwrap_or(Ended {
            exit_code: INTERNAL_ERROR_EXIT_CODE,
            counts: Counts::default(),
        })
    }

    /// Resolve once the script's background jobs have ended too, or it was
    /// stopped, and the shell has let go of its output.
    pub async fn gone(&mut self) {
        // Sent or dropped, the thread is at its end either way.
        let _ = (&mut self.gone).await;
    }
}
