//! Process control: signals, process groups, detached spawns. Signatures
//! that report outcomes return `bool` where callers treat "unproven"
//! conservatively (a kill that could not be proven reports false, matching
//! the TS `killOrphanProcess` contract).

use std::path::PathBuf;
use std::process::Command;

/// A program on the session's `PATH`. On Windows a bare name also resolves
/// through `.exe`, `.cmd` and `.bat`: `CreateProcess` adds only `.exe`, and
/// launchers such as `npm` and `npx` are `.cmd` files there.
pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::split_paths(&pa_types::session_scope::var_os("PATH")?)
        .flat_map(|directory| {
            suffixes
                .iter()
                .map(move |suffix| directory.join(format!("{name}{suffix}")))
        })
        .find(|candidate| candidate.is_file() && super::is_executable(candidate))
}

/// The real Unix user id, matching Node `process.getuid()` for daemon namespaces.
#[cfg(unix)]
#[must_use]
pub fn current_user_id() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

/// Termination signal for [`kill_pid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Graceful stop (SIGTERM).
    Term,
    /// Forcible stop (SIGKILL).
    Kill,
}

/// Put the spawned child into its own process group so later group-scoped
/// kills reach all of its descendants (TS: `detached: true` on POSIX).
#[cfg(unix)]
pub fn set_new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Start the spawned child in a new session with no controlling terminal
/// (`setsid`): the child cannot open `/dev/tty`, and job-control signals
/// from the parent's terminal never reach it. A new process group alone
/// is not enough: the group can still open the terminal, and a background
/// read then stops it with SIGTTIN.
#[cfg(unix)]
pub fn set_new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the forked child before exec and only calls
    // setsid(2), which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(drop)
                .map_err(std::io::Error::from)
        });
    }
}

/// Windows: a new process group with its own console that has no window
/// (`CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`). The child never shares
/// the parent's console, so the parent's console closing or a Ctrl+C there
/// never reaches it. It must not be detached from consoles altogether (Node's
/// `detached: true` mapping): a process with no console hands none to its
/// console children, and Windows opens each of them a new, visible console
/// window titled with the program's path (`git.exe`, a venv's `python.exe`).
/// Windows also ignores `CREATE_NO_WINDOW` next to that flag. A windowless
/// console is inherited by the child's whole subtree. Tree kills need none
/// of it (`taskkill /T` walks the parent-child tree).
#[cfg(windows)]
pub fn set_new_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `winbase.h`: new process group (no ctrl+c broadcast from the parent).
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    /// `winbase.h` `CREATE_NO_WINDOW` (TS `windowsHide`): a new console
    /// without a window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
pub fn set_new_process_group(_command: &mut Command) {
    // No detached-group mechanism on this platform; group-scoped kills are
    // unavailable and callers fall back to single-pid kills.
}

/// Hide the console window of a non-interactive spawn (TS `windowsHide`).
/// Only one of [`set_new_process_group`] and [`set_no_window`] may be
/// applied: creation flags replace each other.
#[cfg(windows)]
pub fn set_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `winbase.h` `CREATE_NO_WINDOW` (TS `windowsHide`).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Unix: every product console surface owns its own window behavior
/// (`windowsHide` has no meaning without windows).
#[cfg(not(windows))]
pub fn set_no_window(_command: &mut Command) {}

/// Windows: this process's own standard handles stop being inheritable, so
/// a child receives only the handles its spawn hands it. Windows gives every
/// child all of the parent's inheritable handles (Rust spawns with
/// `bInheritHandles`), and a process's standard handles are inheritable when
/// its parent passed them down: without this, a daemon this CLI starts keeps
/// a copy of the client's stdio pipes, and an ACP client (an editor) never
/// sees this CLI's stdout end when it exits. A child that is meant to share
/// a standard stream still does: Rust hands it a fresh inheritable duplicate.
/// Call it before the first spawn. Unix: descriptors are close-on-exec
/// already, and nothing is done.
pub fn withhold_standard_handles_from_children() {
    #[cfg(windows)]
    win32::withhold_standard_handles();
}

/// Raise the soft open-file limit to the hard limit and return the
/// resulting soft limit (Node raises it the same way at startup). macOS
/// refuses a soft limit above `kern.maxfilesperproc`, so the target is
/// capped there.
///
/// # Errors
///
/// The OS error of a failed `getrlimit`, `sysctlbyname` or `setrlimit`.
#[cfg(unix)]
pub fn raise_open_file_limit() -> std::io::Result<Option<u64>> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(not(target_os = "macos"))]
    let target = limit.rlim_max;
    #[cfg(target_os = "macos")]
    let target = {
        let mut per_process: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut per_process).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return Err(std::io::Error::last_os_error());
        }
        limit.rlim_max.min(per_process.unsigned_abs().into())
    };
    if limit.rlim_cur < target {
        limit.rlim_cur = target;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(Some(limit.rlim_cur))
}

/// No per-process descriptor limit to raise.
///
/// # Errors
///
/// Never: the not-unix arm has no descriptor limit to raise.
#[cfg(not(unix))]
pub fn raise_open_file_limit() -> std::io::Result<Option<u64>> {
    Ok(None)
}

/// Signal a single pid. Returns true only when the signal was delivered,
/// proving the pid was alive at signal time.
#[cfg(unix)]
#[must_use]
pub fn kill_pid(pid: i32, signal: Signal) -> bool {
    if pid <= 0 {
        return false;
    }
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    unsafe { libc::kill(pid, sig) == 0 }
}

/// Whether a process group currently exists, without delivering a signal.
/// Existence alone never establishes ownership of a durable group's old id.
///
/// # Errors
/// Returns the native probe error when group existence cannot be established.
#[cfg(unix)]
pub fn process_group_exists(pgid: i32) -> std::io::Result<bool> {
    if pgid <= 1 {
        return Ok(false);
    }
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// No process groups: none exists.
///
/// # Errors
///
/// Never: the not-unix arm has no group to probe.
#[cfg(not(unix))]
pub fn process_group_exists(_pgid: i32) -> std::io::Result<bool> {
    Ok(false)
}

/// Signal every member of process group `pgid` (`kill(-pgid, sig)`). Returns
/// true only when the signal was delivered to at least one member.
#[cfg(unix)]
#[must_use]
pub fn signal_process_group(pgid: i32, signal: Signal) -> bool {
    if pgid <= 1 {
        return false;
    }
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    unsafe { libc::kill(-pgid, sig) == 0 }
}

#[cfg(not(unix))]
#[must_use]
pub fn signal_process_group(_pgid: i32, _signal: Signal) -> bool {
    // No process groups: persistent shells are POSIX-only.
    false
}

/// Windows: `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess` on the
/// single pid (every signal terminates; the `Term`/`Kill` distinction
/// collapses there). Descendants are NOT killed: teardown paths needing
/// tree kills use [`kill_process_group_or_pid`].
#[cfg(windows)]
#[must_use]
pub fn kill_pid(pid: i32, signal: Signal) -> bool {
    if pid <= 0 {
        return false;
    }
    let _ = signal;
    win32::terminate_process(pid as u32)
}

#[cfg(not(any(unix, windows)))]
pub fn kill_pid(_pid: i32, _signal: Signal) -> bool {
    // No signaling mechanism; the kill stays unproven, the conservative
    // answer callers act on.
    false
}

/// Kill a process and all its children: the process group first (`bash()`
/// children run detached in a new group), then the bare pid as fallback.
/// Returns true when either signal was delivered (TS `killProcessTree`).
#[cfg(unix)]
#[must_use]
pub fn kill_process_group_or_pid(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) == 0 {
            return true;
        }
    }
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

/// Windows: `taskkill /F /T /PID <pid>` from the absolute System32 path -
/// a bare `taskkill` name could resolve a planted CWD executable. True
/// only when taskkill exited 0, the same proof TS's `result.status === 0`
/// requires.
#[cfg(windows)]
#[must_use]
pub fn kill_process_group_or_pid(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let system_root = pa_types::session_scope::var_os("SystemRoot")
        .unwrap_or_else(|| std::ffi::OsString::from("C:\\Windows"));
    let taskkill = std::path::Path::new(&system_root)
        .join("System32")
        .join("taskkill.exe");
    let mut command = pa_types::session_scope::std_command(&taskkill);
    command
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    set_no_window(&mut command);
    command.status().is_ok_and(|status| status.success())
}

#[cfg(not(any(unix, windows)))]
pub fn kill_process_group_or_pid(_pid: i32) -> bool {
    // No tree-kill mechanism; the kill stays unproven, the conservative
    // answer callers act on.
    false
}

/// Cheap `kill(pid, 0)` existence probe; counts zombies as existing.
#[cfg(unix)]
#[must_use]
pub fn pid_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Which side of a [`split_process`] the caller is on.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// The original process; `child` is the new one's pid.
    Parent { child: u32 },
    /// The new process: a copy of the original at the moment of the split.
    Child,
}

/// How a child process ended.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    /// It exited with this status.
    Code(i32),
    /// It was killed by this signal.
    Signal(i32),
}

/// Split this process in two (`fork`) with no `exec`: both sides continue from
/// here, the child as an exact copy in a session of its own (`setsid`), so it
/// shares no terminal session or process group with the original. Callers
/// must be single-threaded: the
/// copy has only the calling thread, so a lock another thread held (the
/// allocator's included) would stay locked in it forever. The one caller is
/// the daemon launch, first thing in `main`.
///
/// # Errors
///
/// Returns an error when the operating system refuses the new process.
#[cfg(unix)]
pub fn split_process() -> std::io::Result<Split> {
    // SAFETY: the caller is single-threaded (the documented contract; the
    // only call site is the top of `main`, before any thread exists), so the
    // child starts with every lock free and may run arbitrary code.
    match unsafe { nix::unistd::fork() }.map_err(std::io::Error::from)? {
        nix::unistd::ForkResult::Parent { child } => Ok(Split::Parent {
            child: child.as_raw().unsigned_abs(),
        }),
        nix::unistd::ForkResult::Child => {
            // A new process is never a process-group leader, the one case in
            // which `setsid` fails, so there is no failure to report here.
            let _ = nix::unistd::setsid();
            Ok(Split::Child)
        }
    }
}

/// The calling process's parent pid.
#[cfg(unix)]
#[must_use]
pub fn parent_pid() -> u32 {
    nix::unistd::getppid().as_raw().unsigned_abs()
}

/// Block until the child process `pid` ends and report how. Only a child of
/// the calling process can be waited for.
///
/// # Errors
///
/// Returns an error when `pid` is not a waitable child of this process.
#[cfg(unix)]
pub fn wait_child(pid: u32) -> std::io::Result<ChildExit> {
    use nix::sys::wait::{waitpid, WaitStatus};
    let pid = nix::unistd::Pid::from_raw(
        i32::try_from(pid).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?,
    );
    loop {
        match waitpid(pid, None).map_err(std::io::Error::from)? {
            WaitStatus::Exited(_, code) => return Ok(ChildExit::Code(code)),
            WaitStatus::Signaled(_, signal, _) => return Ok(ChildExit::Signal(signal as i32)),
            // Stops and continues are not ends (none are requested; the arms
            // keep the match total).
            WaitStatus::Stopped(..) | WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            #[cfg(any(target_os = "linux", target_os = "android"))]
            WaitStatus::PtraceEvent(..) | WaitStatus::PtraceSyscall(_) => {}
        }
    }
}

/// The kernel-held process handle (`pidfd_open`): pins the exact process
/// behind the pid, so a signal through it ([`pidfd_signal`]) reaches that
/// process even if the numeric pid is recycled afterwards.
///
/// # Errors
///
/// Returns the open failure verbatim: `ESRCH` names a process that is
/// already gone, `Unsupported` a platform with no pidfd arm.
#[cfg(all(
    unix,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn open_pidfd(pid: u32) -> std::io::Result<i32> {
    // `SYS_pidfd_open`/`SYS_pidfd_send_signal` share their numbers across
    // x86_64 and aarch64; pidfd is Linux-only (macOS libc has no
    // `SYS_pidfd_*` constants to compile against).
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd as i32)
    }
}

/// No pidfd arm on this platform or kernel: the handle is unobtainable.
///
/// # Errors
///
/// Always `Unsupported`.
#[cfg(all(
    unix,
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
pub fn open_pidfd(_pid: u32) -> std::io::Result<i32> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// No pidfd arm on this platform or kernel: the handle is unobtainable.
///
/// # Errors
///
/// Always `Unsupported`.
#[cfg(not(unix))]
pub fn open_pidfd(_pid: u32) -> std::io::Result<i32> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Signal through the kernel-held handle (`pidfd_send_signal`): the signal
/// reaches the pinned process; the handle CLOSES on drop by the caller.
#[cfg(all(
    unix,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[must_use]
pub fn pidfd_signal(fd: i32, signal: Signal) -> bool {
    let signum = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            signum,
            std::ptr::null::<u8>(),
            0,
        ) == 0
    }
}

#[cfg(all(
    unix,
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
#[must_use]
pub fn pidfd_signal(_fd: i32, _signal: Signal) -> bool {
    false
}

#[cfg(not(unix))]
#[must_use]
pub fn pidfd_signal(_fd: i32, _signal: Signal) -> bool {
    false
}

pub fn close_pidfd(fd: i32) {
    #[cfg(unix)]
    unsafe {
        libc::close(fd);
    }
    #[cfg(not(unix))]
    let _ = fd;
}

/// Resolve once the process behind `pid` has exited, parked on the kernel's
/// exit notification (Linux pidfd readability, macOS kqueue
/// `EVFILT_PROC`/`NOTE_EXIT`), with no timer. A pid that names no process
/// (already exited and reaped) resolves at once. The kernel handle pins
/// the process instance, so a pid recycled after registration is never
/// mistaken for it.
///
/// # Errors
///
/// The OS error when the watch cannot register (no pidfd, descriptor
/// exhaustion, an unsupported platform); the caller owns its fallback.
#[cfg(target_os = "linux")]
pub async fn wait_for_exit(pid: u32) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tokio::io::{unix::AsyncFd, Interest};

    let fd = match open_pidfd(pid) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
        Err(error) => return Err(error),
    };
    // SAFETY: `open_pidfd` returned a fresh descriptor this call owns; the
    // `AsyncFd` owns the `OwnedFd`, so it stays open and unchanged until drop.
    let fd =
        unsafe { AsyncFd::register_with_interest(OwnedFd::from_raw_fd(fd), Interest::READABLE) }?;
    // Every wake is confirmed by a zero-timeout poll (AsyncFd readiness
    // can be spurious).
    loop {
        let mut ready = fd.readable().await?;
        if let Ok(result) = ready.try_io(|fd| {
            let mut probe = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: polls one descriptor this call owns, without
            // blocking.
            match unsafe { libc::poll(&raw mut probe, 1, 0) } {
                1 => Ok(()),
                0 => Err(std::io::ErrorKind::WouldBlock.into()),
                _ => Err(std::io::Error::last_os_error()),
            }
        }) {
            return result;
        }
    }
}

/// macOS: kqueue `EVFILT_PROC`/`NOTE_EXIT` on a pollable kqueue.
///
/// # Errors
///
/// The OS error when the watch cannot register; the caller owns its
/// fallback.
#[cfg(target_os = "macos")]
pub async fn wait_for_exit(pid: u32) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tokio::io::{unix::AsyncFd, Interest};

    // SAFETY: `kqueue()` takes no arguments.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor this call owns (and closes on drop, on
    // every path).
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    // The block drops the `kevent` value (its `udata` raw pointer is
    // `!Send`) before the first `.await`.
    {
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: registers one change and reads no events; `change`
        // outlives the call. With no event space, an attach failure comes
        // back as -1/errno (kevent(2)), not an `EV_ERROR` event.
        if unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            // `ESRCH` means the pid names no attachable process (reaped,
            // or already past its exit ref-drain - XNU runs the drain
            // before the exit knote fires): the exit already happened.
            // Every other errno (EMFILE, ENOMEM, ...) is a watch that
            // could not register; the caller falls back.
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(()),
                _ => Err(error),
            };
        }
    }
    // SAFETY: the `AsyncFd` owns `kq`, so it stays open and unchanged until drop.
    let kq = unsafe { AsyncFd::register_with_interest(kq, Interest::READABLE) }?;
    // Only the exit knote is registered, so a drained event is the exit.
    loop {
        let mut ready = kq.readable().await?;
        if let Ok(result) = ready.try_io(|kq| {
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let mut event = libc::kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: drains one event with a zero timeout; `event` and
            // `zero` outlive the call.
            match unsafe {
                libc::kevent(
                    kq.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &raw mut event,
                    1,
                    &raw const zero,
                )
            } {
                1 => Ok(()),
                0 => Err(std::io::ErrorKind::WouldBlock.into()),
                _ => Err(std::io::Error::last_os_error()),
            }
        }) {
            return result;
        }
    }
}

/// No kernel exit watch on this platform: the caller takes its own
/// liveness-poll fallback.
///
/// # Errors
///
/// Always `Unsupported`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[expect(
    clippy::unused_async,
    reason = "one awaitable signature across the kernel-watch arms; this arm has nothing to await"
)]
pub async fn wait_for_exit(_pid: u32) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Windows: the shared handle probe (win32 has no unreaped-zombie state,
/// so existing and running are the same predicate). A query that fails
/// outright reads as gone.
#[cfg(windows)]
#[must_use]
pub fn pid_exists(pid: u32) -> bool {
    pa_types::platform::process::is_process_alive(pid).unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
pub fn pid_exists(_pid: u32) -> bool {
    false
}

/// The signal number that terminated a child, when it was signaled
/// (`ExitStatus::signal` on Unix; None elsewhere).
#[cfg(unix)]
#[must_use]
pub fn termination_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
#[must_use]
pub fn termination_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    // Windows terminations surface as exit codes, not signals.
    None
}

/// A child's exit code as a shell's `$?` reports it: the status code, or
/// 128 + the signal number when a signal killed it (TS `normalizedExitCode`).
/// Without the mapping a signal death carries no code and reads as success.
#[must_use]
pub fn normalized_exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    status
        .code()
        .or_else(|| termination_signal(status).map(|signal| 128 + signal))
}

/// The kernel32 termination surface for [`kill_pid`], hand-declared (repo
/// policy: pinned constants and externs, no windows-sys dependency).
#[cfg(windows)]
mod win32 {

    use std::ffi::c_void;

    /// `winnt.h`: the right to terminate the process.
    const PROCESS_TERMINATE: u32 = 0x0001;

    type Handle = *mut c_void;

    extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: usize) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn TerminateProcess(handle: Handle, exit_code: u32) -> i32;
        fn GetStdHandle(which: u32) -> Handle;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    }

    /// `winbase.h` `STD_INPUT_HANDLE`, `STD_OUTPUT_HANDLE`, `STD_ERROR_HANDLE`
    /// (`(DWORD)-10`, `-11`, `-12`).
    const STANDARD_HANDLES: [u32; 3] = [0xFFFF_FFF6, 0xFFFF_FFF5, 0xFFFF_FFF4];
    /// `winbase.h`: the handle is inherited by children.
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    /// `handleapi.h` `INVALID_HANDLE_VALUE` (`(HANDLE)-1`), as an address.
    const INVALID_HANDLE_VALUE: usize = usize::MAX;

    /// Clear the inherit flag on each standard handle the process has. A
    /// handle that refuses the flag is a console pseudo-handle (Windows 7),
    /// which inheritance never copies, so the outcome is not consulted.
    pub(crate) fn withhold_standard_handles() {
        for which in STANDARD_HANDLES {
            let handle = unsafe { GetStdHandle(which) };
            if handle.is_null() || handle.addr() == INVALID_HANDLE_VALUE {
                continue;
            }
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }

    /// Terminate exactly the pid; false when it could not be proven
    /// terminated (gone already, access denied, or invalid).
    pub(crate) fn terminate_process(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as usize) };
        if handle.is_null() {
            return false;
        }
        let terminated = unsafe { TerminateProcess(handle, 1) != 0 };
        unsafe { CloseHandle(handle) };
        terminated
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn pid_exists_for_self_but_not_zero() {
        assert!(pid_exists(std::process::id()));
        assert!(!pid_exists(0));
        assert!(!pid_exists(u32::MAX));
    }

    #[test]
    fn out_of_range_pids_never_prove_kills() {
        assert!(!kill_pid(-1, Signal::Kill));
        assert!(!kill_pid(0, Signal::Term));
        assert!(!kill_process_group_or_pid(-1));
    }
}

/// The kernel-watch arms' shared exit contract: a pid that names no live
/// process resolves at once, so a worker that died before its watch
/// registered still reports an exit. Both kernel-watch platforms only
/// (the other targets take the caller's fallback by design).
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod exit_wait_tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_exit_resolves_at_once_for_a_reaped_pid() {
        let mut child = pa_types::session_scope::std_command("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
        assert!(
            wait_for_exit(pid).await.is_ok(),
            "a reaped pid resolves at once"
        );
    }
}

/// Windows creation flags live behind this module's seams. A console child
/// of a process that has no console gets a new, visible console window, so
/// no product spawn may detach from the console, and every other
/// creation-flags call stays in the listed sites.
/// Point this process's standard input at the null device, so nothing in it
/// can block reading a terminal or a pipe it was started with. A daemon that
/// hosts sessions calls this once at boot: a session cell's `(read-line)`
/// then reads end-of-file, however the daemon was started.
///
/// # Errors
///
/// Returns the operating system's error when the null device cannot be
/// opened or put in place of descriptor 0.
#[cfg(unix)]
pub fn close_standard_input() -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let null = std::fs::File::open("/dev/null")?;
    nix::unistd::dup2(null.as_raw_fd(), 0)
        .map(|_| ())
        .map_err(std::io::Error::from)
}

/// Windows consoles are not redirected here: a daemon there is started
/// detached, with no console input to read.
///
/// # Errors
///
/// Never fails.
#[cfg(not(unix))]
pub fn close_standard_input() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod creation_flag_tests {
    use std::path::{Path, PathBuf};

    /// Product sources that set Windows creation flags themselves: this
    /// module, and pa-tui's `gh` probe (pa-tui links pa-types alone, and its
    /// flag is `CREATE_NO_WINDOW`).
    const CREATION_FLAG_SITES: [&str; 2] = [
        "pa-core/src/platform/process.rs",
        "pa-tui/src/export_share.rs",
    ];

    #[test]
    fn no_spawn_detaches_from_the_console() {
        // Split so this file's own needles never match.
        let detached = concat!("DETACHED", "_PROCESS");
        let creation_flags = concat!("creation_flags", "(");
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the crates directory");
        let mut pending: Vec<PathBuf> = std::fs::read_dir(crates)
            .expect("read the crates directory")
            .map(|entry| entry.expect("crate entry").path().join("src"))
            .filter(|src| src.is_dir())
            .collect();
        let mut violations = Vec::new();
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    std::fs::read_dir(&path)
                        .expect("read a source directory")
                        .map(|entry| entry.expect("source entry").path()),
                );
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let relative = path
                .strip_prefix(crates)
                .expect("a crate source path")
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path).expect("read a source file");
            for (index, line) in source.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                if code.contains(detached)
                    || (code.contains(creation_flags)
                        && !CREATION_FLAG_SITES.contains(&relative.as_str()))
                {
                    violations.push(format!("{relative}:{}: {code}", index + 1));
                }
            }
        }
        violations.sort();
        assert_eq!(violations, Vec::<String>::new());
    }
}
