//! One session's Frida state: its connection, every session it attached and
//! every script it loaded, each under the id a cell passes back.
//!
//! The registry outlives the kernel's engine: `kill` shuts it down and the
//! next kernel of the same session starts from an empty one. Everything it
//! tracks is ended by [`Registry::shutdown`]; a call still inside Frida at
//! that moment finds on its return that it was shut down and undoes what it
//! made, so nothing attached or spawned is left without an owner.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use super::backend::{Backend, BackendScript, BackendSession, Outcome, RemoteOptions, Runtime};
use super::cancel::Cancel;
use super::log::ScriptLog;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};

/// How long one Frida call of a teardown may take: a Frida that does not
/// answer cannot hold a session's end longer than this per call.
const TEARDOWN_LIMIT: Duration = Duration::from_secs(3);
/// What a wait or an `rpc.exports` call waits when the cell names no limit.
const DEFAULT_WAIT_MS: u64 = 30_000;

/// The tracer a `frida-trace` script runs: `(agent)(spec)`.
const TRACE_AGENT: &str = include_str!("trace_agent.js");

/// Opens the session's connection to Frida, or says why there is none.
pub(super) type Connect = Box<dyn Fn() -> Outcome<Arc<dyn Backend>> + Send + Sync>;

/// What a session's own detach notice reaches: the reason, and the logs of
/// its scripts, which end with it.
#[derive(Default)]
struct Ended {
    reason: Mutex<Option<String>>,
    logs: Mutex<Vec<Arc<ScriptLog>>>,
}

impl Ended {
    /// The session is over for `reason`: its scripts say nothing more.
    fn end(&self, reason: &str) {
        self.reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| reason.to_string());
        for log in self
            .logs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            log.close();
        }
    }
}

struct Session {
    attached: Arc<dyn BackendSession>,
    device: Option<String>,
    pid: u32,
    /// The registry started the target, and so ends it.
    spawned: bool,
    /// A spawned target stays suspended until `frida-resume`.
    held: bool,
    resumed: AtomicBool,
    ended: Arc<Ended>,
}

struct Script {
    session: u64,
    loaded: Arc<dyn BackendScript>,
    log: Arc<ScriptLog>,
}

#[derive(Default)]
struct State {
    /// Bumped by every shutdown: a call that began under an older value
    /// keeps nothing it made.
    generation: u64,
    next_id: u64,
    backend: Option<Arc<dyn Backend>>,
    sessions: BTreeMap<u64, Arc<Session>>,
    scripts: BTreeMap<u64, Arc<Script>>,
}

/// A session's Frida state. See the module documentation.
pub(crate) struct Registry {
    connect: Mutex<Connect>,
    handlers: HostRequestHandlers,
    state: Mutex<State>,
}

impl Registry {
    /// A registry that reaches Frida through `connect` on its first call,
    /// and reports its use through `handlers`' `frida.used`.
    pub(super) fn new(connect: Connect, handlers: HostRequestHandlers) -> Self {
        Registry {
            connect: Mutex::new(connect),
            handlers,
            state: Mutex::default(),
        }
    }

    /// Reach Frida through `connect` from now on.
    #[cfg(test)]
    pub(super) fn connect_with(&self, connect: Connect) {
        *self.connect.lock().unwrap_or_else(PoisonError::into_inner) = connect;
    }

    /// Count one use for the session's adoption telemetry: a fact name,
    /// never a target, a pid, a device, a script or a path. Fire-and-forget:
    /// the reply never matters, and no call waits for it.
    fn note(&self, fact: &str) {
        if let (Some(handler), Ok(runtime)) = (
            self.handlers.get("frida.used"),
            tokio::runtime::Handle::try_current(),
        ) {
            runtime.spawn(handler(HostRequestPayload {
                data: json!({ "fact": fact }),
            }));
        }
    }

    /// End one session: a target the registry spawned is killed unless it
    /// already exited, the session detaches, and its scripts' logs close.
    fn end_session(backend: &dyn Backend, session: &Session, cancel: &Cancel) -> Outcome<()> {
        let exited = session
            .ended
            .reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            == Some("process-terminated");
        // Killed first: a target nobody controls any more must not stay
        // suspended or run on instrumented, and a detach can wait on a
        // script that never yields.
        let killed = if session.spawned && !exited {
            backend.kill(session.device.as_deref(), session.pid, cancel)
        } else {
            Ok(())
        };
        let detached = session.attached.detach(cancel);
        session.ended.end("application-requested");
        if session.spawned {
            // The target is dead (or its kill failed, which is the error):
            // what detaching from it said no longer matters.
            killed
        } else {
            detached
        }
    }

    /// A signal for one teardown call a cell asked for: cancelled with the
    /// cell's `cancel`, and after [`TEARDOWN_LIMIT`] on its own, because a
    /// script that never yields keeps Frida from answering. The watch must
    /// be kept for as long as the signal is used.
    fn bounded(cancel: &Cancel) -> (Cancel, super::cancel::Watch) {
        let bounded = Cancel::deadline(TEARDOWN_LIMIT);
        let watch = cancel.watch({
            let bounded = bounded.clone();
            move || bounded.cancel()
        });
        (bounded, watch)
    }

    /// End everything: every session detaches, every spawned target is
    /// killed, and the connection closes. Each Frida call is bounded, so
    /// this returns even when Frida does not answer. The registry is empty
    /// and usable again afterwards.
    pub(crate) fn shutdown(&self) {
        let (backend, sessions) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.generation += 1;
            state.scripts.clear();
            (state.backend.take(), std::mem::take(&mut state.sessions))
        };
        let Some(backend) = backend else {
            return;
        };
        for session in sessions.values() {
            if let Err(error) =
                Self::end_session(backend.as_ref(), session, &Cancel::deadline(TEARDOWN_LIMIT))
            {
                tracing::warn!(
                    "frida: ending the session on pid {} failed: {error}",
                    session.pid
                );
            }
        }
        backend.close(&Cancel::deadline(TEARDOWN_LIMIT));
    }

    /// Run one operation of the Scheme API and return its reply. Blocks
    /// until Frida answers or `cancel` is cancelled.
    ///
    /// # Errors
    ///
    /// Frida is unavailable, the arguments are malformed, an id names
    /// nothing, the call was cancelled, or Frida refused (its own message).
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per operation of the API; each is a few lines over the same lookups"
    )]
    pub(super) fn dispatch(
        &self,
        operation: &str,
        payload: &Value,
        cancel: &Cancel,
    ) -> Outcome<Value> {
        let (backend, generation) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let backend = if let Some(backend) = &state.backend {
                Arc::clone(backend)
            } else {
                let connected = (self.connect.lock().unwrap_or_else(PoisonError::into_inner))();
                let backend = connected.inspect_err(|_| self.note("unavailable"))?;
                state.backend = Some(Arc::clone(&backend));
                backend
            };
            (backend, state.generation)
        };
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let flag = |key: &str| payload.get(key).and_then(Value::as_bool) == Some(true);
        let limit = Duration::from_millis(
            payload
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WAIT_MS),
        );
        let device = text("device");
        let session = |state: &State| {
            payload
                .get("session")
                .and_then(Value::as_u64)
                .and_then(|id| Some((id, Arc::clone(state.sessions.get(&id)?))))
                .ok_or_else(|| {
                    "no such session (detached, or from before a kernel restart)".to_string()
                })
        };
        let script = || {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            payload
                .get("script")
                .and_then(Value::as_u64)
                .and_then(|id| state.scripts.get(&id).map(Arc::clone))
                .ok_or_else(|| "no such script (unloaded, its session detached, or from before a kernel restart)".to_string())
        };
        // A pid, or the pid of the one process with that name.
        let pid = || match payload.get("target") {
            Some(Value::Number(pid)) => pid
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| "the pid is out of range".to_string()),
            Some(Value::String(name)) => backend.find_process(device, name, cancel),
            Some(Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_)) | None => {
                Err("the target must be a pid (integer) or a process name (string)".to_string())
            }
        };
        match operation {
            "devices" => {
                let devices = backend.devices(cancel)?;
                self.note("devices");
                Ok(devices
                    .into_iter()
                    .map(
                        |device| json!({"id": device.id, "name": device.name, "type": device.kind}),
                    )
                    .collect())
            }
            "add-remote" => {
                let device = backend.add_remote(
                    text("address").ok_or("the address must be a string")?,
                    &RemoteOptions {
                        certificate: text("certificate").map(str::to_string),
                        origin: text("origin").map(str::to_string),
                        token: text("token").map(str::to_string),
                        keepalive_interval: payload
                            .get("keepalive_interval")
                            .and_then(Value::as_i64)
                            .and_then(|interval| i32::try_from(interval).ok()),
                    },
                    cancel,
                )?;
                self.note("devices");
                Ok(json!({"id": device.id, "name": device.name, "type": device.kind}))
            }
            "ps" => {
                let listed = if flag("applications") {
                    backend.applications(device, cancel)
                } else {
                    backend.processes(device, cancel)
                }?;
                self.note("ps");
                Ok(listed
                    .into_iter()
                    .map(|process| match process.identifier {
                        Some(identifier) => json!({
                            "identifier": identifier, "name": process.name, "pid": process.pid,
                        }),
                        None => json!({"pid": process.pid, "name": process.name}),
                    })
                    .collect())
            }
            "kill" => {
                backend.kill(device, pid()?, cancel)?;
                self.note("kill");
                Ok(json!(true))
            }
            "attach" => {
                let spawn = flag("spawn");
                let args: Vec<String> = payload
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| {
                        args.iter()
                            .map(|arg| arg.as_str().map(str::to_string))
                            .collect::<Option<_>>()
                            .ok_or("args must be a list of strings")
                    })
                    .transpose()?
                    .unwrap_or_default();
                let pid = if spawn {
                    backend.spawn(
                        device,
                        text("target").ok_or("a spawned target must be a program path or an application identifier (string)")?,
                        &args,
                        cancel,
                    )?
                } else if args.is_empty() {
                    pid()?
                } else {
                    return Err("args needs #:spawn #t".to_string());
                };
                let ended = Arc::new(Ended::default());
                let attached = backend.attach(
                    device,
                    pid,
                    Box::new({
                        let ended = Arc::clone(&ended);
                        move |reason| ended.end(reason)
                    }),
                    cancel,
                );
                // Tracked from here on, unless the cell or the kernel ended
                // while Frida worked: then nothing made is kept.
                let kept = match attached {
                    Ok(session) => {
                        let session = Arc::new(Session {
                            attached: session,
                            device: device.map(str::to_string),
                            pid,
                            spawned: spawn,
                            held: flag("pause"),
                            resumed: AtomicBool::new(!spawn),
                            ended,
                        });
                        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                        if cancel.is_cancelled() || state.generation != generation {
                            Err((
                                Some(session),
                                "the call was abandoned: its cell or kernel was stopped"
                                    .to_string(),
                            ))
                        } else {
                            state.next_id += 1;
                            let id = state.next_id;
                            state.sessions.insert(id, session);
                            Ok(id)
                        }
                    }
                    Err(reason) => Err((None, reason)),
                };
                match kept {
                    Ok(id) => {
                        self.note(if spawn { "spawn" } else { "attach" });
                        Ok(json!({"session": id, "pid": pid, "spawned": spawn}))
                    }
                    Err((session, reason)) => {
                        let undo = Cancel::deadline(TEARDOWN_LIMIT);
                        let undone = match session {
                            Some(session) => Self::end_session(backend.as_ref(), &session, &undo),
                            None if spawn => backend.kill(device, pid, &undo),
                            None => Ok(()),
                        };
                        Err(match undone {
                            Ok(()) => reason,
                            Err(left) => format!("{reason} (and ending the target failed: {left})"),
                        })
                    }
                }
            }
            "sessions" => {
                let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                Ok(state
                    .sessions
                    .iter()
                    .map(|(id, session)| {
                        json!({
                            "session": id,
                            "pid": session.pid,
                            "spawned": session.spawned,
                            "detached-reason": session
                                .ended
                                .reason
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .clone()
                                .map_or(json!(false), Value::String),
                            "scripts": state
                                .scripts
                                .iter()
                                .filter(|(_, script)| script.session == *id)
                                .map(|(script, _)| *script)
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect())
            }
            "resume" => {
                let (_, session) =
                    session(&self.state.lock().unwrap_or_else(PoisonError::into_inner))?;
                if !session.resumed.swap(true, Ordering::SeqCst) {
                    backend.resume(session.device.as_deref(), session.pid, cancel)?;
                }
                Ok(json!(true))
            }
            "detach" => {
                let (id, session) =
                    session(&self.state.lock().unwrap_or_else(PoisonError::into_inner))?;
                let (bounded, _watch) = Self::bounded(cancel);
                // Still tracked if this fails: the kernel's end tries again.
                Self::end_session(backend.as_ref(), &session, &bounded)?;
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                state.sessions.remove(&id);
                state.scripts.retain(|_, script| script.session != id);
                Ok(json!(true))
            }
            "script-load" | "trace" => {
                let (session_id, session) =
                    session(&self.state.lock().unwrap_or_else(PoisonError::into_inner))?;
                let source = if operation == "trace" {
                    format!(
                        "({TRACE_AGENT})({});",
                        payload.get("spec").unwrap_or(&Value::Null)
                    )
                } else {
                    text("source")
                        .ok_or("the script source must be a string")?
                        .to_string()
                };
                let log = Arc::new(ScriptLog::default());
                let created = session.attached.create_script(
                    &super::backend::ScriptSpec {
                        source: &source,
                        name: text("name"),
                        runtime: match text("runtime") {
                            None => Runtime::Default,
                            Some("qjs") => Runtime::Qjs,
                            Some("v8") => Runtime::V8,
                            Some(other) => {
                                return Err(format!(
                                    "the runtime must be \"qjs\" or \"v8\", not {other:?}"
                                ))
                            }
                        },
                    },
                    Box::new({
                        let log = Arc::clone(&log);
                        move |text, data| log.receive(text, data)
                    }),
                    cancel,
                )?;
                // Tracked before it runs: a script whose load is abandoned
                // is unloaded with its session.
                session
                    .ended
                    .logs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Arc::clone(&log));
                let id = {
                    let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                    if state.generation != generation || !state.sessions.contains_key(&session_id) {
                        return Err(
                            "the session was detached while the script was created".to_string()
                        );
                    }
                    state.next_id += 1;
                    let id = state.next_id;
                    state.scripts.insert(
                        id,
                        Arc::new(Script {
                            session: session_id,
                            loaded: Arc::clone(&created),
                            log,
                        }),
                    );
                    id
                };
                if let Err(error) = created.load(cancel) {
                    self.state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .scripts
                        .remove(&id);
                    return Err(error);
                }
                self.note(if operation == "trace" {
                    "trace"
                } else {
                    "script_load"
                });
                if !session.held && !session.resumed.swap(true, Ordering::SeqCst) {
                    backend.resume(session.device.as_deref(), session.pid, cancel)?;
                }
                Ok(
                    json!({"script": id, "session": session_id, "name": text("name").map_or(json!(false), |name| json!(name))}),
                )
            }
            "script-messages" => {
                let script = script()?;
                let (messages, dropped) = script.log.snapshot(
                    payload
                        .get("n")
                        .and_then(Value::as_u64)
                        .map(|newest| usize::try_from(newest).unwrap_or(usize::MAX)),
                );
                Ok(if flag("tail") {
                    json!(messages
                        .iter()
                        .map(super::log::Message::line)
                        .collect::<Vec<_>>()
                        .join("\n"))
                } else {
                    json!({
                        "messages": messages.iter().map(super::log::Message::json).collect::<Vec<_>>(),
                        "dropped": dropped,
                    })
                })
            }
            "script-wait" => {
                let (message, left) = script()?.log.next_message(limit, cancel)?;
                Ok(json!({
                    "message": message.map_or(json!(false), |message| message.json()),
                    "remaining-ms": u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
                }))
            }
            "script-call" => {
                let script = script()?;
                // `rpc.exports` names are camelCase: `read-memory` and
                // `read_memory` both reach `readMemory`.
                let mut export = String::new();
                let mut upper = false;
                for letter in text("export").ok_or("the export must be a string")?.chars() {
                    if matches!(letter, '-' | '_') && !export.is_empty() {
                        upper = true;
                    } else if upper {
                        export.extend(letter.to_uppercase());
                        upper = false;
                    } else {
                        export.push(letter);
                    }
                }
                let call = script.log.begin_call();
                script.loaded.post(
                    &json!([
                        "frida:rpc",
                        call,
                        "call",
                        export,
                        payload.get("args").cloned().unwrap_or_else(|| json!([])),
                    ])
                    .to_string(),
                    None,
                )?;
                script.log.reply(call, limit, cancel)
            }
            "script-post" => {
                let data = text("data_base64")
                    .map(|data| {
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|error| format!("the data is not base64: {error}"))
                    })
                    .transpose()?;
                script()?.loaded.post(
                    &payload.get("message").unwrap_or(&Value::Null).to_string(),
                    data.as_deref(),
                )?;
                Ok(json!(true))
            }
            "script-unload" => {
                let script = {
                    let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                    payload
                        .get("script")
                        .and_then(Value::as_u64)
                        .and_then(|id| state.scripts.remove(&id))
                        .ok_or("no such script (already unloaded, or its session detached)")?
                };
                script.log.close();
                let (bounded, _watch) = Self::bounded(cancel);
                script.loaded.unload(&bounded)?;
                Ok(json!(true))
            }
            "compile" => {
                let bundle = backend.compile(
                    text("entrypoint").ok_or("the entrypoint must be a path (string)")?,
                    text("project_root"),
                    cancel,
                )?;
                self.note("compile");
                Ok(json!(bundle))
            }
            unknown => Err(format!("unknown operation {unknown}")),
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.shutdown();
    }
}
