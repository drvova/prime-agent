//! The `frida-*` functions through a real session kernel, against a fake
//! Frida that records what it is asked. `live_tests.rs` runs the same
//! functions against frida-core.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::backend::{
    Backend, BackendScript, BackendSession, DetachSink, DeviceInfo, MessageSink, Outcome,
    ProcessInfo, RemoteOptions, ScriptSpec,
};
use super::cancel::Cancel;
use super::log::{ScriptLog, DATA_CAP_BYTES, LOG_CAP};
use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::kernel_cell::{
    ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};
use crate::tools::tool_definition::AbortSignal;

mod lifecycle;
#[cfg(all(feature = "frida", target_os = "linux"))]
mod live;

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
};

/// How long a test waits for something that should happen at once.
const PATIENCE: Duration = Duration::from_secs(20);

/// The pid the fake gives a spawned target, and the one its process list
/// names `target`.
const SPAWNED: u32 = 4242;
const RUNNING: u32 = 7;

/// A Frida that records every call and answers from a script: a spawn
/// yields [`SPAWNED`], a script answers what it is posted, and a source or
/// flag can make a call block until it is cancelled.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    /// The sink of each script created, in order.
    scripts: Mutex<Vec<Arc<MessageSink>>>,
    /// The detach notice of each session attached, in order.
    sessions: Mutex<Vec<DetachSink>>,
    /// An attach blocks until its call is cancelled, then succeeds: Frida
    /// finishing work its caller no longer waits for.
    attach_outlives_its_cell: bool,
    /// Told each time a call starts to block.
    blocked: tokio::sync::Notify,
}

impl Fake {
    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Block the calling thread until `cancel` is cancelled.
    fn until_cancelled(&self, cancel: &Cancel) {
        let (stopped, stop) = mpsc::channel();
        let stopped = Mutex::new(stopped);
        let _watch = cancel.watch(move || {
            // The waiter may have gone.
            let _ = stopped.lock().unwrap().send(());
        });
        self.blocked.notify_one();
        stop.recv().unwrap();
    }
}

struct FakeSession {
    fake: Arc<Fake>,
    pid: u32,
}

struct FakeScript {
    fake: Arc<Fake>,
    sink: Arc<MessageSink>,
    never_returns: bool,
}

/// What the kernel sees of a shared fake: the trait object needs the `Arc`
/// to hand to the sessions it opens.
struct FakeBackend(Arc<Fake>);

impl Backend for FakeBackend {
    fn devices(&self, _cancel: &Cancel) -> Outcome<Vec<DeviceInfo>> {
        self.0.record("devices");
        Ok(vec![DeviceInfo {
            id: "local".to_string(),
            name: "Local System".to_string(),
            kind: "local".to_string(),
        }])
    }

    fn add_remote(
        &self,
        address: &str,
        options: &RemoteOptions,
        _cancel: &Cancel,
    ) -> Outcome<DeviceInfo> {
        self.0.record(format!("add-remote {address} {options:?}"));
        Ok(DeviceInfo {
            id: format!("socket@{address}"),
            name: address.to_string(),
            kind: "remote".to_string(),
        })
    }

    fn processes(&self, device: Option<&str>, _cancel: &Cancel) -> Outcome<Vec<ProcessInfo>> {
        self.0.record(format!("processes {device:?}"));
        Ok(vec![ProcessInfo {
            pid: RUNNING,
            name: "target".to_string(),
            identifier: None,
        }])
    }

    fn applications(&self, device: Option<&str>, _cancel: &Cancel) -> Outcome<Vec<ProcessInfo>> {
        self.0.record(format!("applications {device:?}"));
        Ok(vec![ProcessInfo {
            pid: 0,
            name: "App".to_string(),
            identifier: Some("com.example.app".to_string()),
        }])
    }

    fn find_process(&self, _device: Option<&str>, name: &str, _cancel: &Cancel) -> Outcome<u32> {
        self.0.record(format!("find {name}"));
        if name == "target" {
            Ok(RUNNING)
        } else {
            Err("Process not found".to_string())
        }
    }

    fn spawn(
        &self,
        _device: Option<&str>,
        program: &str,
        args: &[String],
        _cancel: &Cancel,
    ) -> Outcome<u32> {
        self.0.record(format!("spawn {program} {args:?}"));
        Ok(SPAWNED)
    }

    fn resume(&self, _device: Option<&str>, pid: u32, _cancel: &Cancel) -> Outcome<()> {
        self.0.record(format!("resume {pid}"));
        Ok(())
    }

    fn kill(&self, _device: Option<&str>, pid: u32, _cancel: &Cancel) -> Outcome<()> {
        self.0.record(format!("kill {pid}"));
        Ok(())
    }

    fn attach(
        &self,
        _device: Option<&str>,
        pid: u32,
        on_detached: DetachSink,
        cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendSession>> {
        self.0.record(format!("attach {pid}"));
        if self.0.attach_outlives_its_cell {
            self.0.until_cancelled(cancel);
        }
        self.0.sessions.lock().unwrap().push(on_detached);
        Ok(Arc::new(FakeSession {
            fake: Arc::clone(&self.0),
            pid,
        }))
    }

    fn compile(
        &self,
        entrypoint: &str,
        project_root: Option<&str>,
        _cancel: &Cancel,
    ) -> Outcome<String> {
        self.0
            .record(format!("compile {entrypoint} {project_root:?}"));
        Ok(format!("bundle of {entrypoint}"))
    }

    fn close(&self, _cancel: &Cancel) {
        self.0.record("close");
    }
}

impl BackendSession for FakeSession {
    fn create_script(
        &self,
        spec: &ScriptSpec<'_>,
        sink: MessageSink,
        _cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendScript>> {
        self.fake.record(format!(
            "create {:?} {:?} {}",
            spec.name,
            spec.runtime,
            spec.source.lines().last().unwrap_or_default()
        ));
        if spec.source == "not javascript" {
            return Err("Script(line 1): SyntaxError: expecting ';'".to_string());
        }
        let sink = Arc::new(sink);
        self.fake.scripts.lock().unwrap().push(Arc::clone(&sink));
        Ok(Arc::new(FakeScript {
            fake: Arc::clone(&self.fake),
            sink,
            never_returns: spec.source == "while (true) {}",
        }))
    }

    fn detach(&self, _cancel: &Cancel) -> Outcome<()> {
        self.fake.record(format!("detach {}", self.pid));
        Ok(())
    }
}

impl BackendScript for FakeScript {
    fn load(&self, cancel: &Cancel) -> Outcome<()> {
        self.fake.record("load");
        if self.never_returns {
            self.fake.until_cancelled(cancel);
            self.fake.record("load cancelled");
            return Err("Operation was cancelled".to_string());
        }
        (self.sink)(r#"{"type":"send","payload":{"loaded":true}}"#, None);
        (self.sink)(r#"{"type":"log","level":"info","payload":"ready"}"#, None);
        Ok(())
    }

    fn unload(&self, _cancel: &Cancel) -> Outcome<()> {
        self.fake.record("unload");
        Ok(())
    }

    /// An `rpc.exports` call is answered at once (`fail` throws, anything
    /// else returns its export name and arguments); any other message comes
    /// back as a `send`, with the data it carried.
    fn post(&self, text: &str, data: Option<&[u8]>) -> Outcome<()> {
        self.fake.record(format!("post {text}"));
        let message: Value = serde_json::from_str(text).unwrap();
        let reply = match message.as_array().map(Vec::as_slice) {
            Some([_, id, _, export, args]) if export == "fail" => {
                let _ = args;
                json!({"type": "send", "payload": ["frida:rpc", id, "error", "boom"]})
            }
            Some([_, id, _, export, args]) => {
                json!({"type": "send", "payload": ["frida:rpc", id, "ok", [export, args]]})
            }
            Some(_) | None => json!({"type": "send", "payload": {"got": message}}),
        };
        (self.sink)(&reply.to_string(), data);
        Ok(())
    }
}

/// Handlers whose `frida.used` sends each fact to the returned receiver.
fn counting() -> (HostRequestHandlers, mpsc::Receiver<String>) {
    let (noted, facts) = mpsc::channel();
    let noted = Mutex::new(noted);
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "frida.used",
        host_handler(move |payload: HostRequestPayload| {
            // The test may have stopped listening.
            drop(
                noted.lock().unwrap().send(
                    payload.data["fact"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ),
            );
            async move { Ok(json!({})) }
        }),
    );
    (handlers, facts)
}

/// A session's provisioner whose Frida is `fake`.
fn provisioner(options: SchemeKernelOptions, fake: &Arc<Fake>) -> SchemeKernelProvisioner {
    let provisioner = SchemeKernelProvisioner::with_options(options);
    let fake = Arc::clone(fake);
    provisioner.state.frida.connect_with(Box::new(move || {
        Ok(Arc::new(FakeBackend(Arc::clone(&fake))))
    }));
    provisioner
}

/// The value of each expression as JSON, or the error it raised.
async fn values(kernel: &dyn KernelExecutor, expressions: &[&str]) -> Vec<Result<Value, String>> {
    let mut values = Vec::new();
    for expression in expressions {
        let cell = format!("(display (value->jsexpr-string {expression}))");
        let result = kernel.execute(&cell, NO_OPTIONS).await.unwrap();
        values.push(match result.error {
            None => Ok(serde_json::from_str(&result.stdout).unwrap()),
            Some(error) => Err(error.evalue),
        });
    }
    values
}

/// Every function reaches Frida with what the cell passed, a script's
/// messages stay in its log until asked for, a spawned target resumes after
/// its first script and is killed when its session detaches, and each use
/// is counted by kind.
#[tokio::test(flavor = "multi_thread")]
async fn the_functions_drive_frida_and_a_scripts_messages_stay_in_its_log() {
    let fake = Arc::new(Fake::default());
    let (handlers, facts) = counting();
    let kernel = provisioner(
        SchemeKernelOptions {
            host_handlers: handlers,
            ..SchemeKernelOptions::default()
        },
        &fake,
    )
    .ensure(None, None)
    .await
    .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            "(frida-devices)",
            r#"(frida-add-remote "10.0.0.2:27042" #:token "t" #:keepalive-interval 30)"#,
            r#"(frida-ps #:device "usb")"#,
            "(frida-ps #:applications #t)",
            r#"(frida-kill "target")"#,
            r#"(begin (define s (frida-attach "/bin/app" #:spawn #t #:args (list "-v" "x"))) s)"#,
            r#"(begin (define sc (frida-script-load s "send(1)" #:name "probe" #:runtime "qjs")) sc)"#,
            "(frida-script-messages sc)",
            "(frida-script-tail sc 1)",
            "(frida-script-wait sc)",
            r#"(frida-script-call sc "read-memory" (list 16 "x"))"#,
            r#"(frida-script-call sc "fail")"#,
            r#"(begin (frida-script-post sc (hash 'type "poke") #:data-base64 "AQID")
                      (frida-script-wait sc #:predicate (lambda (m) (hash? (hash-ref m 'payload)))))"#,
            "(frida-script-wait sc #:timeout-ms 20)",
            r#"(frida-script-load s "not javascript")"#,
            "(frida-sessions)",
            r#"(frida-compile "agent.ts" #:project-root "/p")"#,
            r#"(frida-trace s #:include (list "open*" "libc.so*!read") #:exclude-modules (list "libssl*"))"#,
            "(frida-script-unload sc)",
            "(frida-script-messages sc)",
            "(frida-detach s)",
            "(frida-sessions)",
            "(frida-detach s)",
            r#"(frida-attach "target" #:args (list "x"))"#,
            "(frida-attach (list 1))",
        ],
    )
    .await;
    // Counted in the background, in no promised order.
    let mut counted: Vec<String> = (0..9)
        .map(|_| facts.recv_timeout(PATIENCE).unwrap())
        .collect();
    counted.sort();
    assert_eq!(
        (returned, fake.calls(), counted),
        (
            vec![
                Ok(json!([{"id": "local", "name": "Local System", "type": "local"}])),
                Ok(json!({"id": "socket@10.0.0.2:27042", "name": "10.0.0.2:27042", "type": "remote"})),
                Ok(json!([{"pid": RUNNING, "name": "target"}])),
                Ok(json!([{"identifier": "com.example.app", "name": "App", "pid": 0}])),
                Ok(json!(true)),
                Ok(json!({"session": 1, "pid": SPAWNED, "spawned": true})),
                Ok(json!({"script": 2, "session": 1, "name": "probe"})),
                Ok(json!({
                    "messages": [
                        {"seq": 0, "type": "send", "payload": {"loaded": true}},
                        {"seq": 1, "type": "log", "payload": "ready", "level": "info"},
                    ],
                    "dropped": 0,
                })),
                Ok(json!("#1 info: ready")),
                Ok(json!({"seq": 0, "type": "send", "payload": {"loaded": true}})),
                Ok(json!(["readMemory", [16, "x"]])),
                Err("Error: Generic: frida-script-call: boom".to_string()),
                Ok(json!({
                    "seq": 2,
                    "type": "send",
                    "payload": {"got": {"type": "poke"}},
                    "data-base64": "AQID",
                    "data-size": 3,
                })),
                Ok(json!(false)),
                Err("Error: Generic: frida-script-load: Script(line 1): SyntaxError: expecting ';'".to_string()),
                Ok(json!([{
                    "session": 1,
                    "pid": SPAWNED,
                    "spawned": true,
                    "detached-reason": false,
                    "scripts": [2],
                }])),
                Ok(json!("bundle of agent.ts")),
                Ok(json!({"script": 3, "session": 1, "name": "trace"})),
                Ok(json!(true)),
                Err("Error: Generic: frida-script-messages: no such script (unloaded, its session detached, or from before a kernel restart)".to_string()),
                Ok(json!(true)),
                Ok(json!([])),
                Err("Error: Generic: frida-detach: no such session (detached, or from before a kernel restart)".to_string()),
                Err("Error: Generic: frida-attach: args needs #:spawn #t".to_string()),
                Err("Error: Generic: frida-attach: the target must be a pid (integer) or a process name (string)".to_string()),
            ],
            [
                "devices",
                "add-remote 10.0.0.2:27042 RemoteOptions { certificate: None, origin: None, token: Some(\"t\"), keepalive_interval: Some(30) }",
                "processes Some(\"usb\")",
                "applications None",
                "find target",
                "kill 7",
                "spawn /bin/app [\"-v\", \"x\"]",
                "attach 4242",
                "create Some(\"probe\") Qjs send(1)",
                "load",
                "resume 4242",
                "post [\"frida:rpc\",1,\"call\",\"readMemory\",[16,\"x\"]]",
                "post [\"frida:rpc\",2,\"call\",\"fail\",[]]",
                "post {\"type\":\"poke\"}",
                "create None Default not javascript",
                "compile agent.ts Some(\"/p\")",
                "create Some(\"trace\") Default )([[\"+\",\"exports:*!open*\"],[\"+\",\"exports:libc.so*!read\"],[\"-\",\"exports:libssl*!*\"]]);",
                "load",
                "unload",
                "kill 4242",
                "detach 4242",
            ]
            .map(str::to_string)
            .to_vec(),
            [
                "compile",
                "devices",
                "devices",
                "kill",
                "ps",
                "ps",
                "script_load",
                "spawn",
                "trace",
            ]
            .map(str::to_string)
            .to_vec(),
        )
    );
}
