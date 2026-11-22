//! Per-session ambient state for processes that host more than one session.
//!
//! A session used to own a whole process, so its launch environment, working
//! directory and a handful of settings lived in process globals. A daemon
//! that hosts many sessions cannot keep them there: each hosted session
//! carries a [`SessionScope`] value instead, installed on every thread that
//! runs the session's code (its runtime's worker and blocking threads through
//! [`inherit`], its own OS threads through [`propagate`]).
//!
//! Code that used to read the process environment or working directory on a
//! session's behalf reads [`var`], [`var_os`], [`vars`] and [`current_dir`]
//! instead, and builds child processes with [`std_command`] /
//! [`tokio_command`]. On a thread with no scope (a CLI process, the daemon's
//! control plane) every accessor is exactly the `std::env` call it replaces.
//!
//! Limits, stated plainly: the scope follows threads, not tasks. A task a
//! session spawns onto a runtime that was built outside its scope, or a
//! thread started without [`propagate`], sees the process environment. A
//! third-party crate that reads `std::env` directly is not redirected.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// One hosted session's launch environment, working directory and
/// crate-owned per-session values.
pub struct SessionScope {
    id: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    removed_env: BTreeSet<String>,
    slots: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    /// Set once the session's host has ended this lifetime.
    ended: std::sync::atomic::AtomicBool,
    /// What to run when it ends.
    at_end: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for SessionScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The environment may carry credentials: only its size is printed.
        formatter
            .debug_struct("SessionScope")
            .field("id", &self.id)
            .field("cwd", &self.cwd)
            .field("env_len", &self.env.len())
            .finish_non_exhaustive()
    }
}

impl SessionScope {
    /// A scope whose environment is the process environment with `env` laid
    /// over it and `removed_env` hidden.
    #[must_use]
    pub fn new(
        id: String,
        cwd: PathBuf,
        env: BTreeMap<String, String>,
        removed_env: BTreeSet<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            cwd,
            env,
            removed_env,
            slots: Mutex::new(HashMap::new()),
            ended: std::sync::atomic::AtomicBool::new(false),
            at_end: Mutex::new(Vec::new()),
        })
    }

    /// End this lifetime of the session: its host calls this once it has
    /// stopped serving it. Threads of the session that are still running (a
    /// call that never returned, a thread the host could not wait for) keep
    /// this scope, and from now on [`ensure_live`] refuses their durable
    /// writes; whatever was registered with [`SessionScope::at_end`] runs.
    pub fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        let hooks =
            std::mem::take(&mut *self.at_end.lock().unwrap_or_else(PoisonError::into_inner));
        for hook in hooks {
            hook();
        }
    }

    /// Whether this lifetime has ended.
    #[must_use]
    pub fn has_ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Run `hook` when this lifetime ends ([`SessionScope::end`]): at once
    /// if it already has. For work of the session that lives on threads its
    /// host does not own and must stop with it.
    pub fn at_end(&self, hook: Box<dyn FnOnce() + Send>) {
        let mut hooks = self.at_end.lock().unwrap_or_else(PoisonError::into_inner);
        if self.has_ended() {
            drop(hooks);
            hook();
        } else {
            hooks.push(hook);
        }
    }

    /// The hosted session's stable id (the daemon's worker id).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session's working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The value of type `T` this session carries, when one was set.
    #[must_use]
    pub fn slot<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
    }

    /// Set this session's value of type `T`, replacing an earlier one.
    pub fn set_slot<T: Any + Send + Sync>(&self, value: Arc<T>) {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(TypeId::of::<T>(), value);
    }

    /// This session's value of type `T`, created with `init` on first use.
    pub fn slot_or_insert_with<T: Any + Send + Sync>(&self, init: impl FnOnce() -> T) -> Arc<T> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = slots
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
        {
            return existing;
        }
        let created = Arc::new(init());
        slots.insert(TypeId::of::<T>(), Arc::clone(&created) as _);
        created
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<SessionScope>>> = const { RefCell::new(None) };
}

/// The scope of the session this thread is running, when it runs one.
#[must_use]
pub fn current() -> Option<Arc<SessionScope>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Restores the thread's previous scope when dropped.
#[must_use = "the scope is left when the guard drops"]
pub struct ScopeGuard {
    previous: Option<Arc<SessionScope>>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

/// Run this thread inside `scope` until the guard drops.
pub fn enter(scope: Option<Arc<SessionScope>>) -> ScopeGuard {
    ScopeGuard {
        previous: CURRENT.with(|current| current.replace(scope)),
    }
}

/// A thread-start hook that puts a new thread into the scope of the thread
/// that called `inherit`. Pass it to a runtime builder's `on_thread_start`
/// so every worker and blocking thread of a runtime built by a session runs
/// in that session's scope.
pub fn inherit() -> impl Fn() + Send + Sync + 'static {
    let scope = current();
    move || CURRENT.with(|current| current.borrow_mut().clone_from(&scope))
}

/// Wrap a thread body so the new thread runs in the scope of the thread that
/// called `propagate`.
pub fn propagate<T>(body: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let scope = current();
    move || {
        let _guard = enter(scope);
        body()
    }
}

/// Refuse a durable write on a thread whose session's lifetime has ended.
/// A hosted session that is ended and rebuilt can leave a thread of the old
/// lifetime behind (stuck in a call, or one the host could not wait for);
/// if it runs on, it must not write where the rebuilt session writes now.
/// Every durable write a session makes checks this first.
///
/// # Errors
///
/// The calling thread's session lifetime has ended.
pub fn ensure_live() -> std::io::Result<()> {
    match current() {
        Some(scope) if scope.has_ended() => Err(std::io::Error::other(format!(
            "session lifetime {} has ended: its remaining threads do not write",
            scope.id
        ))),
        Some(_) | None => Ok(()),
    }
}

/// Where the lines a hosted session's code logs go: a slot its host sets on
/// the session's scope. Sessions hosted in one process share its standard
/// error, so a line that belongs to one of them is handed to that session's
/// own log instead (the host tags and stores it).
pub struct LogSink(pub Box<dyn for<'a> Fn(std::fmt::Arguments<'a>) + Send + Sync>);

/// Log one line from wherever the caller runs: to the calling session's
/// [`LogSink`] when its scope has one, otherwise to the process's standard
/// error, as `eprintln!` would (a closed standard error is ignored, never a
/// panic).
pub fn log_line(message: std::fmt::Arguments<'_>) {
    if let Some(sink) = current().and_then(|scope| scope.slot::<LogSink>()) {
        (sink.0)(message);
    } else {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "{message}");
    }
}

thread_local! {
    /// Whether a caller on this thread catches the panics raised under it.
    static PANICS_CAUGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `body` and return its panic instead of unwinding past this call
/// (`std::panic::catch_unwind`), marking the thread meanwhile so a process
/// panic hook can tell a panic that is handled here from one that ends a
/// session or goes unhandled ([`panics_are_caught`]).
///
/// # Errors
///
/// The payload `body` panicked with.
pub fn catching_panics<T>(
    body: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> std::thread::Result<T> {
    let before = PANICS_CAUGHT.replace(true);
    let outcome = std::panic::catch_unwind(body);
    PANICS_CAUGHT.set(before);
    outcome
}

/// Whether a panic raised on this thread right now is caught and handled by
/// a [`catching_panics`] caller.
#[must_use]
pub fn panics_are_caught() -> bool {
    PANICS_CAUGHT.get()
}

/// `std::env::var` for the current session.
///
/// # Errors
///
/// As `std::env::var`: the variable is unset (or hidden from this session),
/// or the process value is not valid Unicode.
pub fn var(key: impl AsRef<OsStr>) -> Result<String, std::env::VarError> {
    let key = key.as_ref();
    if let (Some(scope), Some(name)) = (current(), key.to_str()) {
        if let Some(value) = scope.env.get(name) {
            return Ok(value.clone());
        }
        if scope.removed_env.contains(name) {
            return Err(std::env::VarError::NotPresent);
        }
    }
    std::env::var(key)
}

/// `std::env::var_os` for the current session.
pub fn var_os(key: impl AsRef<OsStr>) -> Option<OsString> {
    let key = key.as_ref();
    if let (Some(scope), Some(name)) = (current(), key.to_str()) {
        if let Some(value) = scope.env.get(name) {
            return Some(OsString::from(value));
        }
        if scope.removed_env.contains(name) {
            return None;
        }
    }
    std::env::var_os(key)
}

/// `std::env::vars` for the current session (process variables whose name or
/// value is not Unicode are skipped rather than panicking).
#[must_use]
pub fn vars() -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    if let Some(scope) = current() {
        merged.extend(
            scope
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        merged.retain(|name, _| !scope.removed_env.contains(name));
    }
    merged.into_iter().collect()
}

/// `std::env::current_dir` for the current session.
///
/// # Errors
///
/// As `std::env::current_dir` on a thread with no scope.
pub fn current_dir() -> std::io::Result<PathBuf> {
    match current() {
        Some(scope) => Ok(scope.cwd.clone()),
        None => std::env::current_dir(),
    }
}

/// Give `command` the current session's environment and working directory.
/// Call it before the command's own `env` / `current_dir` calls so those
/// still win.
pub fn apply_to_command(command: &mut std::process::Command) {
    if let Some(scope) = current() {
        command.envs(&scope.env);
        for name in &scope.removed_env {
            command.env_remove(name);
        }
        if scope.cwd.is_dir() {
            command.current_dir(&scope.cwd);
        }
    }
}

/// `std::process::Command::new` in the current session's environment and
/// working directory.
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    apply_to_command(&mut command);
    command
}

/// `tokio::process::Command::new` in the current session's environment and
/// working directory.
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    apply_to_command(command.as_std_mut());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(id: &str, value: &str) -> Arc<SessionScope> {
        SessionScope::new(
            id.to_string(),
            std::env::temp_dir(),
            BTreeMap::from([("PA_SCOPE_TEST_VALUE".to_string(), value.to_string())]),
            BTreeSet::from(["PATH".to_string()]),
        )
    }

    #[test]
    fn two_scopes_in_one_process_never_see_each_others_environment() {
        let read = |scope: Arc<SessionScope>| {
            std::thread::spawn(move || {
                let _guard = enter(Some(scope));
                // A thread the session starts inherits its scope.
                let inherited = std::thread::spawn(propagate(|| var("PA_SCOPE_TEST_VALUE").ok()))
                    .join()
                    .expect("inherited thread");
                let mut child = std_command("sh");
                child.args(["-c", "printf %s \"$PA_SCOPE_TEST_VALUE\""]);
                // PATH is hidden from the scope, so the shell is named in full
                // on the systems that need it.
                let spawned = std::process::Command::new("sh")
                    .args(["-c", "printf %s \"$PA_SCOPE_TEST_VALUE\""])
                    .envs(
                        child
                            .get_envs()
                            .filter_map(|(name, value)| Some((name, value?))),
                    )
                    .output()
                    .expect("spawn sh");
                (
                    var("PA_SCOPE_TEST_VALUE").ok(),
                    inherited,
                    String::from_utf8_lossy(&spawned.stdout).into_owned(),
                    var_os("PATH"),
                    current_dir().expect("cwd"),
                )
            })
            .join()
            .expect("scope thread")
        };
        let expected = |value: &str| {
            (
                Some(value.to_string()),
                Some(value.to_string()),
                value.to_string(),
                None,
                std::env::temp_dir(),
            )
        };
        assert_eq!(
            (
                read(scope("one", "first")),
                read(scope("two", "second")),
                var("PA_SCOPE_TEST_VALUE").ok(),
            ),
            (expected("first"), expected("second"), None)
        );
    }

    /// Where a session's scope reaches from its runtime, measured: its
    /// async tasks, its blocking pool (`spawn_blocking`), a thread it
    /// starts through [`propagate`]. And where it does not: a thread it
    /// starts plainly, and work it hands to a runtime another owner built
    /// (both that runtime's tasks and its blocking pool), which see the
    /// process environment.
    #[test]
    fn a_scope_follows_its_runtimes_threads_and_no_one_elses() {
        fn read() -> Option<String> {
            var("PA_SCOPE_TEST_VALUE").ok()
        }
        let foreign = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("foreign runtime");
        let foreign_handle = foreign.handle().clone();
        let seen = std::thread::spawn(move || {
            let _guard = enter(Some(scope("one", "first")));
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .on_thread_start(inherit())
                .build()
                .expect("session runtime");
            runtime.block_on(async {
                let task = tokio::spawn(async move { read() }).await.expect("task");
                let blocking = tokio::task::spawn_blocking(read).await.expect("blocking");
                let nested = tokio::spawn(async move {
                    tokio::task::spawn_blocking(read).await.expect("nested")
                })
                .await
                .expect("nesting task");
                let propagated = tokio::task::spawn_blocking(|| {
                    std::thread::spawn(propagate(read)).join().expect("thread")
                })
                .await
                .expect("propagating");
                let plain_thread = std::thread::spawn(read).join().expect("plain thread");
                let foreign_task = foreign_handle
                    .spawn(async move { read() })
                    .await
                    .expect("foreign task");
                let foreign_blocking = foreign_handle
                    .spawn_blocking(read)
                    .await
                    .expect("foreign blocking");
                [
                    read(),
                    task,
                    blocking,
                    nested,
                    propagated,
                    plain_thread,
                    foreign_task,
                    foreign_blocking,
                ]
            })
        })
        .join()
        .expect("session thread");
        let first = Some("first".to_string());
        assert_eq!(
            seen,
            [
                first.clone(),
                first.clone(),
                first.clone(),
                first.clone(),
                first,
                None,
                None,
                None
            ]
        );
    }
}
