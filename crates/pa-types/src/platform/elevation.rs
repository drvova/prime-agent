//! Whether this process runs with the machine's highest rights: an elevated
//! administrator token on Windows (UAC), the root user on Unix. A daemon's
//! sessions all run in its process, so this is a fact about the daemon, read
//! once: the model is told it, the TUI shows it, and telemetry counts it.
//!
//! On Windows, [`run_elevated`] starts a program through UAC's consent prompt
//! (`ShellExecuteExW` with the `runas` verb): `prime-agent --elevated` starts
//! itself that way. Pinned Win32 constants and hand-declared externs, with no
//! windows-sys dependency (the policy of this module's siblings).

use serde::{Deserialize, Serialize};

/// A process's rights relative to the user's own account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Elevation {
    /// The user's own rights: administrator or root rights need a consent
    /// step (UAC, `sudo`).
    Standard,
    /// Administrator (Windows, an elevated token) or root (Unix, effective
    /// uid 0): every command has those rights without asking.
    Elevated,
}

impl Elevation {
    /// The elevation of a Unix process whose effective user id is `euid`.
    #[must_use]
    pub fn of_effective_uid(euid: u32) -> Self {
        if euid == 0 {
            Elevation::Elevated
        } else {
            Elevation::Standard
        }
    }
}

/// How a program on this platform gets rights its user's account lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElevationRoute {
    /// Windows: an elevated program the user consents to through UAC.
    Uac,
    /// macOS: `sudo`, or the system's password dialog through `osascript`.
    MacosSudo,
    /// Linux and other Unix: `sudo`, or polkit's `pkexec` dialog.
    UnixSudo,
}

impl ElevationRoute {
    /// This build's platform.
    #[cfg(windows)]
    pub const CURRENT: Self = ElevationRoute::Uac;
    /// This build's platform.
    #[cfg(target_os = "macos")]
    pub const CURRENT: Self = ElevationRoute::MacosSudo;
    /// This build's platform.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub const CURRENT: Self = ElevationRoute::UnixSudo;

    /// The account an elevated process acts as, as the user knows it.
    #[must_use]
    pub fn elevated_account(self) -> &'static str {
        match self {
            ElevationRoute::Uac => "Administrator",
            ElevationRoute::MacosSudo | ElevationRoute::UnixSudo => "root",
        }
    }
}

/// This process's elevation, read once.
#[must_use]
pub fn process_elevation() -> Elevation {
    static ELEVATION: std::sync::OnceLock<Elevation> = std::sync::OnceLock::new();
    *ELEVATION.get_or_init(read_elevation)
}

#[cfg(unix)]
fn read_elevation() -> Elevation {
    // SAFETY: geteuid only reads this process's effective uid.
    Elevation::of_effective_uid(unsafe { libc::geteuid() })
}

#[cfg(windows)]
fn read_elevation() -> Elevation {
    // SAFETY: the pseudo-handle of the current process needs no closing.
    if windows::token_is_elevated(unsafe { windows::GetCurrentProcess() }) {
        Elevation::Elevated
    } else {
        Elevation::Standard
    }
}

#[cfg(not(any(unix, windows)))]
fn read_elevation() -> Elevation {
    Elevation::Standard
}

/// The access an elevated daemon's pipes (supervisor and workers) are created
/// with, in SDDL: a protected DACL that lets only `SYSTEM` and
/// `Administrators` (`BA`, which only an elevated token holds for granting)
/// open them, and a High mandatory label that refuses writes from any process
/// below High integrity. A daemon that is not elevated keeps Windows' default.
const ELEVATED_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;HI)";

/// The security descriptor (SDDL) a daemon of `elevation` creates its pipes
/// with; `None` keeps Windows' default access list.
// Off Windows only the tests call it: there are no pipes.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pipe_security_descriptor(elevation: Elevation) -> Option<&'static str> {
    match elevation {
        Elevation::Standard => None,
        Elevation::Elevated => Some(ELEVATED_PIPE_SDDL),
    }
}

/// Whether a client of elevation `client` may use a pipe whose serving process
/// has elevation `server`: an elevated client trusts only an elevated server,
/// since any process of the user can create a pipe of the elevated daemon's
/// name before it does.
// Off Windows only the tests call it: a Unix client checks its peer's uid.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn trusts_pipe_server(client: Elevation, server: Elevation) -> bool {
    match (client, server) {
        (Elevation::Standard, Elevation::Standard | Elevation::Elevated)
        | (Elevation::Elevated, Elevation::Elevated) => true,
        (Elevation::Elevated, Elevation::Standard) => false,
    }
}

/// How a [`run_elevated`] launch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevatedRun {
    /// The user consented; the elevated program exited with this code.
    Exited(u32),
    /// The user declined the consent prompt.
    Declined,
}

#[cfg(windows)]
pub(crate) use windows::process_elevation_of;
#[cfg(windows)]
pub use windows::run_elevated;

/// Start `program` with `args` in `directory` elevated, through UAC's consent
/// prompt, and wait for it: Windows only.
///
/// # Errors
///
/// Always: only Windows starts a program elevated on the user's consent.
#[cfg(not(windows))]
pub fn run_elevated(
    _program: &std::ffi::OsStr,
    _args: &[String],
    _directory: &std::ffi::OsStr,
) -> std::io::Result<ElevatedRun> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "only Windows starts a program elevated through a consent prompt",
    ))
}

#[cfg(windows)]
#[allow(non_snake_case)] // The Win32 functions and fields keep their documented names.
mod windows {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    use super::ElevatedRun;

    type Handle = *mut c_void;

    /// `winnt.h`: the right to read a token's information.
    const TOKEN_QUERY: u32 = 0x0008;
    /// `winnt.h` `TokenElevation` (`TOKEN_INFORMATION_CLASS`).
    const TOKEN_ELEVATION: i32 = 20;
    /// `winnt.h`: the right to ask about a process, which Windows grants on
    /// another process of the same user at any integrity level.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    /// `shellapi.h`: keep the started process's handle in `hProcess`.
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    /// `shellapi.h`: finish the launch before returning.
    const SEE_MASK_NOASYNC: u32 = 0x0000_0100;
    /// `winuser.h` `SW_SHOWNORMAL`.
    const SW_SHOWNORMAL: i32 = 1;
    /// `winbase.h` `INFINITE`.
    const INFINITE: u32 = 0xFFFF_FFFF;
    /// `winerror.h` `ERROR_CANCELLED`: the user declined the consent prompt.
    const ERROR_CANCELLED: i32 = 1223;

    /// `SHELLEXECUTEINFOW` (its `hIcon`/`hMonitor` union is one handle).
    #[repr(C)]
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

    // The natural layout is `shellapi.h`'s on 64-bit Windows (112 bytes); 32-bit Windows packs
    // the structure to 1 byte, which this declaration does not.
    const _: () = assert!(
        std::mem::size_of::<ShellExecuteInfo>() == 112,
        "SHELLEXECUTEINFOW is declared for 64-bit Windows"
    );

    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: i32,
            info: *mut c_void,
            len: u32,
            returned: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub(super) fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        // The id is a `DWORD`; declared as the crate's other declaration of
        // `OpenProcess` declares it (`platform::process`): one symbol's
        // declarations must agree.
        fn OpenProcess(access: u32, inherit: i32, process_id: usize) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
    }

    /// Whether the token of `process` (a handle with at least
    /// `PROCESS_QUERY_LIMITED_INFORMATION`) is elevated (`TOKEN_ELEVATION`). A
    /// token that cannot be read reads as not elevated.
    pub(super) fn token_is_elevated(process: Handle) -> bool {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: an open process handle and a live out-pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
            return false;
        }
        let mut elevated: u32 = 0;
        let mut returned = 0;
        // SAFETY: an open token and a live `TOKEN_ELEVATION` (one DWORD) of
        // the length passed.
        let read = unsafe {
            GetTokenInformation(
                token,
                TOKEN_ELEVATION,
                (&raw mut elevated).cast(),
                4,
                &raw mut returned,
            )
        };
        // SAFETY: the token was opened above and is closed once, here.
        unsafe { CloseHandle(token) };
        read != 0 && elevated != 0
    }

    /// The elevation of process `pid`. A process that cannot be opened, or
    /// whose token cannot be read, reads as not elevated.
    pub(crate) fn process_elevation_of(pid: u32) -> super::Elevation {
        // SAFETY: plain call; a null result is checked.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                usize::try_from(pid).unwrap_or(usize::MAX),
            )
        };
        if process.is_null() {
            return super::Elevation::Standard;
        }
        let elevated = token_is_elevated(process);
        // SAFETY: the handle was opened above and is closed once, here.
        unsafe { CloseHandle(process) };
        if elevated {
            super::Elevation::Elevated
        } else {
            super::Elevation::Standard
        }
    }

    /// Start `program` with `args` in `directory` through UAC's `runas` verb,
    /// so the user sees a consent prompt, and wait for it to exit. Windows
    /// starts the elevated program in a console of its own, from the user's
    /// environment rather than this process's.
    ///
    /// # Errors
    ///
    /// The system's error when the launch fails for any reason but the
    /// user's refusal, or when the program's exit code cannot be read.
    pub fn run_elevated(
        program: &OsStr,
        args: &[String],
        directory: &OsStr,
    ) -> std::io::Result<ElevatedRun> {
        let wide =
            |text: &OsStr| -> Vec<u16> { text.encode_wide().chain(std::iter::once(0)).collect() };
        // One command line, quoted as `CommandLineToArgvW` (and Rust's
        // `std::env::args`) reads it back: an argument with a blank or a quote
        // is quoted, a quote in it escaped, and the backslashes before a
        // quote, or before the closing one, doubled.
        let mut line = String::new();
        for arg in args {
            if !line.is_empty() {
                line.push(' ');
            }
            if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
                line.push_str(arg);
                continue;
            }
            line.push('"');
            let mut backslashes = 0;
            for character in arg.chars() {
                match character {
                    '\\' => backslashes += 1,
                    '"' => {
                        line.push_str(&"\\".repeat(backslashes * 2 + 1));
                        line.push('"');
                        backslashes = 0;
                    }
                    other => {
                        line.push_str(&"\\".repeat(backslashes));
                        line.push(other);
                        backslashes = 0;
                    }
                }
            }
            line.push_str(&"\\".repeat(backslashes * 2));
            line.push('"');
        }
        let verb = wide(OsStr::new("runas"));
        let file = wide(program);
        let parameters = wide(OsStr::new(&line));
        let directory = wide(directory);
        let mut info = ShellExecuteInfo {
            cbSize: u32::try_from(std::mem::size_of::<ShellExecuteInfo>())
                .map_err(std::io::Error::other)?,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            hwnd: std::ptr::null_mut(),
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: parameters.as_ptr(),
            lpDirectory: directory.as_ptr(),
            nShow: SW_SHOWNORMAL,
            hInstApp: std::ptr::null_mut(),
            lpIDList: std::ptr::null_mut(),
            lpClass: std::ptr::null(),
            hkeyClass: std::ptr::null_mut(),
            dwHotKey: 0,
            hIconOrMonitor: std::ptr::null_mut(),
            hProcess: std::ptr::null_mut(),
        };
        // SAFETY: `info` is a live, correctly laid out structure whose
        // strings are NUL-terminated and outlive the call.
        if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(ERROR_CANCELLED) => Ok(ElevatedRun::Declined),
                _ => Err(error),
            };
        }
        if info.hProcess.is_null() {
            return Err(std::io::Error::other(
                "Windows started the elevated program without a process handle",
            ));
        }
        let mut code = 0;
        // SAFETY: `hProcess` is the started process's handle, which
        // `SEE_MASK_NOCLOSEPROCESS` hands to the caller, and `code` a live
        // DWORD.
        let read = unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &raw mut code)
        };
        // Read before the close below can replace the thread's last error.
        let failure = (read == 0).then(std::io::Error::last_os_error);
        // SAFETY: the handle is closed once, here.
        unsafe { CloseHandle(info.hProcess) };
        match failure {
            Some(error) => Err(error),
            None => Ok(ElevatedRun::Exited(code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An elevated daemon's pipes admit only `SYSTEM` and elevated
    /// administrators at High integrity; a standard daemon's keep the default.
    #[test]
    fn only_an_elevated_daemon_restricts_its_pipes() {
        assert_eq!(
            [
                pipe_security_descriptor(Elevation::Standard),
                pipe_security_descriptor(Elevation::Elevated),
            ],
            [None, Some("D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;HI)")]
        );
    }

    /// An elevated client refuses a pipe a process that is not elevated serves
    /// (a squatter of the elevated daemon's name); a standard client trusts as
    /// before.
    #[test]
    fn an_elevated_client_trusts_only_an_elevated_server() {
        assert_eq!(
            [
                trusts_pipe_server(Elevation::Elevated, Elevation::Standard),
                trusts_pipe_server(Elevation::Elevated, Elevation::Elevated),
                trusts_pipe_server(Elevation::Standard, Elevation::Standard),
                trusts_pipe_server(Elevation::Standard, Elevation::Elevated),
            ],
            [false, true, true, true]
        );
    }

    #[test]
    fn root_is_the_only_elevated_unix_user() {
        assert_eq!(
            [0, 1, 501, 1000, u32::MAX].map(Elevation::of_effective_uid),
            [
                Elevation::Elevated,
                Elevation::Standard,
                Elevation::Standard,
                Elevation::Standard,
                Elevation::Standard
            ]
        );
    }
}
