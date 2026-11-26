//! The runtime daemon's restart owner and boot reap (phase 7c-2), ported
//! from the old daemon's `daemon_keeper_e2e` and `daemon_boot_reap_e2e`:
//!
//! - a kept runtime daemon killed outright (SIGKILL) is started again by its
//!   keeper on the same socket, and the death is recorded under the runtime
//!   daemon's socket, never the old daemon's;
//! - a deliberate stop (the daemon's own drain on SIGTERM, or a kill after a
//!   recorded stop intent) is not restarted;
//! - a runtime daemon that dies at every start is given up on at the limit,
//!   every death recorded;
//! - a wedged (SIGSTOP'd) runtime daemon holding the agent dir's lock is
//!   reaped by the next one, which then starts;
//! - a process the lock file names that is not a runtime daemon holding the
//!   lock is never signalled, and the new daemon is refused.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pa_types::daemon::framing::{encode_private_frame, DEFAULT_PRIVATE_FRAME_LIMITS};
use pa_types::wire2::{ClientFrame, WIRE2_VERSION};
use serde_json::{json, Value};

use super::Bed;

/// How long any one wait in this file may take before the test fails.
const WAIT: Duration = Duration::from_secs(60);

/// A restart owner and the runtime daemon it keeps; dropping it ends both.
struct Kept {
    keeper: Child,
    agent_dir: PathBuf,
}

impl Drop for Kept {
    fn drop(&mut self) {
        // The keeper first, so nothing restarts the daemon killed after it.
        let _ = self.keeper.kill();
        let _ = self.keeper.wait();
        // A daemon killed by the test leaves its record behind: only a live
        // product process under it is the daemon.
        if let Some(holder) = pa_core::platform::RuntimeDaemonLock::recorded(&self.agent_dir)
            .filter(|holder| pa_core::platform::process_stop::exe_is_product_binary(holder.pid))
        {
            signal(holder.pid, Signal::SIGKILL);
        }
    }
}

impl Bed {
    /// The runtime daemon on `daemon_socket`'s directory, kept by a restart
    /// owner (the launch an interactive client makes).
    fn kept_runtime_daemon(&self, daemon_socket: &Path) -> Kept {
        let mut command = self.runtime_daemon_command(&json!({ "responses": [] }), daemon_socket);
        command.env(pa_daemon::keeper::KEEPER_ENV, "1");
        Kept {
            keeper: command.spawn().expect("spawn the restart owner"),
            agent_dir: self.agent_dir(),
        }
    }

    /// The deaths the keeper of the daemon on `socket` recorded.
    fn deaths(&self, socket: &Path) -> Vec<Value> {
        std::fs::read_to_string(
            pa_daemon::descriptor::descriptor_dir(&self.agent_dir(), socket)
                .join(pa_daemon::keeper::DAEMON_DEATH_LOG),
        )
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a death record"))
        .collect()
    }
}

fn signal(pid: u32, signal: Signal) {
    let _ = kill(Pid::from_raw(i32::try_from(pid).expect("a pid")), signal);
}

/// Whether a daemon on `socket` answers a wire2 hello within a second.
fn answers(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let hello = encode_private_frame(
        &serde_json::to_value(ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: "runtime-daemon-e2e".to_string(),
        })
        .unwrap(),
        &[],
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream.write_all(&hello).is_ok() && stream.read(&mut [0; 1]).is_ok_and(|read| read > 0)
}

/// Waits until `ready` holds, failing as `what` after [`WAIT`].
fn wait_for<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The runtime daemon answering on the bed's runtime socket, as its lock
/// records it, once that is not `previous`.
fn serving_holder(
    bed: &Bed,
    previous: Option<u32>,
) -> pa_core::platform::runtime_daemon_lock::RecordedHolder {
    wait_for("a runtime daemon to serve", || {
        pa_core::platform::RuntimeDaemonLock::recorded(&bed.agent_dir())
            .filter(|holder| Some(holder.pid) != previous)
            .filter(|_| answers(&bed.runtime_socket()))
    })
}

/// The restart owner's exit code, once it exits.
fn exit_code(keeper: &mut Child) -> Option<i32> {
    wait_for("the restart owner to exit", || {
        keeper
            .try_wait()
            .expect("poll the restart owner")
            .map(|status| status.code())
    })
}

/// A death record without its time and uptime.
fn death_summary(record: &Value) -> Value {
    json!({
        "pid": record["pid"],
        "signal": record["signal"],
        "exitCode": record["exitCode"],
        "liveSessions": record.get("liveSessions"),
        "consecutive": record["consecutive"],
        "outcome": record["outcome"],
    })
}

#[test]
fn a_killed_kept_runtime_daemon_is_restarted_and_its_death_recorded() {
    let bed = Bed::new();
    let kept = bed.kept_runtime_daemon(&bed.old_socket());
    let first = serving_holder(&bed, None);
    signal(first.pid, Signal::SIGKILL);
    let second = serving_holder(&bed, Some(first.pid));
    let deaths = bed.deaths(&bed.runtime_socket());
    assert_eq!(
        (
            first.keeper,
            second.keeper,
            deaths.iter().map(death_summary).collect::<Vec<_>>(),
            bed.deaths(&bed.old_socket()),
        ),
        (
            Some(kept.keeper.id()),
            Some(kept.keeper.id()),
            vec![json!({
                "pid": first.pid, "signal": 9, "exitCode": null, "liveSessions": null,
                "consecutive": 1, "outcome": "restarted",
            })],
            Vec::<Value>::new(),
        ),
        "deaths: {deaths:?}"
    );
}

#[test]
fn a_deliberate_stop_of_a_kept_runtime_daemon_is_not_restarted() {
    // The daemon's own drain on SIGTERM: it exits cleanly and its keeper with it.
    let drained = Bed::new();
    let mut drained_kept = drained.kept_runtime_daemon(&drained.old_socket());
    signal(serving_holder(&drained, None).pid, Signal::SIGTERM);
    let drained_exit = exit_code(&mut drained_kept.keeper);

    // A stop that escalates: the stopper records its intent, then kills the
    // daemon outright. The keeper reads the kill as the stop it is.
    let killed = Bed::new();
    let mut killed_kept = killed.kept_runtime_daemon(&killed.old_socket());
    let holder = serving_holder(&killed, None);
    pa_daemon::keeper::record_stop_intent(&killed.agent_dir(), &killed.runtime_socket())
        .expect("record the stop intent");
    signal(holder.pid, Signal::SIGKILL);
    let killed_exit = exit_code(&mut killed_kept.keeper);

    assert_eq!(
        (
            drained_exit,
            killed_exit,
            answers(&drained.runtime_socket()),
            answers(&killed.runtime_socket()),
            drained.deaths(&drained.runtime_socket()),
            killed.deaths(&killed.runtime_socket()),
        ),
        (
            Some(0),
            Some(0),
            false,
            false,
            Vec::<Value>::new(),
            Vec::<Value>::new()
        )
    );
}

#[test]
fn a_runtime_daemon_that_dies_at_every_start_is_given_up_on() {
    let bed = Bed::new();
    // The socket's directory is a regular file: the daemon cannot bind and
    // exits at once, every time.
    let blocker = bed.dir.path().join("blocked");
    std::fs::write(&blocker, "not a directory").unwrap();
    let mut kept = bed.kept_runtime_daemon(&blocker.join("daemon.sock"));
    let exit = exit_code(&mut kept.keeper);
    let summary: Vec<Value> = bed
        .deaths(&blocker.join("daemon2.sock"))
        .iter()
        .map(|record| {
            json!([
                record["consecutive"],
                record["outcome"],
                record["signal"],
                record["exitCode"].is_number()
            ])
        })
        .collect();
    assert_eq!(
        (exit, summary),
        (
            Some(1),
            vec![
                json!([1, "restarted", null, true]),
                json!([2, "restarted", null, true]),
                json!([3, "restarted", null, true]),
                json!([4, "restarted", null, true]),
                json!([5, "gave_up", null, true]),
            ]
        )
    );
}

#[test]
fn a_wedged_runtime_daemon_holding_the_lock_is_reaped_and_the_next_one_starts() {
    let bed = Bed::new();
    let script = json!({ "responses": [] });
    let mut wedged = bed.runtime_daemon(&script);
    signal(wedged.0.id(), Signal::SIGSTOP);
    let mut next = bed
        .runtime_daemon_command(&script, &bed.old_socket())
        .spawn()
        .expect("spawn the next runtime daemon");
    let next_pid = next.id();
    let holder = wait_for("the next runtime daemon to serve", || {
        if let Ok(Some(status)) = next.try_wait() {
            panic!("the next runtime daemon exited: {status}");
        }
        pa_core::platform::RuntimeDaemonLock::recorded(&bed.agent_dir())
            .filter(|holder| holder.pid == next_pid)
            .filter(|_| answers(&bed.runtime_socket()))
    });
    let wedged_end = wedged.0.wait().expect("reap the wedged daemon");
    let _ = next.kill();
    let _ = next.wait();
    assert_eq!(
        (
            holder.socket,
            pa_core::platform::process::termination_signal(&wedged_end),
        ),
        (bed.runtime_socket(), Some(9))
    );
}

#[test]
fn a_process_the_lock_names_that_is_not_its_holder_is_never_signalled() {
    let bed = Bed::new();
    // Another product process on the agent dir that does not hold the lock:
    // an old daemon on a socket of its own, whose boot reap never takes the
    // runtime daemon below (its command line names another socket) for a
    // predecessor.
    let other_socket = bed.dir.path().join("other").join("daemon.sock");
    let mut command = Command::new(
        Path::new(env!("CARGO_BIN_EXE_prime-agent"))
            .parent()
            .expect("the profile dir")
            .join("pa-daemon"),
    );
    bed.env(&mut command, "old-daemon.log");
    let mut unrelated = super::Daemon(
        command
            .arg("supervisor")
            .arg("--socket")
            .arg(&other_socket)
            .arg("--agent-dir")
            .arg(bed.agent_dir())
            .spawn()
            .expect("spawn the old daemon"),
    );
    super::support::await_listening(&other_socket, &mut unrelated.0);
    // This test holds the lock and names that process as its holder on the
    // runtime daemon's socket, which nothing serves.
    let _lock = pa_core::platform::RuntimeDaemonLock::try_acquire(
        &bed.agent_dir(),
        &bed.runtime_socket(),
        None,
    )
    .unwrap()
    .expect("the lock is free");
    std::fs::write(
        bed.agent_dir().join("runtime-daemon.lock"),
        json!({ "pid": unrelated.0.id(), "socket": bed.runtime_socket(), "keeper": null })
            .to_string(),
    )
    .unwrap();
    let mut refused = bed.runtime_daemon_command(&json!({ "responses": [] }), &bed.old_socket());
    refused.stderr(std::process::Stdio::piped());
    let output = refused.output().expect("run the runtime daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        (
            output.status.code(),
            stderr.contains(&format!(
                "a runtime daemon is already running on {} (pid {})",
                bed.agent_dir().display(),
                unrelated.0.id()
            )),
            unrelated.0.try_wait().expect("poll the old daemon"),
        ),
        (Some(1), true, None),
        "stderr: {stderr}"
    );
}
