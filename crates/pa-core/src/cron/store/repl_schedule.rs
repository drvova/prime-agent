//! Session-owned scheduled jobs created from the kernel's `schedule` module
//! (`schedule.add/list/cancel`): ordinary cron jobs (they fire as a plain
//! prompt on the follow-up lane, persist in the session's artifact partition,
//! and show in `prime-agent schedule list`) tagged with their own source, so a
//! session lists and cancels only the jobs it created, never the user's.

use super::state::compare_optional_iso;
use super::{AgentCronJobStore, CreateAgentCronJobInput};
use crate::cron::{
    next_run_at_for_schedule, parse_agent_cron_schedule, AgentCronJob, JobStatus, ScheduleKind,
};

/// The `source` every kernel-scheduled job carries.
pub(crate) const REPL_SCHEDULE_SOURCE: &str = "repl_schedule";

/// Live (active or paused) kernel-scheduled jobs one session may hold.
pub(crate) const MAX_REPL_SCHEDULES_PER_SESSION: usize = 10;

/// The shortest gap a recurring kernel-scheduled job may fire at.
pub(crate) const MIN_REPL_SCHEDULE_INTERVAL_MS: u64 = 5 * 60 * 1000;

/// How many upcoming cron fires the minimum-gap check walks.
const CRON_GAP_SAMPLES: usize = 64;

/// When a kernel-scheduled job fires: exactly one of the `schedule.add` keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplScheduleWhen {
    /// One shot: an ISO-8601 time, or a relative `in 30m`.
    At(String),
    /// Recurring at a fixed interval (`30m`, `every 2h`).
    Every(String),
    /// Recurring on a five-field cron expression (or alias such as `@daily`).
    Cron(String),
}

/// Which of a session's kernel-scheduled jobs a listing returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplScheduleListing {
    /// Active and paused jobs.
    Live,
    /// Every job, finished and cancelled ones included.
    All,
}

fn owned_by(job: &AgentCronJob, active_session_id: &str) -> bool {
    job.active_session_id == active_session_id
        && job.source.as_deref() == Some(REPL_SCHEDULE_SOURCE)
}

fn is_live(job: &AgentCronJob) -> bool {
    matches!(job.status, JobStatus::Active | JobStatus::Paused)
}

impl AgentCronJobStore {
    /// The session's kernel-scheduled jobs, soonest first.
    pub(crate) fn list_repl_schedules(
        &self,
        active_session_id: &str,
        listing: ReplScheduleListing,
    ) -> Vec<AgentCronJob> {
        let mut jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .filter(|job| {
                owned_by(job, active_session_id)
                    && match listing {
                        ReplScheduleListing::Live => is_live(job),
                        ReplScheduleListing::All => true,
                    }
            })
            .collect();
        jobs.sort_by(|left, right| {
            compare_optional_iso(left.next_run_at.as_deref(), right.next_run_at.as_deref())
        });
        jobs
    }

    /// Create a kernel-scheduled job for the input's session, enforcing the per-session cap and
    /// the recurring minimum interval.
    ///
    /// # Errors
    ///
    /// Returns an error when the prompt is empty, the schedule does not parse as the requested
    /// kind, a recurring schedule fires more often than the minimum, the session already holds
    /// the maximum number of jobs, or the write fails.
    pub(crate) fn create_repl_schedule(
        &self,
        input: &CreateAgentCronJobInput,
        when: &ReplScheduleWhen,
    ) -> anyhow::Result<AgentCronJob> {
        let now = input.now.unwrap_or_else(super::now_millis);
        let (schedule_text, kind) = match when {
            ReplScheduleWhen::At(text) => {
                let text = text.trim();
                let relative = text.to_lowercase().starts_with("in ");
                let schedule = if relative {
                    text.to_string()
                } else {
                    format!("at {text}")
                };
                (schedule, ScheduleKind::Once)
            }
            ReplScheduleWhen::Every(text) => {
                let text = text.trim();
                let lower = text.to_lowercase();
                let schedule = if lower.starts_with("every ") || lower.starts_with("each ") {
                    text.to_string()
                } else {
                    format!("every {text}")
                };
                (schedule, ScheduleKind::Interval)
            }
            ReplScheduleWhen::Cron(text) => (text.trim().to_string(), ScheduleKind::Cron),
        };
        let (schedule, first_run) = parse_agent_cron_schedule(&schedule_text, now)?;
        if schedule.kind != kind {
            anyhow::bail!(
                "schedule {schedule_text:?} is not {}",
                match kind {
                    ScheduleKind::Once => "a one-shot time (an ISO-8601 time or `in 30m`)",
                    ScheduleKind::Interval => "an interval (`30m`, `2h`, `1d`)",
                    ScheduleKind::Cron => "a five-field cron expression",
                }
            );
        }
        let live = self
            .read_jobs()
            .iter()
            .filter(|job| owned_by(job, &input.active_session_id) && is_live(job))
            .count();
        if live >= MAX_REPL_SCHEDULES_PER_SESSION {
            anyhow::bail!(
                "this session already has {live} scheduled jobs (the maximum is \
                 {MAX_REPL_SCHEDULES_PER_SESSION}); cancel one with schedule.cancel(id) first"
            );
        }
        // The shortest gap between the schedule's upcoming fires (a one-shot has none).
        let mut previous = first_run;
        let mut shortest_gap = u64::MAX;
        for _ in 0..CRON_GAP_SAMPLES {
            let Some(next) = next_run_at_for_schedule(&schedule, previous)? else {
                break;
            };
            shortest_gap = shortest_gap.min(next.saturating_sub(previous));
            previous = next;
        }
        if shortest_gap < MIN_REPL_SCHEDULE_INTERVAL_MS {
            anyhow::bail!(
                "a recurring schedule must fire at most every {} minutes",
                MIN_REPL_SCHEDULE_INTERVAL_MS / 60_000
            );
        }
        let job = self.create(&CreateAgentCronJobInput {
            schedule_text,
            source: Some(REPL_SCHEDULE_SOURCE.to_string()),
            delivery_mode: None,
            now: Some(now),
            ..input.clone()
        })?;
        // Plain cron jobs are outside the heartbeat signature the write compares, so the
        // catalog-change broadcast (which refreshes `prime-agent schedule list`) fires here.
        self.notify_heartbeat_change();
        Ok(job)
    }

    /// Cancel one of the session's kernel-scheduled jobs. `Ok(None)` when the session owns no
    /// such job (another session's job and the user's own jobs answer the same); a job that
    /// already finished comes back unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub(crate) fn cancel_repl_schedule(
        &self,
        active_session_id: &str,
        id: &str,
        now: u64,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let Some(job) = self
            .read_jobs()
            .into_iter()
            .find(|job| job.id == id && owned_by(job, active_session_id))
        else {
            return Ok(None);
        };
        if !is_live(&job) {
            return Ok(Some(job));
        }
        let cancelled = self.cancel(id, now)?;
        self.notify_heartbeat_change();
        Ok(cancelled)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::cron::store::{input, iso_from_millis};
    use crate::cron::AgentCronSchedule;

    const NOW: u64 = 1_700_000_000_000;

    fn store() -> (tempfile::TempDir, AgentCronJobStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        (dir, store)
    }

    fn add(
        store: &AgentCronJobStore,
        session: &str,
        when: &ReplScheduleWhen,
    ) -> anyhow::Result<AgentCronJob> {
        let mut input = input("check CI", "unused", NOW);
        input.active_session_id = session.to_string();
        store.create_repl_schedule(&input, when)
    }

    /// A session sees and cancels only the jobs it scheduled: the user's own cron job on the
    /// same session and another session's kernel job stay out of reach.
    #[test]
    fn a_session_lists_and_cancels_only_its_own_jobs() {
        let (_dir, store) = store();
        let users = store.create(&input("user job", "every 1h", NOW)).unwrap();
        let mine = add(
            &store,
            "live-1",
            &ReplScheduleWhen::At("in 30m".to_string()),
        )
        .unwrap();
        let theirs = add(&store, "live-2", &ReplScheduleWhen::Every("1h".to_string())).unwrap();
        assert_eq!(
            (
                mine.schedule.clone(),
                mine.source.clone(),
                mine.next_run_at.clone()
            ),
            (
                AgentCronSchedule {
                    kind: ScheduleKind::Once,
                    expression: "in 30m".to_string(),
                    interval_ms: None,
                },
                Some(REPL_SCHEDULE_SOURCE.to_string()),
                Some(iso_from_millis(NOW + 30 * 60_000)),
            )
        );
        assert_eq!(
            store.list_repl_schedules("live-1", ReplScheduleListing::Live),
            vec![mine.clone()]
        );
        assert_eq!(
            store
                .cancel_repl_schedule("live-1", &users.id, NOW)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .cancel_repl_schedule("live-1", &theirs.id, NOW)
                .unwrap(),
            None
        );
        let cancelled = store
            .cancel_repl_schedule("live-1", &mine.id, NOW + 1)
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(
            store.list_repl_schedules("live-1", ReplScheduleListing::Live),
            Vec::new()
        );
        assert_eq!(
            store.list_repl_schedules("live-1", ReplScheduleListing::All),
            vec![cancelled.clone()]
        );
        // A second cancel answers the finished job unchanged.
        assert_eq!(
            store
                .cancel_repl_schedule("live-1", &mine.id, NOW + 2)
                .unwrap(),
            Some(cancelled)
        );
        let mut statuses: Vec<(String, JobStatus)> = store
            .list()
            .into_iter()
            .map(|job| (job.id, job.status))
            .collect();
        statuses.sort_by(|left, right| left.0.cmp(&right.0));
        let mut expected = vec![
            (users.id, JobStatus::Active),
            (theirs.id, JobStatus::Active),
            (mine.id, JobStatus::Cancelled),
        ];
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(statuses, expected);
    }

    /// The per-session cap and the recurring minimum interval reject before anything is written,
    /// and a schedule of the wrong kind names the kind it needed.
    #[test]
    fn caps_and_kinds_are_enforced() {
        let (_dir, store) = store();
        let rejected =
            |when: ReplScheduleWhen| add(&store, "live-1", &when).unwrap_err().to_string();
        assert_eq!(
            rejected(ReplScheduleWhen::Every("1m".to_string())),
            "a recurring schedule must fire at most every 5 minutes"
        );
        assert_eq!(
            rejected(ReplScheduleWhen::Cron("* * * * *".to_string())),
            "a recurring schedule must fire at most every 5 minutes"
        );
        assert_eq!(
            rejected(ReplScheduleWhen::Cron("every 10m".to_string())),
            "schedule \"every 10m\" is not a five-field cron expression"
        );
        assert!(store.list().is_empty());
        add(
            &store,
            "live-1",
            &ReplScheduleWhen::Cron("*/5 * * * *".to_string()),
        )
        .unwrap();
        for _ in 1..MAX_REPL_SCHEDULES_PER_SESSION {
            add(&store, "live-1", &ReplScheduleWhen::Every("2h".to_string())).unwrap();
        }
        assert_eq!(
            rejected(ReplScheduleWhen::At("in 1h".to_string())),
            "this session already has 10 scheduled jobs (the maximum is 10); cancel one with \
             schedule.cancel(id) first"
        );
        // The cap is per session.
        add(&store, "live-2", &ReplScheduleWhen::At("in 1h".to_string())).unwrap();
    }

    /// Creating and cancelling broadcast the catalog change, so the supervisor's cached
    /// `cron_list` slice for this worker (what `prime-agent schedule list` reads) refreshes.
    #[test]
    fn create_and_cancel_broadcast_the_catalog_change() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        store.on_heartbeat_change(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let job = add(
            &store,
            "live-1",
            &ReplScheduleWhen::At("in 30m".to_string()),
        )
        .unwrap();
        assert_eq!(changes.load(Ordering::SeqCst), 1);
        store.cancel_repl_schedule("live-1", &job.id, NOW).unwrap();
        assert_eq!(changes.load(Ordering::SeqCst), 2);
    }
}
