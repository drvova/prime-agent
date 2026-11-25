//! Whether this process runs with the machine's highest rights: an elevated
//! administrator token on Windows (UAC), the root user on Unix. A daemon's
//! sessions all run in its process, so this is a fact about the daemon, read
//! once: the model is told it, the TUI shows it, and telemetry counts it.
//!
//! Pinned Win32 constants and hand-declared externs, with no windows-sys
//! dependency (the policy of this module's siblings).

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
    if windows::token_is_elevated() {
        Elevation::Elevated
    } else {
        Elevation::Standard
    }
}

#[cfg(not(any(unix, windows)))]
fn read_elevation() -> Elevation {
    Elevation::Standard
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    /// `winnt.h`: the right to read a token's information.
    const TOKEN_QUERY: u32 = 0x0008;
    /// `winnt.h` `TokenElevation` (`TOKEN_INFORMATION_CLASS`).
    const TOKEN_ELEVATION: i32 = 20;

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
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    /// Whether this process's token is elevated (`TOKEN_ELEVATION`). A token
    /// that cannot be read reads as not elevated.
    pub(super) fn token_is_elevated() -> bool {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: the pseudo-handle of the current process and a live
        // out-pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
