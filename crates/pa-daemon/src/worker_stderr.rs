//! Per-session logs. When every session was a worker process its stderr went
//! to a per-worker file; hosted sessions share the daemon's stderr, so the
//! lines a session's code logs are routed here instead: [`line`] writes each
//! one, tagged with the session, to that session's own log under the daemon's
//! logs dir (the sink is a slot of the session's scope, opened on the
//! session's first line). This crate's `eprintln!` is that function (see
//! `lib.rs`), so session code logs exactly as it did; code outside any
//! session still reaches the daemon's stderr.
//!
//! A bounded tail of the log rides a launch failure to the client, and
//! opening a log prunes the directory to the newest [`RETAINED_FILES`].

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// The stderr tail attached to not-ready launch failures (Codex
/// `STDERR_LOG_TAIL_BYTES`).
const TAIL_BYTES: u64 = 4096;

/// How many worker stderr logs the daemon retains (the prune cap).
const RETAINED_FILES: usize = 64;

/// Logs younger than this are never prune targets: a concurrent spawn's prune
/// cannot unlink another launch's fresh log.
const PRUNE_PROTECTION_SECS: u64 = 120;

/// A session log past this size starts over when a lifetime of its session
/// opens it.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// The session's log: `worker-<id>.stderr.log` under the daemon's logs dir
/// (the name per-worker logs always had).
pub(crate) fn log_path(agent_dir: &Path, worker_id: &str) -> PathBuf {
    crate::paths::logs_dir(agent_dir).join(format!("worker-{worker_id}.stderr.log"))
}

/// One hosted session lifetime's log sink (a slot of its scope). `None` when
/// the log could not be opened: the session's lines then go to the daemon's
/// stderr, still tagged.
struct SessionLog(Option<std::sync::Mutex<File>>);

/// Open a session's log for appending, creating it and the logs dir, and
/// prune the directory's older session logs.
fn open_log(path: &Path) -> std::io::Result<File> {
    if let Some(logs_dir) = path.parent() {
        std::fs::create_dir_all(logs_dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    pa_core::platform::perms::set_private_mode(&mut options);
    let file = options.open(path)?;
    if file.metadata()?.len() > MAX_LOG_BYTES {
        file.set_len(0)?;
    }
    if let Some(logs_dir) = path.parent() {
        prune_retained(logs_dir, path, |name| {
            name.starts_with("worker-") && name.ends_with(".stderr.log")
        });
    }
    Ok(file)
}

/// Log one line from wherever the caller runs: inside a hosted session, to
/// that session's own log, tagged with the time and the session; anywhere
/// else, to the daemon's stderr. This is what `eprintln!` means in this crate.
pub(crate) fn line(message: std::fmt::Arguments<'_>) {
    use std::io::Write as _;
    let Some(scope) = pa_types::session_scope::current() else {
        std::eprintln!("{message}");
        return;
    };
    // Resolved before the slot is taken: the scope answers both.
    let agent_dir = crate::paths::agent_dir();
    let log = scope.slot_or_insert_with(|| {
        SessionLog(
            agent_dir
                .ok()
                .and_then(|agent_dir| open_log(&log_path(&agent_dir, scope.id())).ok())
                .map(std::sync::Mutex::new),
        )
    });
    let tagged = format!(
        "{} [session {}] {message}\n",
        crate::util::now_iso(),
        scope.id()
    );
    let written = log.0.as_ref().is_some_and(|file| {
        file.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(tagged.as_bytes())
            .is_ok()
    });
    if !written {
        std::eprint!("{tagged}");
    }
}

/// Append the reason a session never came up to that session's log, from
/// outside the session (it is gone): the launch failure's tail then carries
/// it even when the session logged nothing itself.
pub(crate) fn append_launch_failure(agent_dir: &Path, worker_id: &str, reason: &str) {
    use std::io::Write as _;
    let appended = open_log(&log_path(agent_dir, worker_id)).and_then(|mut file| {
        writeln!(
            file,
            "{} [session {worker_id}] Error: {reason}",
            crate::util::now_iso()
        )
    });
    if let Err(error) = appended {
        std::eprintln!("session {worker_id}: its log could not be written ({error}): {reason}");
    }
}

/// Keep only the newest [`RETAINED_FILES`] matching logs: worker ids are
/// minted per launch and socket paths accumulate, so without the prune
/// every session ever hosted leaves a log. The `keep` path is spared;
/// deletion is best-effort.
fn prune_retained(logs_dir: &Path, keep: &Path, matches: impl Fn(&str) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return 0;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
        })
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            // A future mtime (clock skew) reads as protected: a skewed clock must not
            // turn a fresh log into a prune target.
            let fresh = modified.elapsed().map_or(true, |age| {
                age < std::time::Duration::from_secs(PRUNE_PROTECTION_SECS)
            });
            if fresh {
                return None;
            }
            Some((modified, path))
        })
        .collect();
    if logs.len() <= RETAINED_FILES {
        return 0;
    }
    logs.sort_by_key(|(modified, _)| *modified);
    let excess = logs.len() - RETAINED_FILES;
    let mut deleted = 0;
    for (_, path) in &logs {
        if deleted == excess {
            break;
        }
        if path == keep {
            continue;
        }
        let _ = std::fs::remove_file(path);
        deleted += 1;
    }
    deleted
}

fn socket_log_name(name: &str) -> bool {
    let stem = name
        .strip_suffix(".log.1")
        .or_else(|| name.strip_suffix(".log"))
        .unwrap_or("");
    stem.rsplit_once('.')
        .is_some_and(|(_, hash)| hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(crate) fn prune_socket_logs(agent_dir: &Path, socket_path: &Path) -> usize {
    let keep = crate::paths::daemon_log_path(socket_path, agent_dir);
    prune_retained(&crate::paths::logs_dir(agent_dir), &keep, socket_log_name)
}

/// Read the last [`TAIL_BYTES`] of a worker stderr log, dropping the leading partial
/// line (the read is bounded). Returns `Ok(None)` for a missing or empty log.
///
/// # Errors
///
/// Returns an error when the log cannot be opened, inspected, or read and it exists.
fn read_tail(path: &Path) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open worker stderr log {}", path.display()));
        }
    };
    let len = file
        .metadata()
        .with_context(|| format!("inspect worker stderr log {}", path.display()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("seek worker stderr log {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read worker stderr log {}", path.display()))?;
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        }
    }
    let contents = String::from_utf8_lossy(&bytes).trim_end().to_string();
    if contents.is_empty() {
        return Ok(None);
    }
    Ok(Some(contents))
}

/// Attach the worker's captured stderr tail to a not-ready launch failure so
/// the error names the panic the supervisor only saw as silence. The base
/// headline stays first; an unreadable log degrades to a note.
pub(crate) fn not_ready_with_tail(base: anyhow::Error, log_path: &Path) -> anyhow::Error {
    match read_tail(log_path) {
        Ok(Some(tail)) => anyhow!(
            "{base}\n\nsession worker stderr ({}):\n{}",
            log_path.display(),
            tail.lines()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Ok(None) => base,
        Err(error) => anyhow!("{base}\n\nfailed to read session worker stderr log: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn write_log(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        // The name may carry a subpath (the truncation test writes into
        // <agent>/logs/): the parent is created, not just the base dir.
        std::fs::create_dir_all(path.parent().expect("log path parent")).expect("create log dir");
        std::fs::write(&path, contents).expect("write log");
        path
    }

    /// Set the file's mtime deterministically regardless of filesystem
    /// timestamp granularity.
    fn backdate(path: &Path, seconds: i64) {
        let mtime = filetime::FileTime::from_unix_time(seconds, 0);
        filetime::set_file_mtime(path, mtime).expect("set mtime");
    }

    #[test]
    fn tail_reads_the_last_4k_from_a_line_boundary() {
        use std::fmt::Write as _;
        let dir = tempfile::tempdir().expect("temp dir");
        // 8 KiB of numbered lines: the tail must be exactly the last
        // TAIL_BYTES worth, minus the leading partial line.
        let mut contents = String::new();
        let mut line_index = 0;
        while contents.len() < 8192 {
            writeln!(contents, "panic trace line {line_index}").expect("write to String");
            line_index += 1;
        }
        let path = write_log(dir.path(), "worker-a.stderr.log", &contents);
        let tail = read_tail(&path)
            .expect("read tail")
            .expect("non-empty tail");
        assert!(tail.len() <= TAIL_BYTES as usize);
        assert!(
            tail.contains(&format!("panic trace line {}", line_index - 1)),
            "holds the newest line"
        );
        assert!(
            tail.lines()
                .all(|line| line.starts_with("panic trace line")),
            "no partial line: {tail}"
        );
        assert!(tail.starts_with("panic trace line"));
    }

    #[test]
    fn tail_is_none_for_missing_and_empty_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            read_tail(&dir.path().join("absent.stderr.log")),
            Ok(None)
        ));
        let path = write_log(dir.path(), "worker-a.stderr.log", "");
        assert!(matches!(read_tail(&path), Ok(None)));
    }

    #[test]
    fn not_ready_error_carries_the_tail() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_log(
            dir.path(),
            "worker-a.stderr.log",
            "Error: bind worker socket failed\nthread 'main' panicked\n",
        );
        let error = not_ready_with_tail(anyhow!("session worker a did not come up in time"), &path);
        let message = format!("{error:#}");
        assert!(
            message.starts_with("session worker a did not come up in time"),
            "headline stays first: {message}"
        );
        assert!(
            message.contains(&format!("session worker stderr ({}):", path.display())),
            "names the captured log: {message}"
        );
        assert!(
            message.contains("  Error: bind worker socket failed"),
            "indents the worker's stderr lines: {message}"
        );
        assert!(message.contains("  thread 'main' panicked"));
    }

    #[test]
    fn not_ready_error_keeps_the_bare_headline_for_a_silent_worker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_log(dir.path(), "worker-a.stderr.log", "");
        let error = not_ready_with_tail(anyhow!("session worker a did not come up in time"), &path);
        assert_eq!(
            format!("{error:#}"),
            "session worker a did not come up in time"
        );
    }

    #[test]
    fn socket_log_retention_keeps_the_newest_and_spares_foreign_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let agent_dir = dir.path().join("agent");
        let logs_dir = agent_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).expect("logs dir");
        for index in 0..(RETAINED_FILES + 6) {
            let path = write_log(&logs_dir, &format!("one.sock.{index:08x}.log"), "line\n");
            backdate(&path, index as i64);
        }
        let rotated = write_log(&logs_dir, "other.sock.abcdef01.log.1", "line\n");
        backdate(&rotated, 0);
        let fresh = write_log(&logs_dir, "fresh.sock.abcdef01.log", "line\n");
        let socket = dir.path().join("daemon.sock");
        let keep = crate::paths::daemon_log_path(&socket, &agent_dir);
        std::fs::write(&keep, "supervisor started\n").expect("keep log");
        let foreign = write_log(&logs_dir, "worker-009.stderr.log", "worker\n");
        let jsonl = write_log(&logs_dir, "agent.jsonl", "{}\n");

        assert_eq!(
            prune_socket_logs(&agent_dir, &socket),
            7,
            "the oldest socket logs past the cap are pruned, the rotation included"
        );
        let remaining_socket_logs = std::fs::read_dir(&logs_dir)
            .expect("read logs dir")
            .filter_map(std::result::Result::ok)
            .filter(|entry| socket_log_name(&entry.file_name().to_string_lossy()))
            .count();
        assert_eq!(
            remaining_socket_logs,
            RETAINED_FILES + 2,
            "the newest RETAINED_FILES aged logs plus the fresh log and the keep survive"
        );
        assert!(keep.exists());
        assert!(fresh.exists());
        assert!(foreign.exists());
        assert!(jsonl.exists());
    }
}
