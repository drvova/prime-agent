//! The runtime daemon's boot reap: a predecessor that holds the agent dir's
//! lock but no longer answers on its socket (wedged: stopped, deadlocked,
//! stuck in the kernel) would keep every later runtime daemon from starting.
//! A daemon that finds the lock held asks the holder's socket for a welcome;
//! one that answers runs and the new daemon is refused, one that does not is
//! stopped once it is shown to be a runtime daemon on this agent dir and
//! socket (`pa_core::platform::RuntimeDaemonLock::reap_holder`), and the lock
//! is taken. The stale socket it leaves is removed by the embedder's socket
//! preparation, which runs after the lock.

use std::path::Path;
use std::time::Duration;

use pa_core::platform::process_stop::StopOutcome;
use pa_core::platform::runtime_daemon_lock::HolderReap;
use pa_core::platform::RuntimeDaemonLock;
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::wire2::{ClientFrame, WIRE2_VERSION};
use tokio::io::AsyncWriteExt as _;

/// How long a holder has to answer a hello before it counts as wedged: the
/// welcome is one database read on a connection's own task, so a live
/// daemon under load answers well inside it.
const PROBE_WAIT: Duration = Duration::from_secs(3);

/// How long a stopped holder may take to let go of the lock. The stop ladder
/// counts a zombie as gone, but a multi-threaded process's leader turns
/// zombie while its other threads are still exiting with the lock's file
/// open, so the lock frees a moment after the stop returns.
const RELEASE_WAIT: Duration = Duration::from_secs(5);

/// How often the lock is tried while a stopped holder lets go of it.
const RELEASE_POLL: Duration = Duration::from_millis(10);

/// Takes the agent dir's runtime-daemon lock for a daemon serving `socket`,
/// kept by `keeper`, reaping a wedged predecessor on the same socket first.
///
/// # Errors
///
/// A runtime daemon that answers, or one that cannot be shown to be a
/// wedged runtime daemon on this socket, holds the lock (the error names its
/// pid); the predecessor outlived the reap; or the lock file failed.
pub async fn take_lock(
    agent_dir: &Path,
    socket: &Path,
    keeper: Option<u32>,
) -> anyhow::Result<RuntimeDaemonLock> {
    if let Some(lock) = RuntimeDaemonLock::try_acquire(agent_dir, socket, keeper)? {
        return Ok(lock);
    }
    let refusal = || {
        anyhow::anyhow!(
            "a runtime daemon is already running on {} (pid {})",
            agent_dir.display(),
            RuntimeDaemonLock::recorded(agent_dir)
                .map_or_else(|| "unknown".to_string(), |holder| holder.pid.to_string())
        )
    };
    if answers_hello(socket).await {
        return Err(refusal());
    }
    match RuntimeDaemonLock::reap_holder(agent_dir, socket).await {
        HolderReap::NotOurs => return Err(refusal()),
        HolderReap::Stopped { pid, outcome } => {
            tracing::warn!(
                pid,
                ?outcome,
                "boot reap: a wedged runtime daemon held the agent dir's lock"
            );
            match outcome {
                StopOutcome::AlreadyGone | StopOutcome::Term | StopOutcome::Kill => {}
                StopOutcome::Survived => {
                    anyhow::bail!(
                        "a wedged runtime daemon (pid {pid}) holds the lock on {} and outlived SIGKILL",
                        agent_dir.display()
                    )
                }
            }
        }
    }
    let released_by = tokio::time::Instant::now() + RELEASE_WAIT;
    loop {
        if let Some(lock) = RuntimeDaemonLock::try_acquire(agent_dir, socket, keeper)? {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= released_by {
            return Err(refusal());
        }
        tokio::time::sleep(RELEASE_POLL).await;
    }
}

/// Whether a daemon on `socket` answers a wire2 hello with any frame within
/// [`PROBE_WAIT`].
async fn answers_hello(socket: &Path) -> bool {
    tokio::time::timeout(PROBE_WAIT, async {
        let stream = pa_types::platform::transport::connect_transport(socket)
            .await
            .ok()?;
        let (reader, mut writer) = stream.split();
        let hello = serde_json::to_value(ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: "runtime-daemon-boot-reap".to_string(),
        })
        .ok()?;
        let bytes = encode_private_frame(&hello, &[], DEFAULT_PRIVATE_FRAME_LIMITS).ok()?;
        writer.write_all(&bytes).await.ok()?;
        writer.flush().await.ok()?;
        PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS)
            .read_frame()
            .await
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten()
    .is_some()
}
