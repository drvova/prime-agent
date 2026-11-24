//! pa-core platform wall: every OS-specific behavior behind small traits in
//! cfg-gated modules. Unix and Windows implementations live behind the same
//! signatures; session-engine call sites never branch on `cfg` themselves.

pub mod browser;
pub mod fs;
pub mod lock_dir;
pub mod perms;
pub mod process;
pub mod shell;

pub use fs::fsync;
pub use lock_dir::{lock_exclusive, try_lock_exclusive, unlock_file, LockDir};
pub use perms::{file_mode, is_executable, restrict_dir, restrict_file, set_private_mode};
pub use process::{
    kill_pid, kill_process_group_or_pid, normalized_exit_code, pid_exists, set_new_process_group,
    set_no_window, termination_signal, Signal,
};
// The rename-onto-destination primitive (bounded win32 destination-busy
// retry) lives in pa-types beside the atomic file replacement built on it
// (`pa_types::platform::replace_file`); re-exported so the platform wall
// stays the engine's single platform entry.
pub use pa_types::platform::rename_onto;
pub use shell::{
    get_shell_config, resolve_kernel_bash_shell, resolve_kernel_shell, KernelShell, ShellConfig,
};

/// How pa-core replaces a file that `std::fs::write` used to rewrite in place
/// (learned stores, records, markers, exports): atomically, the file keeping
/// its mode and a symlink its target (what that write kept), unsynced (a
/// crash may lose the write, never tear it).
pub(crate) const REPLACE_KEEPING_MODE: pa_types::platform::ReplaceOptions =
    pa_types::platform::ReplaceOptions {
        durability: pa_types::platform::Durability::Unsynced,
        mode: pa_types::platform::FileMode::KeepTarget,
        symlinks: pa_types::platform::Symlinks::FollowToTarget,
    };
