//! The e2e teardown shared by every target that spawns a real supervisor:
//! reap the supervisor's whole process tree before the test's temp dir
//! drops.
//!
//! Sessions run inside the supervisor, but the commands their cells start
//! are its child processes: a supervisor-only kill would leave those
//! writing into the removed temp dir, which re-created it under `$TMPDIR`.

use std::process::Child;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// Freeze `child` and every process below it, then kill and reap them all.
///
/// The freeze comes first: a stopped process cannot fork, so the
/// descendant walk settles once every member is stopped, and no member
/// writes between the walk and its kill. Best effort by design: this runs
/// inside `Drop`, where a panic would abort the test process.
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
