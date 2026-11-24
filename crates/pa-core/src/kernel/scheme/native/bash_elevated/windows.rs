//! Elevated commands on Windows: the session's elevated helper.
//!
//! A program gets administrator rights on Windows only by being started
//! elevated, after the user approves a UAC prompt; this process is never
//! elevated itself. The first elevated command of a session starts Prime
//! Agent again, elevated and with no window (`prime-agent --elevation-helper
//! <pipe> <session-pid> <token>`, through `ShellExecuteExW`'s `runas` verb),
//! which connects back over a pipe this process created, and from then on
//! runs the session's elevated commands: each in a kill-on-close job object
//! of the helper's, its standard output and error piped back here
//! ([`protocol`](super::protocol)), so the result is captured as any
//! command's. The command runs on the built-in shell inside the helper, or on
//! the session's shell program when its settings ask for one.
//!
//! The helper ends, and its commands with it, when the pipe closes: the
//! session's kernel ends or is killed, this process dies, or no elevated
//! command has run for [`IDLE`] (ten minutes): the next elevated command
//! then asks the user again. A stopped command is ended by the helper on the
//! session's request ([`Message::Cancel`]), since this process may not end
//! an elevated one.
//!
//! # Threat model
//!
//! What the helper offers is administrator rights to whoever can send it a
//! command, so only this session may. The attacker considered is another
//! program running as the same user, unelevated (Medium integrity): it can
//! read this process's memory and anything the user can read, but holds the
//! Administrators group only for denial (UAC's filtered token).
//!
//! - The pipe has an unguessable name, is created first
//!   (`FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name another program took fails
//!   here instead of being shared), admits one instance and no remote
//!   client, and its access list ([`PIPE_SDDL`]) admits only `SYSTEM` and
//!   elevated administrators. An unelevated program of the user can neither
//!   connect nor create a rival instance, and an `OWNER RIGHTS` entry keeps
//!   the user's implicit right to rewrite the access list from it.
//! - The connecting process must be the very process the UAC launch started
//!   (`GetNamedPipeClientProcessId` against the launch's process handle),
//!   and its hello must name that process and carry the one-time token. The
//!   token is passed on the helper's command line, which only processes that
//!   may read an elevated process's memory (elevated administrators, who
//!   need no helper) can read; it is checked as well as the process, not
//!   instead of it.
//! - The helper checks that the pipe's server is the session's process
//!   (`GetNamedPipeServerProcessId`), so a pipe of the same name served by
//!   another program is refused.
//! - The helper serves one connection, of one session, and exits when it
//!   closes: no other session or daemon reaches it.
//!
//! Out of reach of this design: an elevated program (it has the rights
//! already); a program that replaced Prime Agent's executable (UAC shows
//! which program asks); and the commands themselves, which run with full
//! administrator rights by the user's consent.

use std::collections::HashMap;
use std::ffi::{c_void, OsStr};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, watch, Notify};

use super::super::job::{self, lock, GroupHold, Job};
use super::super::winjob::JobObject;
use super::super::Session;
use super::protocol::{
    check_hello, check_server, output_bytes, read_frame, write_frame, Message, OUTPUT_CHUNK,
    PROTOCOL,
};
use super::{account, report, Elevated, DECLINED, NOT_RUN, UNAVAILABLE};
use crate::kernel::elevation_runs::{ConsentRoute, ElevationOutcome};
use crate::platform;

/// How long the helper is kept with no elevated command running.
const IDLE: Duration = Duration::from_secs(600);
/// How long a consented helper has to connect, and then to say hello.
const CONNECT_WAIT: Duration = Duration::from_secs(60);
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How long a cancelled command has to be reported ended by the helper.
const CANCEL_WAIT: Duration = Duration::from_secs(10);
/// How long the helper still reads a command's output after it exits: a
/// process holding the pipe must not hold the result open.
const EXIT_PIPE_GRACE: Duration = Duration::from_millis(500);
/// The pipe's access list: `SYSTEM` and elevated administrators, and only
/// the right to read the list for its owner (no implicit `WRITE_DAC`).
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;RC;;;OW)";

/// `shellapi.h`: keep the started process's handle in `hProcess`.
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
/// `shellapi.h`: no error dialogs of the shell's own.
const SEE_MASK_FLAG_NO_UI: u32 = 0x0000_0400;
/// `shellapi.h`: finish the launch before returning.
const SEE_MASK_NOASYNC: u32 = 0x0000_0100;
/// `winuser.h` `SW_HIDE`: the helper has no window.
const SW_HIDE: i32 = 0;
/// `winerror.h` `ERROR_CANCELLED`: the user declined the consent prompt.
const ERROR_CANCELLED: i32 = 1223;
/// `sddl.h` `SDDL_REVISION_1`.
const SDDL_REVISION_1: u32 = 1;

type Handle = *mut c_void;

/// `SHELLEXECUTEINFOW` (its `hIcon`/`hMonitor` union is one handle).
#[repr(C)]
#[allow(non_snake_case)] // The Win32 fields keep their documented names.
struct ShellExecuteInfo {
    cbSize: u32,
    fMask: u32,
    hwnd: Handle,
    lpVerb: *const u16,
    lpFile: *const u16,
    lpParameters: *const u16,
    lpDirectory: *const u16,
    nShow: i32,
    hInstApp: Handle,
    lpIDList: *mut c_void,
    lpClass: *const u16,
    hkeyClass: Handle,
    dwHotKey: u32,
    hIconOrMonitor: Handle,
    hProcess: Handle,
}

// The natural layout is `shellapi.h`'s on 64-bit Windows (112 bytes); 32-bit
// Windows packs the structure to 1 byte, which this declaration does not.
const _: () = assert!(
    std::mem::size_of::<ShellExecuteInfo>() == 112,
    "SHELLEXECUTEINFOW is declared for 64-bit Windows"
);

/// `SECURITY_ATTRIBUTES`.
#[repr(C)]
#[allow(non_snake_case)] // The Win32 fields keep their documented names.
struct SecurityAttributes {
    nLength: u32,
    lpSecurityDescriptor: *mut c_void,
    bInheritHandle: i32,
}

#[allow(non_snake_case)] // The Win32 functions keep their documented names.
#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
}

#[allow(non_snake_case)] // The Win32 functions keep their documented names.
#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: Handle) -> i32;
    fn GetProcessId(process: Handle) -> u32;
    fn GetNamedPipeClientProcessId(pipe: Handle, process_id: *mut u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: Handle, process_id: *mut u32) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

#[allow(non_snake_case)] // The Win32 functions keep their documented names.
#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

/// What a session holds of its helper: none until its first elevated
/// command, then the live one (or the one that ended, until the next).
pub(crate) type HelperSlot = tokio::sync::Mutex<Option<Arc<Helper>>>;

/// A connected helper, as the session's commands use it.
pub(crate) struct Helper {
    /// Messages to the helper; closed once the connection has ended.
    outgoing: mpsc::UnboundedSender<Message>,
    /// The runs waiting for the helper's messages about them.
    runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>>,
    /// Told when a run ends, so the idle clock starts.
    settled: Arc<Notify>,
    next: AtomicU64,
}

/// Why no helper could serve a command.
enum NoHelper {
    Declined,
    Unavailable(String),
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Start `program` with `parameters`, elevated through UAC's prompt, with no
/// window. `Ok(None)` when the user declined; else the started process's id.
fn launch(program: &OsStr, parameters: &str, directory: &OsStr) -> std::io::Result<Option<u32>> {
    let verb = wide(OsStr::new("runas"));
    let file = wide(program);
    let parameters = wide(OsStr::new(parameters));
    let directory = wide(directory);
    let mut info = ShellExecuteInfo {
        cbSize: u32::try_from(std::mem::size_of::<ShellExecuteInfo>())
            .map_err(std::io::Error::other)?,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: std::ptr::null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        lpDirectory: directory.as_ptr(),
        nShow: SW_HIDE,
        hInstApp: std::ptr::null_mut(),
        lpIDList: std::ptr::null_mut(),
        lpClass: std::ptr::null(),
        hkeyClass: std::ptr::null_mut(),
        dwHotKey: 0,
        hIconOrMonitor: std::ptr::null_mut(),
        hProcess: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a live, correctly laid out structure whose strings
    // are NUL-terminated and outlive the call.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(ERROR_CANCELLED) => Ok(None),
            Some(_) | None => Err(error),
        };
    }
    if info.hProcess.is_null() {
        return Err(std::io::Error::other(
            "Windows started the elevated helper without a process handle",
        ));
    }
    // SAFETY: `hProcess` is the started process's handle, which
    // `SEE_MASK_NOCLOSEPROCESS` hands to the caller; it is closed once, here.
    let pid = unsafe {
        let pid = GetProcessId(info.hProcess);
        CloseHandle(info.hProcess);
        pid
    };
    if pid == 0 {
        return Err(std::io::Error::other(
            "the elevated helper's process id could not be read",
        ));
    }
    Ok(Some(pid))
}

/// The process on the other end of `pipe`: its client (`client`) or its
/// server.
fn pipe_peer(pipe: &impl AsRawHandle, client: bool) -> std::io::Result<u32> {
    let mut pid = 0;
    // SAFETY: the pipe's handle is open, and `pid` a live ULONG.
    let asked = unsafe {
        if client {
            GetNamedPipeClientProcessId(pipe.as_raw_handle(), &raw mut pid)
        } else {
            GetNamedPipeServerProcessId(pipe.as_raw_handle(), &raw mut pid)
        }
    };
    if asked == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid)
}

/// The first instance of a new pipe named `name`, with [`PIPE_SDDL`].
fn create_pipe(name: &str) -> std::io::Result<NamedPipeServer> {
    let text = wide(OsStr::new(PIPE_SDDL));
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string and live out-pointers.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut attributes = SecurityAttributes {
        nLength: u32::try_from(std::mem::size_of::<SecurityAttributes>())
            .map_err(std::io::Error::other)?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` is a live `SECURITY_ATTRIBUTES` whose descriptor
    // outlives the call.
    let created = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    // SAFETY: the descriptor was allocated by the conversion and is freed
    // once, here, after the pipe copied it.
    unsafe { LocalFree(descriptor) };
    created
}

impl Helper {
    /// Ask the user to approve a helper for `session`, start it, and
    /// connect it: the UAC prompt is on the user's screen while this waits.
    async fn start(session: &Arc<Session>) -> Result<Self, NoHelper> {
        let unavailable = |what: &str, error: &dyn std::fmt::Display| {
            NoHelper::Unavailable(format!("{what}: {error}"))
        };
        let name = format!(
            r"\\.\pipe\prime-agent-elevation-{}",
            uuid::Uuid::new_v4().simple()
        );
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut pipe =
            create_pipe(&name).map_err(|error| unavailable("the helper's pipe", &error))?;
        let program = std::env::current_exe()
            .map_err(|error| unavailable("this program could not be found", &error))?;
        // Generated here: none of them holds a blank or a quote.
        let parameters = format!("--elevation-helper {name} {} {token}", std::process::id());
        let directory = session.cwd.clone();
        let launched = tokio::task::spawn_blocking(move || {
            launch(program.as_os_str(), &parameters, directory.as_os_str())
        })
        .await
        .map_err(|error| unavailable("the UAC launch", &error))?
        .map_err(|error| unavailable("the UAC launch", &error))?
        .ok_or(NoHelper::Declined)?;
        tokio::time::timeout(CONNECT_WAIT, pipe.connect())
            .await
            .map_err(|error| unavailable("the helper did not connect", &error))?
            .map_err(|error| unavailable("the helper did not connect", &error))?;
        let client = pipe_peer(&pipe, /*client*/ true)
            .map_err(|error| unavailable("the helper's process", &error))?;
        let hello = tokio::time::timeout(HELLO_WAIT, read_frame(&mut pipe))
            .await
            .map_err(|error| unavailable("the helper did not say hello", &error))?
            .map_err(|error| unavailable("the helper's hello", &error))?
            .ok_or_else(|| NoHelper::Unavailable("the helper closed the pipe".to_string()))?;
        check_hello(&hello, &token, launched, client)
            .map_err(|refusal| unavailable("the helper was refused", &refusal))?;
        write_frame(&mut pipe, &Message::Welcome { protocol: PROTOCOL })
            .await
            .map_err(|error| unavailable("the helper's pipe", &error))?;
        let (outgoing, to_send) = mpsc::unbounded_channel();
        let helper = Helper {
            outgoing,
            runs: Arc::new(Mutex::new(HashMap::new())),
            settled: Arc::new(Notify::new()),
            next: AtomicU64::new(1),
        };
        session.runtime.spawn(serve(
            pipe,
            to_send,
            Arc::clone(&helper.runs),
            Arc::clone(&helper.settled),
            session.killed.clone(),
        ));
        Ok(helper)
    }

    /// Have the helper run `script` (see [`Message::Run`]): the run's id,
    /// and the helper's messages about it.
    fn run(
        &self,
        script: String,
        shell: Option<String>,
        cwd: String,
        env: Vec<(String, String)>,
    ) -> (u64, mpsc::UnboundedReceiver<Message>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (events, received) = mpsc::unbounded_channel();
        lock(&self.runs).insert(id, events);
        // A closed connection drops the run's sender, which its wait hears.
        if self
            .outgoing
            .send(Message::Run {
                id,
                script,
                shell,
                cwd,
                env,
            })
            .is_err()
        {
            lock(&self.runs).remove(&id);
        }
        (id, received)
    }
}

/// The session's helper: the live one, or a new one the user approves now.
async fn helper(session: &Arc<Session>) -> Result<Arc<Helper>, NoHelper> {
    let mut slot = session.elevation.lock().await;
    if let Some(live) = slot.as_ref().filter(|helper| !helper.outgoing.is_closed()) {
        return Ok(Arc::clone(live));
    }
    let started = Arc::new(Helper::start(session).await?);
    *slot = Some(Arc::clone(&started));
    Ok(started)
}

/// Carry one helper connection until it ends: send what the session's
/// commands send, hand each message of the helper to its run, and close the
/// pipe (which ends the helper) when the kernel is killed, the helper
/// misbehaves, or no command has run for [`IDLE`].
async fn serve(
    pipe: NamedPipeServer,
    mut to_send: mpsc::UnboundedReceiver<Message>,
    runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>>,
    settled: Arc<Notify>,
    killed: crate::tools::tool_definition::AbortSignal,
) {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    // Reading runs on its own: a frame read half-way is never abandoned.
    let reading = {
        let (runs, settled) = (Arc::clone(&runs), Arc::clone(&settled));
        tokio::spawn(async move {
            while let Ok(Some(message)) = read_frame(&mut reader).await {
                let (id, last) = match &message {
                    Message::Started { id } | Message::Output { id, .. } => (*id, false),
                    Message::Exit { id, .. } | Message::Failed { id, .. } => (*id, true),
                    // The helper sends nothing else: the connection is over.
                    Message::Hello { .. }
                    | Message::Welcome { .. }
                    | Message::Run { .. }
                    | Message::Cancel { .. } => return,
                };
                let mut table = lock(&runs);
                // A message about a run nobody waits for any more is dropped.
                if let Some(run) = table.get(&id) {
                    let _ = run.send(message);
                }
                if last {
                    table.remove(&id);
                    drop(table);
                    settled.notify_one();
                }
            }
        })
    };
    tokio::pin!(reading);
    loop {
        let idle = lock(&runs).is_empty();
        tokio::select! {
            () = killed.cancelled() => break,
            _ = &mut reading => break,
            () = tokio::time::sleep(IDLE), if idle => break,
            () = settled.notified() => {}
            message = to_send.recv() => match message {
                Some(message) => {
                    if write_frame(&mut writer, &message).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    // Closing ends the helper; every run still waiting hears its sender go.
    to_send.close();
    lock(&runs).clear();
}

/// See [`super::spawn`].
pub(super) fn spawn(session: &Arc<Session>, command: &str) -> anyhow::Result<Arc<Job>> {
    let settings = &session.host.settings;
    let script = match &settings.command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };
    let shell = match settings.engine {
        platform::KernelShell::BuiltIn => None,
        platform::KernelShell::System => Some(
            platform::resolve_kernel_bash_shell(settings.shell.as_deref())?.ok_or_else(|| {
                anyhow::anyhow!(
                    "no shell program was found (install Git Bash in its default location, or drop \"shell\": \"system\" from settings to use the built-in shell)"
                )
            })?,
        ),
    };
    // The session's environment as its commands get it, later names over
    // earlier ones: the helper's own is not the session's.
    let mut env: Vec<(String, String)> = pa_types::session_scope::vars();
    let marker = shell
        .is_none()
        .then(|| ("PRIME_AGENT_SHELL".to_string(), "builtin".to_string()));
    for (name, value) in job::child_env(&settings.env).into_iter().chain(marker) {
        env.retain(|(existing, _)| *existing != name);
        env.push((name, value));
    }
    let (stop, stopped) = watch::channel(false);
    let mut job = Job::new(
        session,
        command,
        super::super::engine::job_number(),
        uuid::Uuid::new_v4().simple().to_string(),
        /*lasting*/ None,
    );
    job.elevated = Some(Elevated::new(Box::new(move || {
        // A supervisor that has ended hears nothing, and needs nothing.
        let _ = stop.send(true);
    })));
    let fence = format!(
        "\x1eprime-agent-complete:{}{}\x1f",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
    .into_bytes();
    let (output, output_end) = std::io::pipe().context("could not open the output pipe")?;
    let job = Arc::new(job);
    job::read_output(&job, output, fence.clone()).context("could not start the output reader")?;
    if job::enlist(session, &job, /*may_queue*/ false).is_none() {
        bail!("the kernel was shut down");
    }
    let run = (
        script,
        shell,
        session.cwd.to_string_lossy().into_owned(),
        env,
    );
    session.runtime.spawn(supervise(
        Arc::clone(session),
        GroupHold(Arc::clone(&job)),
        stopped,
        output_end,
        fence,
        run,
    ));
    Ok(job)
}

/// Watch one elevated command to its end: get the session's helper (the
/// UAC prompt, when there is none), have it run the command, relay its
/// output, and settle the job.
async fn supervise(
    session: Arc<Session>,
    hold: GroupHold,
    mut stopped: watch::Receiver<bool>,
    mut output_end: std::io::PipeWriter,
    fence: Vec<u8>,
    (script, shell, cwd, env): (String, Option<String>, String, Vec<(String, String)>),
) {
    let job = Arc::clone(&hold.0);
    let mut stop = std::pin::pin!(async move {
        // A dropped sender (the job is gone) stops the command too.
        let _ = stopped.wait_for(|stopped| *stopped).await;
    });
    let obtained = tokio::select! {
        () = &mut stop => None,
        obtained = helper(&session) => Some(obtained),
    };
    // The helper's own messages go where the command's output goes; a closed
    // output has no reader left to tell.
    // Raw output, or (`Err`) one line of the helper's own.
    let mut write = |output: Result<&[u8], String>| {
        let _ = match output {
            Ok(bytes) => output_end.write_all(bytes),
            Err(line) => output_end.write_all(format!("{line}\n").as_bytes()),
        };
    };
    let exit_code = match obtained {
        // Stopped while the user was asked: nothing ran.
        None => pa_shell::STOPPED_EXIT_CODE,
        Some(Err(NoHelper::Declined)) => {
            report(&session, ElevationOutcome::Declined, ConsentRoute::Uac);
            write(Err(format!(
                "{DECLINED} the Windows prompt (UAC) to run this command as {} was declined, so it did not run. Ask the user before trying again.",
                account()
            )));
            NOT_RUN
        }
        Some(Err(NoHelper::Unavailable(why))) => {
            report(&session, ElevationOutcome::Unavailable, ConsentRoute::Uac);
            write(Err(format!(
                "{UNAVAILABLE} the elevated helper could not start ({why}), so the command did not run. Give the user the exact command to run in a terminal opened with \"Run as administrator\"."
            )));
            NOT_RUN
        }
        Some(Ok(helper)) => {
            report(&session, ElevationOutcome::Approved, ConsentRoute::Uac);
            let (id, mut events) = helper.run(script, shell, cwd, env);
            let mut cancelled_at = None;
            loop {
                let deadline = cancelled_at;
                let give_up = async move {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = &mut stop, if cancelled_at.is_none() => {
                        // The helper may never have heard of a closed link.
                        let _ = helper.outgoing.send(Message::Cancel { id });
                        cancelled_at = Some(tokio::time::Instant::now() + CANCEL_WAIT);
                    }
                    () = give_up => {
                        write(Err("the elevated helper did not confirm the command's end".to_string()));
                        break pa_shell::STOPPED_EXIT_CODE;
                    }
                    event = events.recv() => match event {
                        Some(Message::Output { data, .. }) => match output_bytes(&data) {
                            Ok(bytes) => write(Ok(bytes.as_slice())),
                            Err(error) => write(Err(format!("the elevated helper sent unreadable output: {error}"))),
                        },
                        Some(Message::Exit { code, .. }) => break code,
                        Some(Message::Failed { error, .. }) => {
                            write(Err(format!("the elevated helper could not run the command: {error}")));
                            break NOT_RUN;
                        }
                        Some(
                            Message::Started { .. }
                            | Message::Hello { .. }
                            | Message::Welcome { .. }
                            | Message::Run { .. }
                            | Message::Cancel { .. },
                        ) => {}
                        None => {
                            write(Err("the elevated helper ended before the command did".to_string()));
                            break -1;
                        }
                    },
                }
            }
        }
    };
    // The fence follows the last output in the pipe, so the result is
    // everything before it however far the reader has got.
    let fenced = output_end.write_all(&fence).is_ok();
    drop(output_end);
    if fenced {
        job.until(|job| lock(&job.capture).settled).await;
    } else {
        lock(&job.capture).settle();
    }
    let output = lock(&job.capture).fenced.clone();
    job::settle(&session, &job, exit_code, output);
    job::retire(&session, &job);
}

/// What runs a command in the helper: a shell program, or a script on the
/// built-in shell.
enum Started {
    Program(Box<tokio::process::Child>),
    Script(pa_shell::Running),
}

/// One command the helper runs.
struct Run {
    container: JobObject,
    stop: pa_shell::Stop,
    finished: AtomicBool,
}

impl pa_shell::Children for Run {
    fn spawn(&self, mut command: std::process::Command) -> std::io::Result<tokio::process::Child> {
        platform::set_new_process_group(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        // Fail closed: a program outside the job could outlive the command.
        if !child
            .raw_handle()
            .is_some_and(|process| self.container.assign(process))
        {
            let _ = child.start_kill();
            return Err(std::io::Error::other(
                "Windows job containment could not be established",
            ));
        }
        Ok(child)
    }
}

/// Start run `id` in the helper, reporting on `events` (see
/// [`Message::Run`]).
fn start_run(
    id: u64,
    (script, shell, cwd, env): (String, Option<String>, String, Vec<(String, String)>),
    events: &mpsc::UnboundedSender<Message>,
) -> std::io::Result<Arc<Run>> {
    let container = JobObject::create()
        .ok_or_else(|| std::io::Error::other("Windows job containment could not be established"))?;
    let run = Arc::new(Run {
        container,
        stop: pa_shell::Stop::default(),
        finished: AtomicBool::new(false),
    });
    let (mut output, output_end) = std::io::pipe()?;
    let reader = {
        let events = events.clone();
        std::thread::Builder::new()
            .name("elevation-output".to_string())
            .spawn(move || {
                let mut chunk = vec![0u8; OUTPUT_CHUNK];
                loop {
                    match output.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            if events.send(Message::output(id, &chunk[..read])).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })?
    };
    let started = match shell {
        Some(shell) => {
            let mut command = std::process::Command::new(shell);
            command
                .arg("-c")
                .arg(script)
                .current_dir(&cwd)
                .env_clear()
                .envs(env)
                .stdin(std::process::Stdio::null())
                .stdout(output_end.try_clone()?)
                .stderr(output_end);
            Started::Program(Box::new(pa_shell::Children::spawn(run.as_ref(), command)?))
        }
        None => Started::Script(pa_shell::start(
            pa_shell::Script {
                source: script,
                // A number that names no process (`$$`).
                id: 0x2000_0000 + u32::try_from(id % 0x1000_0000).unwrap_or(0),
                cwd: cwd.into(),
                env,
                output: output_end,
            },
            run.stop.clone(),
            Arc::clone(&run) as Arc<dyn pa_shell::Children>,
        )?),
    };
    let _ = events.send(Message::Started { id });
    let (watched, events) = (Arc::clone(&run), events.clone());
    tokio::spawn(async move {
        let code = match started {
            Started::Program(mut child) => child
                .wait()
                .await
                .ok()
                .as_ref()
                .and_then(platform::normalized_exit_code)
                .unwrap_or(-1),
            Started::Script(mut running) => {
                let ended = running.ended().await;
                running.gone().await;
                ended.exit_code
            }
        };
        // Output still on its way is sent before the end; a process holding
        // the pipe only delays it so long.
        let joined = tokio::task::spawn_blocking(move || reader.join());
        let _ = tokio::time::timeout(EXIT_PIPE_GRACE, joined).await;
        let code = if watched.stop.requested() {
            pa_shell::STOPPED_EXIT_CODE
        } else {
            code
        };
        watched.container.terminate();
        watched.finished.store(true, Ordering::SeqCst);
        let _ = events.send(Message::Exit { id, code });
    });
    Ok(run)
}

/// The helper: serve the session that started it until its pipe closes.
pub(super) fn run_helper(args: &[String]) -> i32 {
    let [pipe, session, token] = args else {
        eprintln!(
            "usage: --elevation-helper <pipe> <session-pid> <token> (started by Prime Agent)"
        );
        return 2;
    };
    let Ok(session) = session.parse::<u32>() else {
        eprintln!("--elevation-helper: the session's process id is not a number");
        return 2;
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("--elevation-helper: {error}");
            return 1;
        }
    };
    match runtime.block_on(serve_session(pipe, session, token)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("--elevation-helper: {error}");
            1
        }
    }
}

/// Connect to the session's pipe, prove who this is, and run what it sends.
async fn serve_session(pipe: &str, session: u32, token: &str) -> std::io::Result<()> {
    let client = ClientOptions::new().open(pipe)?;
    check_server(pipe_peer(&client, /*client*/ false)?, session)
        .map_err(|refusal| std::io::Error::other(refusal.to_string()))?;
    let (mut reader, mut writer) = tokio::io::split(client);
    write_frame(
        &mut writer,
        &Message::Hello {
            protocol: PROTOCOL,
            token: token.to_string(),
            pid: std::process::id(),
        },
    )
    .await?;
    match read_frame(&mut reader).await? {
        Some(Message::Welcome { protocol: PROTOCOL }) => {}
        Some(_) | None => return Err(std::io::Error::other("the session did not welcome us")),
    }
    let (events, mut outgoing) = mpsc::unbounded_channel::<Message>();
    let writing = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });
    let mut runs: HashMap<u64, Arc<Run>> = HashMap::new();
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(Message::Run {
                id,
                script,
                shell,
                cwd,
                env,
            })) => match start_run(id, (script, shell, cwd, env), &events) {
                Ok(run) => {
                    runs.insert(id, run);
                }
                Err(error) => {
                    let _ = events.send(Message::Failed {
                        id,
                        error: error.to_string(),
                    });
                }
            },
            Ok(Some(Message::Cancel { id })) => {
                if let Some(run) = runs.get(&id) {
                    run.stop.request();
                    run.container.terminate();
                }
            }
            // The session sends nothing else; a closed pipe is the end.
            Ok(
                Some(
                    Message::Hello { .. }
                    | Message::Welcome { .. }
                    | Message::Started { .. }
                    | Message::Output { .. }
                    | Message::Exit { .. }
                    | Message::Failed { .. },
                )
                | None,
            )
            | Err(_) => break,
        }
        runs.retain(|_, run| !run.finished.load(Ordering::SeqCst));
    }
    // Every command ends with the session that asked for it.
    for run in runs.values() {
        run.stop.request();
        run.container.terminate();
    }
    drop(events);
    writing.abort();
    Ok(())
}
