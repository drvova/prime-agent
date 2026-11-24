//! Per-OS daemon endpoint naming: Unix socket files under
//! `<tmpdir>/prime-agent-<uid>/`; Windows named pipes in the `\\.\\pipe\\`
//! namespace (fixed daemon pipe, hashed worker pipes) - the TS product's
//! exact split. A distribution build names its own endpoints
//! (`prime-agent-<variant>-<uid>/`, `prime-agent-<variant>-daemon`,
//! [`pa_types::platform::StateLayout::endpoint_prefix`]), so it never reaches
//! the official products' daemon.
use std::path::{Path, PathBuf};

use crate::paths::hash_key;

/// Default directory holding daemon socket files (Unix).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = pa_core::platform::process::current_user_id();
    let tmp = pa_types::session_scope::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    socket_dir_for(&tmp, uid)
}

/// Reserve the longest generated endpoint before selecting a directory. Linux
/// can re-anchor deep paths through /proc; other Unix platforms need a short path.
#[cfg(unix)]
fn socket_dir_for(tmp: &Path, uid: u32) -> PathBuf {
    let name = format!(
        "{}-{uid}",
        pa_types::platform::StateLayout::BUILD.endpoint_prefix()
    );
    let dir = tmp.join(&name);
    #[cfg(not(target_os = "linux"))]
    if std::os::unix::net::SocketAddr::from_pathname(
        dir.join("worker-000000000000-000000000000.sock"),
    )
    .is_err()
    {
        return Path::new("/tmp").join(name);
    }
    dir
}

/// The socket-dir half of a discovery state root on Windows: TS computes
/// `<tmpdir>/prime-agent-user` so `DaemonStateRoot` keeps one shape.
#[cfg(not(unix))]
#[must_use]
pub fn socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "{}-user",
        pa_types::platform::StateLayout::BUILD.endpoint_prefix()
    ))
}

/// Default supervisor endpoint: `daemon.sock` in the socket dir (Unix) or
/// the fixed daemon pipe name (Windows). A root process's socket dir is
/// root's own (`prime-agent-0`), so a root daemon never serves the user's
/// normal sessions; Windows has one pipe namespace for every integrity
/// level, so an elevated process names the elevated daemon's own pipe
/// (`prime-agent-elevated-daemon`). The pipe keeps Windows' default access
/// list, whose write access goes to `SYSTEM`, `Administrators` and the
/// creator's owner (by default `Administrators` for an elevated token): a
/// process that is not elevated holds `Administrators` for denying only, so
/// it cannot open the pipe to write.
#[cfg(unix)]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join("daemon.sock")
}

#[cfg(not(unix))]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    let prefix = pa_types::platform::StateLayout::BUILD.endpoint_prefix();
    PathBuf::from(match pa_types::platform::process_elevation() {
        pa_types::platform::Elevation::Standard => format!(r"\\.\pipe\{prefix}-daemon"),
        pa_types::platform::Elevation::Elevated => {
            format!(r"\\.\pipe\{prefix}-elevated-daemon")
        }
    })
}

/// Worker endpoint next to the supervisor's: hashed supervisor key plus the
/// worker id prefix (TS `workerSocketPath`).
#[cfg(unix)]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    socket_dir().join(format!(
        "worker-{key}-{}.sock",
        &worker_id[..12.min(worker_id.len())]
    ))
}

#[cfg(not(unix))]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    PathBuf::from(format!(
        r"\\.\pipe\prime-agent-worker-{key}-{}",
        &worker_id[..12.min(worker_id.len())]
    ))
}

// Socket-filesystem identity is the shared platform contract
// `pa_types::platform::socket_identity`: the same helper serves stale-file
// cleanup here and direct-transport ticket validation in the clients.

pub use pa_types::daemon::SocketIdentity;
pub use pa_types::platform::socket_identity;
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn default_socket_uses_the_same_real_uid_as_the_typescript_daemon() {
        let output = pa_types::session_scope::std_command("id")
            .arg("-ru")
            .output()
            .unwrap();
        assert!(output.status.success());
        let uid = String::from_utf8(output.stdout).unwrap();
        let tmp = pa_types::session_scope::var_os("TMPDIR")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        assert_eq!(
            socket_dir(),
            socket_dir_for(&tmp, uid.trim().parse().unwrap())
        );
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn generated_endpoints_fit_even_when_tmpdir_is_deep() {
        use std::os::unix::net::SocketAddr;

        for length in [20, 65, 120] {
            let tmp = PathBuf::from(format!("/{}", "d".repeat(length - 1)));
            let dir = socket_dir_for(&tmp, 501);
            let worker = dir.join("worker-000000000000-000000000000.sock");
            assert!(SocketAddr::from_pathname(&worker).is_ok());
            if length >= 65 {
                assert_eq!(dir, Path::new("/tmp/prime-agent-501"));
            } else {
                assert_eq!(dir, tmp.join("prime-agent-501"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn short_tmpdir_keeps_the_original_socket_directory() {
        assert_eq!(
            socket_dir_for(Path::new("/tmp"), 501),
            Path::new("/tmp/prime-agent-501")
        );
    }

    #[test]
    fn worker_socket_names_are_deterministic() {
        let supervisor = Path::new("/tmp/prime-agent-1/daemon.sock");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let b = worker_socket_path(supervisor, "fedcba9876543210");
        assert_ne!(a, b);
        // Only the first 12 id characters key the name.
        assert_eq!(a, worker_socket_path(supervisor, "0123456789abffff"));
        #[cfg(unix)]
        assert!(a.starts_with(socket_dir()));
    }

    /// The Windows endpoint names (TS win32 arms): the fixed daemon pipe
    /// name and the hashed worker pipe name in the `\\.\\pipe\\` namespace.
    #[test]
    #[cfg(windows)]
    fn windows_endpoints_are_the_ts_pipe_names() {
        // An elevated process (a CI runner's administrator) reaches the elevated daemon's own.
        assert_eq!(
            default_daemon_socket_path(),
            PathBuf::from(match pa_types::platform::process_elevation() {
                pa_types::platform::Elevation::Standard => r"\\.\pipe\prime-agent-daemon",
                pa_types::platform::Elevation::Elevated => {
                    r"\\.\pipe\prime-agent-elevated-daemon"
                }
            })
        );
        let supervisor = Path::new(r"\\.\pipe\prime-agent-daemon");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let rendered = a.to_string_lossy();
        assert!(
            rendered.starts_with(r"\\.\pipe\prime-agent-worker-"),
            "the worker pipe namespace: {rendered}"
        );
        assert!(
            rendered.ends_with("-0123456789ab"),
            "the 12-char id suffix: {rendered}"
        );
    }
}
