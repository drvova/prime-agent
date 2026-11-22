//! One kernel's rizin sessions, each under the id a cell passes back.
//!
//! The registry outlives the kernel's engine: `kill`, the end of the
//! session's lifetime and the registry's own drop end every rizin it
//! tracks, and the next kernel of the same session starts from an empty
//! one. An open that finishes after such an end keeps nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use super::program::{Installed, LaunchError, Program};
use super::session::{Failure, Reply, Session, HEAD_CAP, INTERRUPT_GRACE};
use super::{MINIMUM_VERSION, NOT_INSTALLED};
use crate::kernel::scheme::SessionEnvironment;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::kernel::spill::{SpillCopy, SpillStore};
use crate::tools::tool_definition::AbortSignal;

/// Bytes of a command's text reply a cell gets as they are; a longer reply
/// keeps this much of its start.
const SHOWN_HEAD_BYTES: usize = 512 * 1024;
/// Bytes of a longer reply's end that a cell gets.
const SHOWN_TAIL_BYTES: usize = 256 * 1024;
/// Bytes of what rizin wrote that an open's result, a timeout and a reply
/// that is not JSON show.
const NOTE_BYTES: usize = 2 * 1024;
/// The longest command line a session takes. With its fence it is written
/// in one piece to a pipe rizin has emptied, and a pipe on Linux or macOS
/// holds at least 16 KiB: the write never waits for a rizin that has
/// stopped reading.
const COMMAND_BYTES: usize = 8 * 1024;

/// One open session, as `rz-sessions` lists it.
struct Opened {
    session: Arc<Session>,
    target: String,
    version: String,
    debug: bool,
    write: bool,
}

impl Opened {
    fn json(&self, id: u64) -> Value {
        let ended = self.session.ended();
        json!({
            "session": id,
            "target": self.target,
            "pid": self.session.pid,
            "version": self.version,
            "debug": self.debug,
            "write": self.write,
            "alive": ended.is_none(),
            "reason": ended,
        })
    }
}

#[derive(Default)]
struct State {
    /// Bumped by every shutdown: an open that began under an older value
    /// keeps nothing.
    generation: u64,
    next_id: u64,
    sessions: BTreeMap<u64, Opened>,
}

/// A kernel's rizin sessions. See the module documentation.
pub(crate) struct Registry {
    program: Mutex<Arc<dyn Program>>,
    handlers: HostRequestHandlers,
    /// The session's spill store, when it is saved: a reply too long for
    /// its cell is kept there under an `out:<id>` handle.
    spill: Option<SpillStore>,
    state: Mutex<State>,
}

impl Registry {
    /// The rizin sessions of a kernel that runs commands as `environment`
    /// says: the installed `rizin`, started in the session's working
    /// directory with its environment. Its use is reported through
    /// `handlers`' `rizin.used`, and long replies are paged into a saved
    /// session's spill store.
    pub(in crate::kernel::scheme) fn new(
        handlers: HostRequestHandlers,
        environment: &SessionEnvironment,
    ) -> Self {
        Registry {
            program: Mutex::new(Arc::new(Installed {
                cwd: environment.cwd.clone(),
                env: environment.env.clone(),
            })),
            handlers,
            spill: environment
                .artifact_dir
                .as_deref()
                .map(SpillStore::for_session_dir),
            state: Mutex::default(),
        }
    }

    /// Start rizins through `program` from now on.
    #[cfg(test)]
    pub(super) fn launch_with(&self, program: Arc<dyn Program>) {
        *self.program.lock().unwrap_or_else(PoisonError::into_inner) = program;
    }

    /// Count one use for the session's adoption telemetry: a fact name,
    /// never a target, a command, or output. Fire-and-forget: the reply
    /// never matters, and no call waits for it.
    fn note(&self, fact: &str) {
        if let (Some(handler), Ok(runtime)) = (
            self.handlers.get("rizin.used"),
            tokio::runtime::Handle::try_current(),
        ) {
            runtime.spawn(handler(HostRequestPayload {
                data: json!({ "fact": fact }),
            }));
        }
    }

    /// Run the command `payload` names in its session and return the kept
    /// reply. A failure is counted and worded for the cell.
    async fn command(&self, payload: &Value, stop: &AbortSignal) -> Result<Reply, String> {
        let session = payload
            .get("session")
            .and_then(Value::as_u64)
            .and_then(|id| {
                self.state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .sessions
                    .get(&id)
                    .map(|opened| Arc::clone(&opened.session))
            })
            .ok_or(
                "no such session (closed, or from before a kernel restart): open one with rz-open",
            )?;
        let command = payload
            .get("command")
            .and_then(Value::as_str)
            .ok_or("the command must be a string")?;
        if command.len() > COMMAND_BYTES || command.contains(['\n', '\r', '\0']) {
            return Err(format!(
                "a command is one line of at most {COMMAND_BYTES} bytes: join commands with `;`"
            ));
        }
        let limit = limit(payload)?;
        session
            .run(command, limit, stop)
            .await
            .map_err(|failure| self.failed(failure, limit))
    }

    /// Count `failure` and word it for the cell.
    fn failed(&self, failure: Failure, limit: Option<Duration>) -> String {
        match failure {
            Failure::Stopped => {
                self.note("interrupted");
                "the call was abandoned: its cell was stopped (the command was interrupted)"
                    .to_string()
            }
            Failure::TimedOut(reply) => {
                self.note("interrupted");
                let written = reply.last(NOTE_BYTES);
                format!(
                    "no reply within {} ms: the command was interrupted and the session is still usable{}",
                    limit.unwrap_or_default().as_millis(),
                    if written.is_empty() {
                        String::new()
                    } else {
                        format!(". What it had written ends:\n{written}")
                    }
                )
            }
            Failure::Dead(reason) => {
                self.note("died");
                format!("the session is dead: {reason}")
            }
        }
    }

    /// Answer one `rz-*` call. `stop` is the calling cell's signal: a
    /// command in flight is interrupted when it fires.
    pub(super) async fn dispatch(
        &self,
        operation: &str,
        payload: &Value,
        stop: &AbortSignal,
    ) -> Result<Value, String> {
        match operation {
            "open" => {
                let flag = |name: &str| payload.get(name).is_some_and(|set| set != &json!(false));
                let text = |name: &str| match payload.get(name) {
                    None | Some(Value::Null | Value::Bool(false)) => Ok(None),
                    Some(Value::String(text)) => Ok(Some(text.clone())),
                    Some(Value::Number(number)) => Ok(Some(number.to_string())),
                    Some(_) => Err(format!("{name} must be a string or an integer")),
                };
                let list = |name: &str| match payload.get(name) {
                    None | Some(Value::Null | Value::Bool(false)) => Ok(Vec::new()),
                    Some(items) => items
                        .as_array()
                        .and_then(|items| {
                            items
                                .iter()
                                .map(|item| item.as_str().map(str::to_string))
                                .collect::<Option<Vec<String>>>()
                        })
                        .ok_or_else(|| format!("{name} must be a list of strings")),
                };
                let target = text("target")?
                .filter(|target| !target.is_empty())
                .ok_or("the target must be a path, a URI (malloc://512, gdb://host:port), a pid, or \"--\" for none")?;
                if target == "-" {
                    return Err(
                        "the target cannot be stdin: write the bytes to a file and open that"
                            .to_string(),
                    );
                }
                let (debug, write) = (flag("debug"), flag("write"));
                let program_arguments = list("args")?;
                if !debug && !program_arguments.is_empty() {
                    return Err("args needs #:debug #t".to_string());
                }
                // Quiet, a NUL after each reply, and plain text: no colors, no
                // prompts for a key, ASCII instead of box-drawing characters.
                let mut arguments: Vec<String> = [
                    "-q0",
                    "-e",
                    "scr.color=0",
                    "-e",
                    "scr.interactive=false",
                    "-e",
                    "scr.utf8=false",
                ]
                .map(str::to_string)
                .to_vec();
                for (set, option) in [
                    (write, "-w"),
                    (debug, "-d"),
                    (flag("raw"), "-n"),
                    (flag("analyze"), "-A"),
                ] {
                    if set {
                        arguments.push(option.to_string());
                    }
                }
                for (name, option) in [("arch", "-a"), ("bits", "-b"), ("base", "-B")] {
                    if let Some(value) = text(name)? {
                        arguments.extend([option.to_string(), value]);
                    }
                }
                if debug {
                    // A debugged program that read rizin's own stdin would take the
                    // commands meant for rizin.
                    let stdin = text("stdin")?.unwrap_or_else(|| {
                        if cfg!(windows) { "NUL" } else { "/dev/null" }.to_string()
                    });
                    arguments.extend(["-R".to_string(), format!("stdin={stdin}")]);
                }
                arguments.extend(list("flags")?);
                arguments.push(target.clone());
                arguments.extend(program_arguments);
                let limit = limit(payload)?;
                let program =
                    Arc::clone(&self.program.lock().unwrap_or_else(PoisonError::into_inner));
                let generation = self
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .generation;
                let running = program.launch(&arguments).map_err(|error| match error {
                    LaunchError::NotInstalled => {
                        self.note("not_installed");
                        NOT_INSTALLED.to_string()
                    }
                    LaunchError::Failed(reason) => reason,
                })?;
                let session = Arc::new(
                    Session::start(running)
                        .map_err(|error| format!("could not read rizin's output: {error}"))?,
                );
                // What rizin writes while it loads the target, then its
                // version: a rizin too old to say it is too old to drive.
                let started = session
                    .run("", limit, stop)
                    .await
                    .map_err(|failure| self.failed(failure, limit))?;
                let version = session
                    .run("verj", Some(INTERRUPT_GRACE), stop)
                    .await
                    .map_err(|failure| self.failed(failure, Some(INTERRUPT_GRACE)))?
                    .last(NOTE_BYTES);
                let version = serde_json::from_str::<Value>(&version)
                    .ok()
                    .and_then(|version| {
                        let part = |name: &str| version.get(name).and_then(Value::as_u64);
                        Some((part("major")?, part("minor")?, part("patch")?))
                    })
                    .filter(|version| *version >= MINIMUM_VERSION)
                    .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                    .ok_or_else(|| {
                        let (major, minor, patch) = MINIMUM_VERSION;
                        format!(
                            "the installed rizin is older than {major}.{minor}.{patch}, the oldest \
                             this can drive (`rizin -v` shows its version): upgrade it"
                        )
                    })?;
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.generation != generation {
                    session.end("its kernel ended while it was opening");
                    return Err("the kernel ended while rizin was opening".to_string());
                }
                state.next_id += 1;
                let id = state.next_id;
                let opened = Opened {
                    session,
                    target,
                    version,
                    debug,
                    write,
                };
                let mut described = opened.json(id);
                described["output"] = json!(started.last(NOTE_BYTES));
                state.sessions.insert(id, opened);
                drop(state);
                self.note(if debug { "debug_open" } else { "open" });
                Ok(described)
            }
            "cmd" => {
                let reply = self.command(payload, stop).await?;
                self.note("command");
                let kept: Vec<u8> = reply.head.iter().chain(&reply.tail).copied().collect();
                // Rizin clears its progress line with an escape sequence
                // even when it writes to a pipe.
                let text = String::from_utf8_lossy(&kept).replace("\u{1b}[2K\r", "");
                if reply.dropped == 0 && text.len() <= SHOWN_HEAD_BYTES + SHOWN_TAIL_BYTES {
                    return Ok(json!(text));
                }
                let mut head = SHOWN_HEAD_BYTES.min(text.len());
                let mut tail = text.len().saturating_sub(SHOWN_TAIL_BYTES).max(head);
                while !text.is_char_boundary(head) {
                    head -= 1;
                }
                while !text.is_char_boundary(tail) {
                    tail += 1;
                }
                let not_shown = text[head..tail].chars().count();
                let lost = if reply.dropped > 0 {
                    format!(
                        "\n[rizin wrote {} bytes more than the {HEAD_CAP} kept: narrow the command]",
                        reply.dropped
                    )
                } else {
                    String::new()
                };
                let marker = match &self.spill {
                    Some(store) => store.spill(SpillCopy::of_text(&text)).marker(not_shown),
                    None => format!("[{not_shown} chars not shown: narrow the command]"),
                };
                Ok(json!(format!(
                    "{}\n{marker}{lost}\n{}",
                    &text[..head],
                    &text[tail..]
                )))
            }
            "cmdj" => {
                let reply = self.command(payload, stop).await?;
                self.note("json_command");
                if reply.dropped > 0 {
                    return Err(format!(
                        "the reply is over {HEAD_CAP} bytes: narrow the command"
                    ));
                }
                let kept: Vec<u8> = reply.head.iter().chain(&reply.tail).copied().collect();
                let text = String::from_utf8_lossy(&kept);
                let text = text.trim();
                if text.is_empty() {
                    return Ok(Value::Bool(false));
                }
                // Warnings and errors arrive before the JSON they belong
                // to: it starts at the first line that parses to the end.
                std::iter::once(0)
                    .chain(text.match_indices('\n').map(|(at, _)| at + 1))
                    .filter(|at| text[*at..].starts_with(['{', '[']))
                    .find_map(|at| serde_json::from_str::<Value>(&text[at..]).ok())
                    .ok_or_else(|| {
                        format!(
                            "the reply is not JSON (does the command have a `j` form?): {}",
                            reply.last(NOTE_BYTES)
                        )
                    })
            }
            "sessions" => Ok(Value::Array(
                self.state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .sessions
                    .iter()
                    .map(|(id, opened)| opened.json(*id))
                    .collect(),
            )),
            "close" => {
                let closed = payload
                    .get("session")
                    .and_then(Value::as_u64)
                    .and_then(|id| {
                        self.state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .sessions
                            .remove(&id)
                    });
                Ok(json!(closed.is_some_and(|opened| {
                    opened.session.end("the session was closed");
                    true
                })))
            }
            unknown => Err(format!("unknown operation {unknown}")),
        }
    }

    /// End every session: each rizin and everything it started is killed.
    /// Never blocks: the reader of each reaps it.
    pub(crate) fn shutdown(&self) {
        let sessions = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.generation += 1;
            std::mem::take(&mut state.sessions)
        };
        for opened in sessions.into_values() {
            opened.session.end("its kernel ended");
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The limit `payload`'s `timeout-ms` sets: none when it is `#f`.
fn limit(payload: &Value) -> Result<Option<Duration>, String> {
    match payload.get("timeout-ms") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(None),
        Some(limit) => limit
            .as_u64()
            .filter(|limit| *limit > 0)
            .map(|limit| Some(Duration::from_millis(limit)))
            .ok_or_else(|| "timeout-ms must be a positive integer or #f".to_string()),
    }
}
