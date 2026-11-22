//! A python state's call: one short-lived, isolated Python subprocess that
//! runs `callable(inputs)` and writes the returned dict as JSON.
//!
//! The kernel itself no longer carries a Python interpreter, so the call
//! uses the one on `PATH` (`python3`, `python` on Windows); a machine
//! without it fails the state with "python state could not start". Inputs
//! arrive as JSON on stdin; everything the call prints, its own
//! subprocesses included, goes to stderr; the returned dict is the only
//! thing on stdout. Both streams go to temporary files, so a large output
//! never blocks the call on a full pipe.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Map, Value};

use super::python_text::{repr_str, type_name};
use super::spec::{declared_port_types, field};
use crate::platform;

/// A python call's time limit when neither the state nor the run sets
/// `budget_ms`.
pub(super) const PYTHON_STATE_TIMEOUT_MS_DEFAULT: u64 = 60_000;
/// How much of a failed call's stderr its error text carries.
const PYTHON_STDERR_TAIL_CAP: u64 = 2000;

/// The `python -I -c` program that runs one call.
const PYTHON_RUNNER: &str = r#"
import asyncio, importlib, inspect, json, os, sys
sys.path.insert(0, os.getcwd())
inputs = json.load(sys.stdin.buffer)
result_stream = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr
result = getattr(importlib.import_module(sys.argv[1]), sys.argv[2])(inputs)
if inspect.iscoroutine(result):
    result = asyncio.run(result)
json.dump(result, result_stream)
result_stream.close()
"#;

/// One running or exited python call.
pub(super) struct PythonCall {
    pid: Option<u32>,
    /// The exit code once the call has exited: -1 for a call ended by a
    /// signal or whose wait failed.
    exited: tokio::sync::watch::Receiver<Option<i32>>,
    stdout: (std::fs::File, PathBuf),
    stderr: (std::fs::File, PathBuf),
}

/// A fresh temporary file, open for reading and writing.
fn scratch_file(kind: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    let path = std::env::temp_dir().join(format!(
        "prime-agent-factory-{kind}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((file, path))
}

impl PythonCall {
    /// Start `target`'s callable on `inputs` in `cwd`, which is on the
    /// call's import path. Must be called from within a tokio runtime.
    ///
    /// # Errors
    ///
    /// The interpreter or a temporary file could not be opened.
    pub(super) fn start(
        target: &Map<String, Value>,
        inputs: &Map<String, Value>,
        cwd: &Path,
    ) -> std::io::Result<Self> {
        let (mut stdin, stdin_path) = scratch_file("stdin")?;
        let prepared = stdin
            .write_all(Value::Object(inputs.clone()).to_string().as_bytes())
            .and_then(|()| stdin.seek(SeekFrom::Start(0)))
            .and_then(|_| Ok((scratch_file("stdout")?, scratch_file("stderr")?)));
        // The call reads its inputs through the open handle.
        let _ = std::fs::remove_file(&stdin_path);
        let (stdout, stderr) = prepared?;
        let mut command = pa_types::session_scope::tokio_command(if cfg!(windows) {
            "python"
        } else {
            "python3"
        });
        command
            .arg("-I")
            .arg("-c")
            .arg(PYTHON_RUNNER)
            .arg(field(target, "import").as_str().unwrap_or_default())
            .arg(field(target, "callable").as_str().unwrap_or_default())
            .current_dir(cwd)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout.0.try_clone()?))
            .stderr(Stdio::from(stderr.0.try_clone()?))
            .kill_on_drop(true);
        // The call has no kernel: nothing of the session's kernel wiring
        // reaches it.
        for (name, _) in pa_types::session_scope::vars() {
            if name.starts_with("RLM_") {
                command.env_remove(name);
            }
        }
        #[cfg(unix)]
        platform::process::set_new_session(command.as_std_mut());
        #[cfg(not(unix))]
        platform::set_new_process_group(command.as_std_mut());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&stdout.1);
                let _ = std::fs::remove_file(&stderr.1);
                return Err(error);
            }
        };
        let pid = child.id();
        let (exit, exited) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code());
            // No one may be waiting any more: the run was dropped.
            let _ = exit.send(Some(code.unwrap_or(-1)));
        });
        Ok(PythonCall {
            pid,
            exited,
            stdout,
            stderr,
        })
    }

    /// A handle that resolves when the call exits.
    pub(super) fn exit_watch(&self) -> tokio::sync::watch::Receiver<Option<i32>> {
        self.exited.clone()
    }

    /// The call's exit code once it has exited: `None` while it runs, -1
    /// for a call ended by a signal.
    pub(super) fn exit_code(&self) -> Option<i32> {
        *self.exited.borrow()
    }

    /// Kill the call's whole process group: a call lives no longer than its
    /// exit, so anything it left running goes too.
    fn kill(&self) {
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            #[cfg(unix)]
            let _ = platform::process::signal_process_group(pid, platform::Signal::Kill);
            #[cfg(not(unix))]
            let _ = platform::kill_process_group_or_pid(pid);
        }
    }

    /// Kill a running call, wait for it to exit, and release its files.
    pub(super) async fn stop(mut self) {
        self.kill();
        // An error means the waiter is gone, and the call with it.
        let _ = self.exited.wait_for(Option::is_some).await;
        self.finish();
    }

    /// The stdout and the stderr tail of an exited call; its files are
    /// released.
    pub(super) fn finish(mut self) -> (String, String) {
        self.kill();
        let mut stdout = Vec::new();
        let _ = self
            .stdout
            .0
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.stdout.0.read_to_end(&mut stdout));
        let mut stderr = Vec::new();
        let length = self.stderr.0.seek(SeekFrom::End(0)).unwrap_or(0);
        let _ = self
            .stderr
            .0
            .seek(SeekFrom::Start(
                length.saturating_sub(PYTHON_STDERR_TAIL_CAP),
            ))
            .and_then(|_| self.stderr.0.read_to_end(&mut stderr));
        let _ = std::fs::remove_file(&self.stdout.1);
        let _ = std::fs::remove_file(&self.stderr.1);
        (
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).trim().to_string(),
        )
    }
}

/// A python call's outputs from its stdout.
///
/// # Errors
///
/// The sentence the attempt fails with when stdout is not one object whose
/// keys are exactly the declared outputs, text outputs being strings.
pub(super) fn python_outputs(
    state_spec: &Map<String, Value>,
    stdout: &str,
) -> Result<Map<String, Value>, String> {
    let outputs = match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(outputs)) => outputs,
        Ok(other) => {
            return Err(format!(
                "python state returned {}, not a dict",
                type_name(&other)
            ))
        }
        Err(_) => return Err("python state did not write one JSON object to stdout".to_string()),
    };
    let declared = declared_port_types(state_spec, "outputs");
    let sorted = |mut names: Vec<&str>| {
        names.sort_unstable();
        let quoted: Vec<String> = names.into_iter().map(repr_str).collect();
        format!("[{}]", quoted.join(", "))
    };
    let returned = sorted(outputs.keys().map(String::as_str).collect());
    let expected = sorted(declared.iter().map(|(name, _)| *name).collect());
    if returned != expected {
        return Err(format!(
            "python state returned keys {returned} but declares outputs {expected}"
        ));
    }
    for (name, port_type) in declared {
        if port_type == "text" && !outputs[name].is_string() {
            return Err(format!(
                "python state output {} is text but the call returned {}",
                repr_str(name),
                type_name(&outputs[name])
            ));
        }
    }
    Ok(outputs)
}
