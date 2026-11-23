//! The `rz-*` functions through a real session kernel, against a scripted
//! rizin that speaks the pipe protocol. `tests/live.rs` runs the same
//! functions against the installed `rizin`.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::program::{LaunchError, Process, Program, Running};
use super::session::{HEAD_CAP, INTERRUPT_GRACE, TAIL_CAP};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::kernel_cell::{
    ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};
use crate::tools::tool_definition::AbortSignal;

mod lifecycle;
#[cfg(unix)]
mod live;

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
};

/// How long a test waits for something that should happen at once.
const PATIENCE: Duration = Duration::from_secs(30);

/// The pid the scripted rizin reports.
const PID: u32 = 4242;

/// What reaches a scripted rizin.
enum Event {
    Input(Vec<u8>),
    Interrupt,
    Kill,
}

/// A rizin that answers from a script and records what it is asked. Its
/// commands: `hang` answers nothing until it is interrupted, `die` ends it
/// as a crash would, `big <n>` prints `n` bytes, `nul` prints a NUL inside
/// its output, `ij` and `warnj` print JSON; anything else is an unknown
/// command.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    /// How many interrupts go unanswered before one ends a hanging command:
    /// `usize::MAX` for a rizin only a kill ends.
    unheard: AtomicUsize,
    /// What `verj` prints instead of version 0.9.1.
    version: Option<&'static str>,
    /// Told each time a command starts to hang.
    hanging: tokio::sync::Notify,
}

impl Fake {
    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The scripted rizin's own thread: what it prints for each line it is
    /// sent, and how it ended.
    fn run(&self, events: &mpsc::Receiver<Event>, mut output: std::io::PipeWriter) -> String {
        // The kernel may have stopped reading: nothing here checks a write.
        let _ = output.write_all(b"loaded\n\0");
        let (mut pending, mut lines, mut hung) = (Vec::new(), VecDeque::new(), false);
        for event in events {
            match event {
                Event::Kill => return "signal: 9 (SIGKILL)".to_string(),
                Event::Interrupt
                    if hung
                        && self
                            .unheard
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |unheard| {
                                unheard.checked_sub(1)
                            })
                            .is_err() =>
                {
                    hung = false;
                    let _ = output.write_all(b"interrupted\n\0");
                }
                Event::Interrupt => {}
                Event::Input(bytes) => {
                    pending.extend(bytes);
                    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        lines.push_back(String::from_utf8_lossy(&line).trim().to_string());
                    }
                }
            }
            while !hung {
                let Some(line) = lines.pop_front() else {
                    break;
                };
                let printed = match line.split_once(' ').unwrap_or((&line, "")) {
                    ("echo", token) => format!("{token}\n").into_bytes(),
                    ("verj", _) => self
                        .version
                        .unwrap_or(r#"{"major":0,"minor":9,"patch":1,"version":"0.9.1"}"#)
                        .as_bytes()
                        .to_vec(),
                    ("hang", _) => {
                        self.record("hang");
                        hung = true;
                        self.hanging.notify_one();
                        continue;
                    }
                    ("die", _) => {
                        let _ = output.write_all(b"Segmentation fault\n");
                        return "signal: 11 (SIGSEGV)".to_string();
                    }
                    ("big", size) => {
                        self.record(line.clone());
                        let mut text = b"start\n".to_vec();
                        text.resize(size.parse().unwrap(), b'.');
                        text.extend(b"\nend\n");
                        text
                    }
                    ("nul", _) => b"a\0b\n".to_vec(),
                    ("ij", _) => br#"{"core":{"file":"/bin/app"},"PC":null,"n":[1,2.5]}"#.to_vec(),
                    ("drj", _) => br#"{"rax":18446744073709551100}"#.to_vec(),
                    ("warnj", _) => b"WARNING: slow\n[{\"a\":1}]\n".to_vec(),
                    (unknown, _) => {
                        self.record(line.clone());
                        format!("ERROR: Command '{unknown}' does not exist.\n").into_bytes()
                    }
                };
                let _ = output.write_all(&printed);
                let _ = output.write_all(b"\0");
            }
        }
        "exit status: 0".to_string()
    }
}

/// A scripted rizin's command pipe.
struct FakeInput(mpsc::Sender<Event>);

impl Write for FakeInput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(Event::Input(bytes.to_vec()))
            .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FakeProcess {
    fake: Arc<Fake>,
    events: Mutex<mpsc::Sender<Event>>,
    thread: Mutex<Option<JoinHandle<String>>>,
    killed: AtomicBool,
}

impl Process for FakeProcess {
    fn pid(&self) -> u32 {
        PID
    }

    fn interrupt(&self) {
        self.fake.record("interrupt");
        // The scripted rizin may have ended.
        let _ = self.events.lock().unwrap().send(Event::Interrupt);
    }

    fn kill(&self) {
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.fake.record("kill");
            let _ = self.events.lock().unwrap().send(Event::Kill);
        }
    }

    fn wait(&self) -> String {
        let thread = self.thread.lock().unwrap().take().unwrap();
        thread.join().unwrap()
    }
}

/// What the kernel sees of a shared fake: every launch is a new scripted
/// rizin that records into the same fake.
struct FakeProgram(Arc<Fake>);

impl Program for FakeProgram {
    fn launch(&self, arguments: &[String]) -> Result<Running, LaunchError> {
        self.0.record(format!("launch {}", arguments.join(" ")));
        let (output, output_end) = std::io::pipe().unwrap();
        let (sender, events) = mpsc::channel();
        let fake = Arc::clone(&self.0);
        let thread = std::thread::spawn(move || fake.run(&events, output_end));
        Ok(Running {
            input: Box::new(FakeInput(sender.clone())),
            output: Box::new(output),
            process: Arc::new(FakeProcess {
                fake: Arc::clone(&self.0),
                events: Mutex::new(sender),
                thread: Mutex::new(Some(thread)),
                killed: AtomicBool::new(false),
            }),
        })
    }
}

/// Handlers whose `rizin.used` sends each fact to the returned receiver.
fn counting() -> (HostRequestHandlers, mpsc::Receiver<String>) {
    let (noted, facts) = mpsc::channel();
    let noted = Mutex::new(noted);
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "rizin.used",
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

/// The next `count` facts, sorted: they are counted in the background, in
/// no promised order.
fn counted(facts: &mpsc::Receiver<String>, count: usize) -> Vec<String> {
    let mut counted: Vec<String> = (0..count)
        .map(|_| facts.recv_timeout(PATIENCE).unwrap())
        .collect();
    counted.sort();
    counted
}

/// A session's provisioner whose rizin is `fake`.
fn provisioner(options: SchemeKernelOptions, fake: &Arc<Fake>) -> SchemeKernelProvisioner {
    let provisioner = SchemeKernelProvisioner::with_options(options);
    provisioner
        .state
        .rizin
        .launch_with(Arc::new(FakeProgram(Arc::clone(fake))));
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

/// Run `cell` and stop it once `ready` says it is under way: its status and
/// how long the stop took to end it.
async fn stopped_when(
    kernel: &dyn KernelExecutor,
    cell: &str,
    ready: impl std::future::Future<Output = ()>,
) -> (ExecuteStatus, Duration) {
    let signal = AbortSignal::new();
    let running = kernel.execute(
        cell,
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            ..NO_OPTIONS
        },
    );
    let stop = async {
        ready.await;
        signal.cancel();
        Instant::now()
    };
    let (result, stopped) = tokio::time::timeout(PATIENCE, async { tokio::join!(running, stop) })
        .await
        .expect("the stopped cell ends");
    (result.unwrap().status, stopped.elapsed())
}

/// The session a fake open returns.
fn opened(id: u64, target: &str, debug: bool, write: bool) -> Value {
    json!({
        "session": id,
        "target": target,
        "pid": PID,
        "version": "0.9.1",
        "debug": debug,
        "write": write,
        "alive": true,
        "reason": false,
    })
}

/// Every function reaches rizin with what the cell passed: an open's
/// options become its command line, a reply ends at its own fence (a NUL
/// inside it is output), JSON keeps rizin's keys and its unsigned 64-bit
/// integers, and each use is counted by kind.
#[tokio::test(flavor = "multi_thread")]
async fn the_functions_drive_rizin_over_its_pipe() {
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
            r#"(begin (define s (rz-open "/bin/app" #:write #t #:arch "arm" #:bits 32 #:base 4096 #:raw #t #:analyze #t #:flags (list "-e" "bin.cache=true"))) s)"#,
            r#"(begin (define d (rz-open "/bin/app" #:debug #t #:args (list "-v" "x") #:stdin "/tmp/in")) d)"#,
            r#"(rz-cmd s "nul")"#,
            r#"(rz-cmd (hash-ref s 'session) "pdx 3 @ main")"#,
            r#"(rz-cmdj s "ij")"#,
            r#"(number->string (hash-ref (rz-cmdj s "drj") 'rax))"#,
            r#"(rz-cmdj s "warnj")"#,
            r#"(rz-cmdj s "nul")"#,
            r#"(rz-cmdj s "")"#,
            "(rz-sessions)",
            "(rz-close d)",
            "(rz-close d)",
            r#"(rz-cmd d "ij")"#,
            r#"(rz-cmd s "a\nb")"#,
            r#"(rz-open "/bin/app" #:args (list "x"))"#,
            r#"(rz-open "-")"#,
            r#"(rz-cmd s "ij" #:timeout-ms 0)"#,
        ],
    )
    .await;
    let mut first = opened(1, "/bin/app", false, true);
    first["output"] = json!("loaded");
    let mut second = opened(2, "/bin/app", true, false);
    second["output"] = json!("loaded");
    assert_eq!(
        (returned, fake.calls(), counted(&facts, 9)),
        (
            vec![
                Ok(first),
                Ok(second),
                Ok(json!("a\u{0}b\n")),
                Ok(json!("ERROR: Command 'pdx' does not exist.\n")),
                Ok(json!({
                    "core": {"file": "/bin/app"},
                    "PC": false,
                    "n": [1, 2.5],
                })),
                Ok(json!("18446744073709551100")),
                Ok(json!([{"a": 1}])),
                Err("Error: Generic: rz-cmdj: the reply is not JSON (does the command have a `j` form?): a\u{0}b".to_string()),
                Ok(json!(false)),
                Ok(json!([
                    opened(1, "/bin/app", false, true),
                    opened(2, "/bin/app", true, false),
                ])),
                Ok(json!(true)),
                Ok(json!(false)),
                Err("Error: Generic: rz-cmd: no such session (closed, or from before a kernel restart): open one with rz-open".to_string()),
                Err("Error: Generic: rz-cmd: a command is one line of at most 8192 bytes: join commands with `;`".to_string()),
                Err("Error: Generic: rz-open: args needs #:debug #t".to_string()),
                Err("Error: Generic: rz-open: the target cannot be stdin: write the bytes to a file and open that".to_string()),
                Err("Error: Generic: rz-cmd: timeout-ms must be a positive integer or #f".to_string()),
            ],
            [
                "launch -q0 -e scr.color=0 -e scr.interactive=false -e scr.utf8=false -w -n -A -a arm -b 32 -B 4096 -e bin.cache=true /bin/app",
                "launch -q0 -e scr.color=0 -e scr.interactive=false -e scr.utf8=false -d -R stdin=/tmp/in /bin/app -v x",
                "pdx 3 @ main",
                "kill",
            ]
            .map(str::to_string)
            .to_vec(),
            [
                "command",
                "command",
                "debug_open",
                "json_command",
                "json_command",
                "json_command",
                "json_command",
                "json_command",
                "open",
            ]
            .map(str::to_string)
            .to_vec(),
        )
    );
}

/// A reply longer than a cell should hold is cut around a handle of the
/// session's spill store, and one longer than the reader keeps is counted,
/// not held: the cell gets the reply's start and its end whatever rizin
/// wrote. A JSON reply that was not kept whole is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_reply_is_cut_around_a_spill_handle_and_never_held_whole() {
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let kernel = provisioner(
        SchemeKernelOptions {
            artifact_dir: Some(dir.path().to_path_buf()),
            ..SchemeKernelOptions::default()
        },
        &fake,
    )
    .ensure(None, None)
    .await
    .unwrap();
    let written = HEAD_CAP + TAIL_CAP + 1_000_000;
    let shape = |command: &str| {
        format!(
            r#"(let* ([text (rz-cmd s "{command}")]
                      [lines (split-many text "\n")]
                      [marks (filter (lambda (line) (starts-with? line "[")) lines)])
                 (list (< (string-length text) 800000)
                       (starts-with? text "start\n....")
                       (ends-with? text "....\nend\n")
                       (map (lambda (mark) (substring mark 0 5)) marks)))"#
        )
    };
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (rz-open "/bin/app")) #t)"#,
            &shape("big 1000000"),
            &shape(&format!("big {written}")),
            &format!(r#"(rz-cmdj s "big {written}")"#),
        ],
    )
    .await;
    // The handle's own line, and for the longer reply the count of what
    // the reader dropped, sit between the two kept parts.
    assert_eq!(
        returned,
        vec![
            Ok(json!(true)),
            Ok(json!([true, true, true, ["[out:"]])),
            Ok(json!([true, true, true, ["[out:", "[rizi"]])),
            Err(format!(
                "Error: Generic: rz-cmdj: the reply is over {HEAD_CAP} bytes: narrow the command"
            )),
        ]
    );
}

/// A command that outruns its limit, or whose cell is stopped, is
/// interrupted and its session stays usable. A rizin that does not answer
/// the interrupt is killed within the grace, and its session then says it
/// is dead and why.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_ends_with_its_limit_or_its_cell_and_a_deaf_rizin_is_killed() {
    let fake = Arc::new(Fake::default());
    let (handlers, facts) = counting();
    let provisioner = provisioner(
        SchemeKernelOptions {
            host_handlers: handlers,
            ..SchemeKernelOptions::default()
        },
        &fake,
    );
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let limited = values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (rz-open "/bin/app")) #t)"#,
            r#"(rz-cmd s "hang" #:timeout-ms 50)"#,
        ],
    )
    .await;
    // The first hang's notice is spent here, so the stop below waits for
    // the second command to be under way.
    fake.hanging.notified().await;
    let (stopped, stop_took) = stopped_when(
        kernel.as_ref(),
        r#"(rz-cmd s "hang" #:timeout-ms #f)"#,
        fake.hanging.notified(),
    )
    .await;
    let usable = values(kernel.as_ref(), &[r#"(rz-cmd s "nul")"#, "(rz-sessions)"]).await;

    let deaf = Arc::new(Fake {
        unheard: AtomicUsize::new(usize::MAX),
        ..Fake::default()
    });
    provisioner
        .state
        .rizin
        .launch_with(Arc::new(FakeProgram(Arc::clone(&deaf))));
    let killing = Instant::now();
    let killed = values(
        kernel.as_ref(),
        &[
            r#"(begin (define deaf (rz-open "/bin/app")) #t)"#,
            r#"(rz-cmd deaf "hang" #:timeout-ms 50)"#,
            r#"(rz-cmd deaf "ij")"#,
            "(cadr (rz-sessions))",
        ],
    )
    .await;
    let kill_took = killing.elapsed();
    let reason = "a command did not end within 2 s of its interrupt and rizin was killed; rizin ended (signal: 9 (SIGKILL))";
    // A command that runs on is interrupted again, as often as its time
    // allows: what each rizin was asked, without the repeats.
    let asked = |fake: &Fake| {
        let mut calls = fake.calls()[1..].to_vec();
        calls.dedup();
        calls
    };
    let mut second = opened(2, "/bin/app", false, false);
    second["alive"] = json!(false);
    second["reason"] = json!(reason);
    assert_eq!(
        (
            limited,
            stopped,
            usable,
            killed,
            asked(&fake),
            asked(&deaf),
            counted(&facts, 7),
        ),
        (
            vec![
                Ok(json!(true)),
                Err("Error: Generic: rz-cmd: no reply within 50 ms: the command was interrupted and the session is still usable. What it had written ends:\ninterrupted".to_string()),
            ],
            ExecuteStatus::Aborted,
            vec![
                Ok(json!("a\u{0}b\n")),
                Ok(json!([opened(1, "/bin/app", false, false)])),
            ],
            vec![
                Ok(json!(true)),
                Err(format!("Error: Generic: rz-cmd: the session is dead: {reason}")),
                Err(format!("Error: Generic: rz-cmd: the session is dead: {reason}")),
                Ok(second),
            ],
            ["hang", "interrupt", "hang", "interrupt"]
                .map(str::to_string)
                .to_vec(),
            ["hang", "interrupt", "kill"].map(str::to_string).to_vec(),
            [
                "command",
                "died",
                "died",
                "interrupted",
                "interrupted",
                "open",
                "open",
            ]
            .map(str::to_string)
            .to_vec(),
        )
    );
    // The stopped cell left at once; the deaf rizin cost its grace, and
    // not a hang.
    assert!(
        stop_took < INTERRUPT_GRACE && (INTERRUPT_GRACE..PATIENCE).contains(&kill_took),
        "stop {stop_took:?}, kill {kill_took:?}"
    );
}

/// An interrupt can be lost: one that arrives while rizin starts a program
/// for the command is taken in the child rizin forked, and the command runs
/// on. It is interrupted again, and the session stays usable where a kill at
/// the grace would have ended it.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_whose_first_interrupt_is_lost_is_interrupted_again() {
    let fake = Arc::new(Fake {
        unheard: AtomicUsize::new(1),
        ..Fake::default()
    });
    let provisioner = provisioner(SchemeKernelOptions::default(), &fake);
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (rz-open "/bin/app")) #t)"#,
            r#"(rz-cmd s "hang" #:timeout-ms 50)"#,
            r#"(rz-cmd s "nul")"#,
        ],
    )
    .await;
    assert_eq!(
        returned,
        vec![
            Ok(json!(true)),
            Err("Error: Generic: rz-cmd: no reply within 50 ms: the command was interrupted and the session is still usable. What it had written ends:\ninterrupted".to_string()),
            Ok(json!("a\u{0}b\n")),
        ]
    );
}
