//! Per-OS daemon endpoint naming, shared by the runtime daemon and the
//! supervisor: Unix socket files under
//! `<tmpdir>/prime-agent-<uid>/`; Windows named pipes in the `\\.\\pipe\\`
//! namespace (fixed daemon pipe, hashed worker pipes) - the TS product's
//! exact split. A distribution build names its own endpoints
//! (`prime-agent-<variant>-<uid>/`, `prime-agent-<variant>-daemon`,
//! [`pa_types::platform::StateLayout::endpoint_prefix`]), so it never reaches
//! the official products' daemon.
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Default directory holding daemon socket files (Unix).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = crate::platform::process::current_user_id();
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
/// the fixed daemon pipe name (Windows).
#[cfg(unix)]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join("daemon.sock")
}

#[cfg(not(unix))]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\{}-daemon",
        pa_types::platform::StateLayout::BUILD.endpoint_prefix()
    ))
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

/// sha256 hex, first `chars` characters: the key that names worker
/// endpoints, daemon logs and update-restart state after a socket path.
#[must_use]
pub fn hash_key(input: &str, chars: usize) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().fold(String::new(), |mut key, b| {
        use std::fmt::Write;
        write!(key, "{b:02x}").expect("write to String");
        key
    })[..chars]
        .to_string()
}

/// Log path for a daemon socket: readable basename plus an 8-char hash of
/// the normalized socket path, under `<agent-dir>/logs` (TS `getDaemonLogPath`).
#[must_use]
pub fn daemon_log_path(socket_path: &Path, agent_dir: &Path) -> PathBuf {
    let normalized = socket_path.to_string_lossy().to_string();
    let base = socket_path.file_name().map_or_else(
        || "daemon.sock".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    agent_dir
        .join("logs")
        .join(format!("{base}.{}.log", hash_key(&normalized, 8)))
}

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
        assert_eq!(
            default_daemon_socket_path(),
            PathBuf::from(r"\\.\pipe\prime-agent-daemon")
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

    #[test]
    fn log_path_hashes_socket() {
        let path = daemon_log_path(
            Path::new("/tmp/prime-agent-1/daemon.sock"),
            Path::new("/ad"),
        );
        assert!(path.starts_with("/ad/logs"));
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("daemon.sock."));
        let other = daemon_log_path(
            Path::new("/tmp/prime-agent-2/daemon.sock"),
            Path::new("/ad"),
        );
        assert_ne!(path, other);
    }
}
