//! Prime Agent patch (see `PRIME_AGENT_PATCH.md`): what a process that embeds
//! several shells gives each of them.

/// The embedding process's hold on one shell and its subshells.
///
/// A shell with a host never starts a process itself and can be stopped from
/// outside: every external command goes through [`Host::spawn`], and the
/// interpreter asks [`Host::interrupted`] before each pipeline.
/// Implementations are shared by the shell's subshells and background jobs,
/// which run on other threads.
pub trait Host: Send + Sync + 'static {
    /// Start `command`, which the shell has fully composed (program,
    /// arguments, working directory, environment and standard streams).
    ///
    /// # Errors
    ///
    /// The process could not be started; `std::io::ErrorKind::NotFound`
    /// reports a missing program.
    fn spawn(&self, command: std::process::Command) -> std::io::Result<tokio::process::Child>;

    /// Whether the shell was told to stop. Once true it must stay true: the
    /// interpreter fails every pipeline it is asked to start from then on.
    fn interrupted(&self) -> bool;

    /// What the shell gives as its own process id (`$$`, `BASHPID`) and
    /// as its parent's (`PPID`). An embedded shell has no process of its
    /// own, and the id of the process it lives in must not reach a script:
    /// `kill $$`, even run by a program, would end every shell in it.
    fn process_id(&self) -> u32;

    /// The implementation, for builtins that share state with it.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Run `future` beside its caller, on a blocking thread of the runtime.
///
/// Upstream starts these (a pipeline stage, a command or process
/// substitution, a background list) with `tokio::spawn`. The interpreter
/// reads and writes pipes with blocking calls, and a task spawned from a
/// worker thread waits in that worker's own queue: when the worker then
/// blocks reading what the task is to write, neither ever runs
/// (`x=$(cat <(echo a))`). A blocking thread per such task cannot be
/// starved that way.
pub(crate) fn spawn_beside<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::task::spawn_blocking(move || tokio::runtime::Handle::current().block_on(future))
}
