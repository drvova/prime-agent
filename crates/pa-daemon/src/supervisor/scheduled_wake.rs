//! Scheduled work of the sessions a daemon death took down while they were
//! idle.
//!
//! A hosted session's scheduler lives in the session, so its heartbeats and
//! scheduled jobs stop with the daemon that hosted it. The boot revives only
//! the sessions that were mid-turn; an idle one stays down until a client
//! opens it. For each idle one the boot leaves down, the supervisor reads
//! the session's own `scheduled-jobs.json` and sleeps until its earliest
//! active job is due, then relaunches the session through the boot's
//! adoption path (the same ownership vetoes, the same background place on
//! the daemon-wide boot budget). The relaunched session's scheduler fires
//! what is due, under the store's own rule for a job whose time passed while
//! nothing ran: one fire, and the next run counted from then.
//!
//! Nothing here runs a job, and no session comes up before it has one due:
//! a boot over many saved sessions arms timers, not sessions.
//!
//! Only a session that was running when the daemon went down is armed (its
//! descriptor is still on disk). A session stopped on purpose, passivated
//! idle ones included, has no descriptor and keeps the earlier contract:
//! its jobs stay dormant until it is opened.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use pa_core::cron::store::{read_scheduled_jobs_artifact, SESSION_SCHEDULED_JOBS_FILENAME};
use pa_core::cron::JobStatus;

use super::adoption::{AdoptionBoot, AdoptionOutcome};
use super::Supervisor;

/// The longest a wake sleeps before it reads the session's jobs again, so a
/// job cancelled, paused or rescheduled through the catalog while its
/// session is down moves the wake with it (at most this late).
const RECHECK: Duration = Duration::from_secs(60);

/// One armed wake's entry in [`Supervisor::scheduled_wakes`], removed when
/// the wake ends however it ends.
struct Armed {
    supervisor: Arc<Supervisor>,
    session_file: String,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.supervisor
            .scheduled_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_file);
    }
}

impl Supervisor {
    /// Wake the down session `worker_id` when the earliest active job in its
    /// scheduled-jobs artifact is due. Returns at once; a session with no
    /// active job, or one that is opened, stopped or deleted before then,
    /// is never woken by this.
    pub(super) fn arm_scheduled_wake(
        self: &Arc<Self>,
        worker_id: String,
        session_file: &str,
        session_id: &str,
    ) {
        let Some(jobs_file) =
            crate::scheduled_jobs::session_artifact_dir(Path::new(session_file), session_id)
                .map(|dir| dir.join(SESSION_SCHEDULED_JOBS_FILENAME))
        else {
            return;
        };
        let session_file = pa_core::session::lease::canonical_session_path(Path::new(session_file))
            .to_string_lossy()
            .to_string();
        if !self
            .scheduled_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_file.clone())
        {
            return;
        }
        let armed = Armed {
            supervisor: Arc::clone(self),
            session_file,
        };
        tokio::spawn(async move {
            let supervisor = Arc::clone(&armed.supervisor);
            loop {
                if supervisor.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                let file = jobs_file.clone();
                let due = tokio::task::spawn_blocking(move || earliest_active_run(&file))
                    .await
                    .ok()
                    .flatten();
                let Some(due) = due else {
                    return;
                };
                let wait = due.saturating_sub(crate::util::now_ms());
                if wait == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(wait).min(RECHECK)).await;
            }
            // The descriptor as it stands now: gone when the session was
            // opened under another id, stopped or deleted meanwhile.
            let descriptor_dir = supervisor.descriptor_dir.clone();
            let socket_path = supervisor.options.socket_path.clone();
            let wanted = worker_id.clone();
            let found = tokio::task::spawn_blocking(move || {
                crate::descriptor::load_descriptors(&descriptor_dir, &socket_path)
                    .into_iter()
                    .find(|(_, descriptor)| descriptor.worker_id == wanted)
            })
            .await
            .ok()
            .flatten();
            let Some((path, descriptor)) = found else {
                return;
            };
            let waking = Arc::clone(&supervisor);
            let outcome = pa_core::kernel::boot_admission::admitted_boot(
                supervisor.admission.boot_source(),
                pa_ai::admission::Priority::Background,
                async move {
                    anyhow::Ok(
                        waking
                            .adopt_persisted_worker(path, descriptor, AdoptionBoot::ScheduledWake)
                            .await,
                    )
                },
            )
            .await
            .result
            .unwrap_or(AdoptionOutcome::Failed);
            supervisor.log_line(&format!(
                "session worker {worker_id} has scheduled work due: {}",
                match outcome {
                    AdoptionOutcome::Revived => "woken to run it",
                    AdoptionOutcome::AdoptedLive => "already running",
                    AdoptionOutcome::SkippedIdle => "not woken (see the line above)",
                    AdoptionOutcome::Stopped => "not woken (its stop was finished instead)",
                    AdoptionOutcome::Failed => "its wake failed",
                }
            ));
        });
    }
}

/// When the earliest active job in one session's scheduled-jobs artifact
/// next runs (Unix ms); `None` when it has no active job with a run time.
fn earliest_active_run(jobs_file: &Path) -> Option<u64> {
    read_scheduled_jobs_artifact(jobs_file)
        .iter()
        .filter(|job| job.status == JobStatus::Active)
        .filter_map(|job| job.next_run_at.as_deref())
        .filter_map(pa_core::session::manager::iso_to_unix_ms)
        .min()
}
