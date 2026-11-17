//! Bytecode for the kernel venv, compiled after the venv is ready.
//!
//! uv leaves compilation to the first import. Until each module had been
//! imported once, a fresh venv paid it inside kernel starts and first
//! `import pandas` cells (about 2.0 s against 0.7 s cold for numpy plus
//! pandas). `uv pip install --compile-bytecode` would move that cost into
//! the install, but it made every venv build about 8 s slower, and after an
//! upgrade the first kernel waits on that build. So compilation runs after
//! the venv is ready instead: a low-priority `compileall` over the venv's
//! site-packages (the runtime and the default packages; editable skills
//! stay outside it and keep their lazily written `__pycache__`).
//!
//! - It runs once per install: a marker beside `.bootstrap-version` records
//!   the version text the compile covered, so a launch whose venv is already
//!   compiled spawns nothing.
//! - Kernel starts never wait on it ([`KernelBytecode::CompileInBackground`]):
//!   a detached thread spawns the compile and reaps it. The child is in its
//!   own session (unix) or detached process group (Windows), so it outlives
//!   a short-lived host.
//! - It is safe to run beside a kernel that imports from the same venv: `py_compile`
//!   writes each `.pyc` through `importlib`'s atomic temp-file-and-replace,
//!   the same path a lazy import uses.
//! - A failure is logged, never surfaced. The marker stays unwritten, so the
//!   next launch tries again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

/// Records the `.bootstrap-version` text the last finished compile covered.
const BYTECODE_MARKER_FILE: &str = ".bytecode-compiled";

/// How the venv's bytecode compile relates to [`super::super::ensure_kernel_python`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KernelBytecode {
    /// Every kernel start, prewarm and session: compile on a detached
    /// low-priority process that nothing awaits.
    #[default]
    CompileInBackground,
    /// The install-time bootstrap (`--prime-agent-bootstrap`): the user is
    /// already waiting on the installer, so the first session starts with
    /// a compiled venv and no compile competes with it.
    CompileBeforeReturn,
}

/// argv: `.bootstrap-version` path, marker path, version text. Low priority
/// first; a fresh lock (a sibling process compiling) means nothing to do; the
/// marker is written only while the version file still holds the text it
/// was asked to cover (a rebuild mid-compile leaves it unwritten).
const COMPILE_SCRIPT: &str = r#"import compileall, os, sys, sysconfig, time
version_path, marker, key = sys.argv[1:4]
if hasattr(os, "nice"):
    os.nice(19)
elif sys.platform == "win32":
    import ctypes
    _k32 = ctypes.windll.kernel32
    _k32.SetPriorityClass(_k32.GetCurrentProcess(), 0x4000)
lock = marker + ".lock"
try:
    os.close(os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
except FileExistsError:
    if time.time() - os.stat(lock).st_mtime < 600:
        sys.exit(0)
    os.unlink(lock)
    os.close(os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
try:
    for root in sorted({sysconfig.get_paths()[name] for name in ("purelib", "platlib")}):
        compileall.compile_dir(root, quiet=2, workers=0)
    with open(version_path, encoding="utf-8") as handle:
        current = handle.read()
    if current == key:
        with open(marker + ".tmp", "w", encoding="utf-8") as handle:
            handle.write(key)
        os.replace(marker + ".tmp", marker)
finally:
    try:
        os.unlink(lock)
    except OSError:
        pass
"#;

/// (venv, version text) pairs this process already launched a compile for:
/// every kernel start re-enters the bootstrap, and one compile per install
/// is enough.
static LAUNCHED: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);

/// Compile the ready venv's site-packages unless the marker already covers
/// its current install (the `.bootstrap-version` text).
#[tracing::instrument(skip_all, fields(venv = %venv.display(), mode = ?mode))]
pub(crate) async fn compile_venv_bytecode(python: &Path, venv: &Path, mode: KernelBytecode) {
    let version_path = venv.join(super::version::BOOTSTRAP_VERSION_FILE);
    let Ok(key) = std::fs::read_to_string(&version_path) else {
        return;
    };
    let marker = venv.join(BYTECODE_MARKER_FILE);
    if key.is_empty() || std::fs::read_to_string(&marker).is_ok_and(|done| done == key) {
        return;
    }
    let mut command = std::process::Command::new(python);
    command
        .args(["-P", "-s", "-c", COMPILE_SCRIPT])
        .arg(&version_path)
        .arg(&marker)
        .arg(&key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .env_clear()
        .envs(crate::kernel::python_env::inherited_python_env());
    match mode {
        KernelBytecode::CompileInBackground => {
            let first = LAUNCHED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(HashSet::new)
                .insert((venv.to_path_buf(), key));
            if !first {
                return;
            }
            #[cfg(unix)]
            crate::platform::process::set_new_session(&mut command);
            #[cfg(not(unix))]
            crate::platform::process::set_new_process_group(&mut command);
            // A plain thread, not the async runtime's blocking pool: a runtime
            // shutdown must never wait for the compile.
            let spawned = std::thread::Builder::new()
                .name("kernel-bytecode".to_string())
                .spawn(move || run_compile(&mut command));
            if let Err(error) = spawned {
                tracing::warn!(%error, "kernel venv bytecode compile thread did not start");
            }
        }
        KernelBytecode::CompileBeforeReturn => {
            crate::platform::process::set_no_window(&mut command);
            let joined = tokio::task::spawn_blocking(move || run_compile(&mut command)).await;
            if let Err(error) = joined {
                tracing::warn!(%error, "kernel venv bytecode compile task failed");
            }
        }
    }
}

/// Spawn the compile and reap it, logging a failure (never surfacing it).
fn run_compile(command: &mut std::process::Command) {
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!(code = ?status.code(), "kernel venv bytecode compile failed");
        }
        Err(error) => {
            tracing::warn!(%error, "kernel venv bytecode compile did not start");
        }
    }
}
