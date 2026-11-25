//! A detector for console windows that the test's own process tree shows on
//! the desktop.
//!
//! The test process joins a fresh job object, so every process it starts
//! afterwards is in the job too, however deep and however detached. That
//! includes the TUI, the daemon the TUI spawns, the session workers, and
//! their git, shell and Python children. The job's completion port reports
//! each new pid. An out-of-context `WinEvent` hook sees every window shown on
//! the desktop. A `ConsoleWindowClass` window whose owner pid is in the job
//! is a console window the product opened. `conhost` reports the console's
//! client process as the window owner. Hand-declared Win32 externs, the
//! repo's no-windows-sys policy (see `pa_types::platform`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

type Handle = *mut c_void;
type Hwnd = *mut c_void;

/// `winuser.h` `EVENT_OBJECT_SHOW`.
const EVENT_OBJECT_SHOW: u32 = 0x8002;
/// `winuser.h` `WINEVENT_OUTOFCONTEXT`.
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
/// `winuser.h` `OBJID_WINDOW`.
const OBJID_WINDOW: i32 = 0;
/// `winuser.h` `WM_QUIT`.
const WM_QUIT: u32 = 0x0012;
/// `winuser.h` `PM_NOREMOVE`.
const PM_NOREMOVE: u32 = 0x0000;
/// `winnt.h` `JobObjectAssociateCompletionPortInformation`.
const JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION: i32 = 7;
/// `winnt.h` `JOB_OBJECT_MSG_NEW_PROCESS`.
const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
/// `winnt.h` `PROCESS_QUERY_LIMITED_INFORMATION`.
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    private: u32,
}

#[repr(C)]
struct JobObjectAssociateCompletionPort {
    completion_key: *mut c_void,
    completion_port: Handle,
}

type WinEventProc = unsafe extern "system" fn(Handle, u32, Hwnd, i32, i32, u32, u32);

extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentThreadId() -> u32;
    fn CreateIoCompletionPort(file: Handle, port: Handle, key: usize, threads: u32) -> Handle;
    fn GetQueuedCompletionStatus(
        port: Handle,
        bytes: *mut u32,
        key: *mut usize,
        overlapped: *mut *mut c_void,
        millis: u32,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        name: *mut u16,
        len: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn SetWinEventHook(
        event_min: u32,
        event_max: u32,
        module: Handle,
        callback: WinEventProc,
        pid: u32,
        thread: u32,
        flags: u32,
    ) -> Handle;
    fn UnhookWinEvent(hook: Handle) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostThreadMessageW(thread: u32, msg: u32, w_param: usize, l_param: isize) -> i32;
    fn GetClassNameW(hwnd: Hwnd, name: *mut u16, len: i32) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, len: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
}

/// One console window shown on the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownConsole {
    /// The window's owner pid: the console's client process.
    pub pid: u32,
    /// The window title (a fresh console shows its program's path).
    pub title: String,
}

/// Every console window shown while a hook runs. The hook callback has no
/// context pointer, so the record is process-wide.
static SHOWN: Mutex<Vec<ShownConsole>> = Mutex::new(Vec::new());

fn utf16(buffer: &[u16], len: i32) -> String {
    String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or_default()])
}

unsafe extern "system" fn on_show(
    _hook: Handle,
    _event: u32,
    hwnd: Hwnd,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW || child != 0 || hwnd.is_null() {
        return;
    }
    let mut class = [0u16; 64];
    // SAFETY: the buffers are live locals and the lengths match them.
    let (class, title, pid) = unsafe {
        let class_len = GetClassNameW(hwnd, class.as_mut_ptr(), 64);
        let mut title = [0u16; 512];
        let title_len = GetWindowTextW(hwnd, title.as_mut_ptr(), 512);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &raw mut pid);
        (utf16(&class, class_len), utf16(&title, title_len), pid)
    };
    if class == "ConsoleWindowClass" {
        SHOWN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ShownConsole { pid, title });
    }
}

/// A raw handle a watcher thread owns.
struct SendHandle(Handle);
// SAFETY: a completion port handle is usable from any thread.
unsafe impl Send for SendHandle {}

/// The running detector: the job's pid feed and the desktop's show events.
pub struct ConsoleWatch {
    pids: Arc<Mutex<BTreeMap<u32, String>>>,
    stop: Arc<AtomicBool>,
    hook_thread: u32,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl ConsoleWatch {
    /// Put this process into a fresh job, then start the pid feed and the
    /// window hook.
    pub fn start() -> ConsoleWatch {
        // SAFETY: plain handle creation; each failure panics before use.
        let port = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null(), "create the watch job");
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
            assert!(!port.is_null(), "create the job completion port");
            let association = JobObjectAssociateCompletionPort {
                completion_key: job,
                completion_port: port,
            };
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION,
                    (&raw const association).cast(),
                    u32::try_from(std::mem::size_of::<JobObjectAssociateCompletionPort>())
                        .expect("size"),
                ),
                0,
                "associate the job's completion port: {}",
                std::io::Error::last_os_error()
            );
            assert_ne!(
                AssignProcessToJobObject(job, GetCurrentProcess()),
                0,
                "put the test process into the watch job: {}",
                std::io::Error::last_os_error()
            );
            SendHandle(port)
        };
        let pids = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let feed = {
            let pids = Arc::clone(&pids);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let port = port;
                while !stop.load(Ordering::SeqCst) {
                    let mut message = 0u32;
                    let mut key = 0usize;
                    let mut overlapped = std::ptr::null_mut();
                    // SAFETY: out-pointers are live locals.
                    let got = unsafe {
                        GetQueuedCompletionStatus(
                            port.0,
                            &raw mut message,
                            &raw mut key,
                            &raw mut overlapped,
                            100,
                        )
                    };
                    if got != 0 && message == JOB_OBJECT_MSG_NEW_PROCESS {
                        // The new process's pid rides in the overlapped slot.
                        let pid = u32::try_from(overlapped as usize).unwrap_or_default();
                        pids.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(pid, image_name(pid));
                    }
                }
            })
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let hook = std::thread::spawn(move || {
            // SAFETY: the message queue is this thread's own; `msg` is a live
            // local and the hook is removed before the thread exits.
            unsafe {
                let mut msg = std::mem::zeroed::<Msg>();
                PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                let hook = SetWinEventHook(
                    EVENT_OBJECT_SHOW,
                    EVENT_OBJECT_SHOW,
                    std::ptr::null_mut(),
                    on_show,
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );
                assert!(!hook.is_null(), "install the window-show hook");
                ready_tx
                    .send(GetCurrentThreadId())
                    .expect("hand back the hook thread id");
                while GetMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    DispatchMessageW(&raw const msg);
                }
                UnhookWinEvent(hook);
            }
        });
        let hook_thread = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the window-show hook started");
        ConsoleWatch {
            pids,
            stop,
            hook_thread,
            threads: vec![feed, hook],
        }
    }

    /// Every pid the job has seen so far, with its image path when it was
    /// still readable.
    pub fn pids(&self) -> BTreeMap<u32, String> {
        self.pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The console windows shown so far by processes in the job.
    pub fn shown_by_tree(&self) -> Vec<ShownConsole> {
        let pids: BTreeSet<u32> = self.pids().into_keys().collect();
        shown()
            .into_iter()
            .filter(|window| pids.contains(&window.pid))
            .collect()
    }
}

/// Every console window shown on the desktop since the hook started.
pub fn shown() -> Vec<ShownConsole> {
    SHOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

impl Drop for ConsoleWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // SAFETY: a thread message to the hook thread's own queue.
        unsafe {
            PostThreadMessageW(self.hook_thread, WM_QUIT, 0, 0);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// The process's image path, empty once it is gone.
fn image_name(pid: u32) -> String {
    // SAFETY: the handle is closed before return; the buffer is a live local.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let mut buffer = [0u16; 1024];
        let mut len = 1024u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &raw mut len);
        CloseHandle(process);
        if ok == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }
}
