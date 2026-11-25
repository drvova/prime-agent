//! What every e2e target that spawns a real supervisor shares: the wait
//! for it to listen, and the teardown that reaps its whole process tree
//! before the test's temp dir drops.
//!
//! Sessions run inside the supervisor, but the commands their cells start
//! are its child processes: a supervisor-only kill would leave those
//! writing into the removed temp dir, which re-created it under `$TMPDIR`.

use std::process::Child;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// Wait until the daemon `child` accepts connections on `socket`.
///
/// The socket's file is not readiness: it appears at `bind`, before `listen`,
/// and a connect in between is refused. This connects until one is accepted;
/// the probe connection is dropped at once.
///
/// # Panics
///
/// `child` exited first, or 15 seconds passed.
// Each test target compiles this module for itself, and not every one waits
// for a daemon through it.
#[allow(dead_code)]
pub fn await_listening(socket: &std::path::Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "the daemon exited before it listened on {}: {status}",
                socket.display()
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never listened on {}",
            socket.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Freeze `child` and every process below it, then kill and reap them all.
///
/// The freeze comes first: a stopped process cannot fork, so the
/// descendant walk settles once every member is stopped, and no member
/// writes between the walk and its kill. Best effort by design: this runs
/// inside `Drop`, where a panic would abort the test process.
// Each test target compiles this module for itself, and not every one reaps
// through it.
#[allow(dead_code)]
pub fn reap_tree(child: &mut Child) {
    let root = child.id();
    signal(root, Signal::SIGSTOP);
    let tree = freeze_below(&[root]);
    let _ = child.kill();
    let _ = child.wait();
    kill_and_await(&tree);
}

/// Stop every process below `roots` until the walk settles, returning them.
fn freeze_below(roots: &[u32]) -> Vec<u32> {
    let mut tree = Vec::new();
    loop {
        let mut walked: Vec<u32> = roots
            .iter()
            .flat_map(|&root| descendants_of(root))
            .collect();
        walked.sort_unstable();
        if walked == tree {
            return tree;
        }
        for &pid in &walked {
            signal(pid, Signal::SIGSTOP);
        }
        tree = walked;
    }
}

/// SIGKILL each pid and watch it go. The orphans reparent away from this
/// process, so `waitpid` cannot reap them: each is watched until it is gone
/// (or a zombie, which writes nothing), bounded by one shared deadline.
fn kill_and_await(pids: &[u32]) {
    for &pid in pids {
        signal(pid, Signal::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    for &pid in pids {
        while process_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn signal(pid: u32, signal: Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let _ = kill(Pid::from_raw(raw), signal);
}

/// Every process below `root`, following parent links.
fn descendants_of(root: u32) -> Vec<u32> {
    let mut tree = Vec::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for child in child_pids_of(pid) {
            tree.push(child);
            frontier.push(child);
        }
    }
    tree
}

#[cfg(target_os = "linux")]
fn child_pids_of(ppid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let candidate = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{candidate}/stat")).ok()?;
            let (_, rest) = stat.rsplit_once(')')?;
            let parent = rest.split_whitespace().nth(1)?.parse::<u32>().ok()?;
            (parent == ppid).then_some(candidate)
        })
        .collect()
}

/// The children of `ppid` without `/proc`: one `ps` listing of every pid's parent.
#[cfg(not(target_os = "linux"))]
fn child_pids_of(ppid: u32) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let child = fields.next()?.parse().ok()?;
            let parent: u32 = fields.next()?.parse().ok()?;
            (parent == ppid).then_some(child)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next().unwrap_or_default();
    !state.starts_with('Z') && !state.starts_with('X')
}

/// Liveness without `/proc`: a signal-0 probe (a zombie still answers, but
/// a zombie writes nothing, and the deadline bounds the wait).
#[cfg(not(target_os = "linux"))]
fn process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    kill(Pid::from_raw(raw), None).is_ok()
}

/// The database of the nearest agent dir above `path` or beside it as
/// `agent/`: the seam's, or one a process without it kept in the dir.
// Each test target compiles this module for itself, and not every one reads
// a database.
#[allow(dead_code)]
fn database_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    // The tests keep the agent dir beside custom sessions dirs: `<root>/agent`.
    path.ancestors()
        .flat_map(|dir| [dir.to_path_buf(), dir.join("agent")])
        .flat_map(|dir| {
            [
                pa_core::session::store::database_path(&dir),
                dir.join(pa_core::session::store::DATABASE_FILE),
            ]
        })
        .find(|database| database.is_file())
}

/// The agent state (`pa_core::agent_state`) of the database holding the
/// store whose file is `path` ([`database_of`]), when there is one.
#[allow(dead_code)]
pub fn agent_state(path: &std::path::Path) -> Option<&'static pa_core::agent_state::StateDatabase> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError};
    type Opened = HashMap<PathBuf, &'static pa_core::agent_state::StateDatabase>;
    static OPENED: Mutex<Option<Opened>> = Mutex::new(None);
    let database = database_of(path)?;
    let mut opened = OPENED.lock().unwrap_or_else(PoisonError::into_inner);
    Some(
        *opened
            .get_or_insert_with(HashMap::new)
            .entry(database.clone())
            .or_insert_with(|| {
                Box::leak(Box::new(
                    pa_core::agent_state::StateDatabase::open(&database)
                        .expect("open the agent database"),
                ))
            }),
    )
}

/// The jobs of the `scheduled-jobs.json` at `path`, wherever its store
/// keeps them ([`agent_state`], else this process's).
#[allow(dead_code)]
pub fn scheduled_jobs(path: &std::path::Path) -> Vec<pa_core::cron::AgentCronJob> {
    match agent_state(path) {
        Some(state) => state.scheduled_jobs(path),
        None => pa_core::cron::store::read_scheduled_jobs_artifact(path),
    }
}

/// The transcript store holding the transcripts under `path`: the database of
/// the nearest agent dir above it or beside it as `agent/` (a supervisor the
/// test started with its own agent dir keeps them there), else this
/// process's own store (a supervisor
/// that runs on the process's agent dir shares it).
// Each test target compiles this module for itself, and not every one reads
// transcripts.
#[allow(dead_code)]
pub fn transcripts(
    path: &std::path::Path,
) -> &'static dyn pa_core::session::store::TranscriptStore {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError};
    type Opened = HashMap<PathBuf, &'static dyn pa_core::session::store::TranscriptStore>;
    static OPENED: Mutex<Option<Opened>> = Mutex::new(None);
    let Some(database) = database_of(path) else {
        return pa_core::session::store::store();
    };
    let mut opened = OPENED.lock().unwrap_or_else(PoisonError::into_inner);
    *opened
        .get_or_insert_with(HashMap::new)
        .entry(database.clone())
        .or_insert_with(|| {
            Box::leak(
                pa_core::session::store::open_database(&database).expect("open the agent database"),
            )
        })
}

/// The transcript at `path` as text, from the store holding it
/// ([`transcripts`]).
///
/// # Errors
///
/// No transcript is there, or it is not UTF-8.
#[allow(dead_code)]
pub fn read_transcript(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let path = path.as_ref();
    String::from_utf8(transcripts(path).read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Copy the transcript at `path` from the store holding it into this
/// process's own store at the same locator, so this process's session-store
/// readers (`SessionFile::open` and the like) see what a supervisor with its
/// own agent dir wrote.
///
/// # Panics
///
/// The transcript cannot be read or copied.
#[allow(dead_code)]
pub fn mirror_into_process_store(path: &std::path::Path) {
    let bytes = transcripts(path).read(path).expect("read the transcript");
    pa_core::session::store::write(path, bytes).expect("copy the transcript");
}

/// Make the transcript this process stored at `path` a JSONL file on disk
/// instead: an original a supervisor with its own agent dir reads as it is
/// (and imports on its first write), as it would a session from before the
/// database.
///
/// # Panics
///
/// The transcript cannot be moved.
#[allow(dead_code)]
pub fn into_original_file(path: &std::path::Path) {
    let store = pa_core::session::store::store();
    let bytes = store.read(path).expect("read the stored transcript");
    store.remove(path).expect("remove the stored transcript");
    std::fs::write(path, bytes).expect("write the original file");
}
