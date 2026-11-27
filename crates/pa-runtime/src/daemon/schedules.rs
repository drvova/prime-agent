//! The runtime daemon's scheduled jobs and heartbeats ([`Schedules`]): the
//! kernel's `schedule.*` and `rlm_heartbeat.*` jobs of its sessions, kept in
//! each session's scheduled-jobs partition (pa-core's `cron` store, the
//! old daemon's rows), fired by the runtime's timer sources.
//!
//! - **Armed.** A session's jobs are armed when its engine is built (it
//!   binds its kernel to the session's store) or when the boot holds it
//!   dormant ([`super::held`]). Arming moves the stored jobs onto the
//!   session (`rebind_session_jobs`) and gives the session one dispatch
//!   lane: pa-core's `AgentCronScheduler` over its store, never started, so
//!   its own timer never runs; the lane runs a pass when a timer fires.
//! - **Timers.** Each active job has a kept timer source `schedule-<job>`
//!   (`{"at": <its next run>}`) and a bus subscription `schedule:<job>`
//!   that hears it and routes to the session; its fire runs a pass. After
//!   every pass, and every kernel mutation, the timers follow the store: a
//!   job's timer is defined again at its new next run, and a job no longer
//!   active loses its timer. The store's claim decides what runs
//!   (`claim_due`, its dispatch records, its interrupted-dispatch
//!   recovery), under its catch-up rule: a job whose time passed while
//!   nothing ran fires once, and its next run counts from that fire. A
//!   timer's own claim (`lastFiredMs`) is cleared whenever it is defined,
//!   so it never holds back a run the store owes. A pass that fails (the
//!   store's state lock a killed daemon left behind, until it is stale) is
//!   run again after a backoff: its timer has fired, and nothing else would.
//! - **Changes.** A job made, changed or cancelled (by the kernel, or by
//!   `schedule.add` and `schedule.cancel`) publishes `agent/<id>/schedules`
//!   (`{"active": <its active jobs>}`) at the lane's next pass: the old
//!   daemon's `heartbeats_changed`, which an ACP client reports.
//! - **Routing**, as the old daemon's: a plain job is a `follow_up` task
//!   over its prompt; a heartbeat steers its `heartbeat_prompt` row onto the
//!   session's agent (a running turn reads it at its next step) and queues
//!   an empty input that runs it when no turn does. Heartbeat deferral is
//!   pa-core's (`should_defer_heartbeat_cron_job`). One fire waits for its
//!   task's `done` before the lane's next, so a job never has two fires
//!   queued: the old daemon's `heartbeat:<id>` queue key.
//! - **Heartbeat commands** (8d-4c, pa-core's `cron::heartbeat_commands`):
//!   `heartbeat_get`, `heartbeat_set`, `heartbeat_update` and
//!   `heartbeat_manage` work on the session's store as the old worker's
//!   arms do; a manage of a saved session's job works on its partition's
//!   store (the old supervisor's passive path), and arms the session when it
//!   resumes one. Every change but a resume, and a kernel mutation that asks
//!   for it, withdraws the heartbeat's fire waiting on the session's agent
//!   (the old `heartbeat:<id>` queue key).
//! - **Waking.** A job due in a dormant session opens it under its lease
//!   first: the old daemon's scheduled wake. A session stopped on purpose is
//!   disarmed and not held, so nothing wakes it.
//! - **Subagents.** A subagent's jobs (its RLM heartbeats) are armed on the
//!   transcript its spawn writes; a due one runs in the child while the
//!   runtime runs it, and is skipped once it does not (a child is never
//!   woken as a session of its own).
//! - **One daemon.** A job runs only while this daemon is the selected one
//!   ([`super::DaemonOptions::selected`]); otherwise its fire is a skip, so
//!   jobs never fire in both daemons.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::Context as _;
use pa_core::cron::heartbeat_commands::{
    no_active_heartbeat, withdraws_queued_fire, HeartbeatSet, HeartbeatUpdate,
    HEARTBEATS_NEED_A_SESSION_FILE,
};
use pa_core::cron::scheduler::{
    AgentCronScheduler, AgentCronSchedulerHooks, FIRE_SETTLE_TIMEOUT_MS,
};
use pa_core::cron::store::{
    AgentCronJobStore, CancelJobsFilter, CreateAgentCronJobInput, HeartbeatManagementAction,
    SessionBinding,
};
use pa_core::cron::{
    is_heartbeat_cron_job, should_defer_heartbeat_cron_job, AgentCronJob, HeartbeatSessionActivity,
    JobStatus,
};
use pa_core::session::lease::acquire_runtime_session_lease;
use pa_core::session_engine::runtime_wiring::{KernelCronBinding, KernelCronWiring};
use pa_sources::SourceKind;
use pa_types::event_log::{
    AgentDone, AgentInput, AgentTarget, NewEvent, Priority, Route, Scope, Subscription, TaskState,
    Topic, TopicFilter, Trust,
};
use serde_json::json;
use tokio::sync::Notify;

use super::held::{OpenSession, OpenSessions};
use crate::bus::{Heard, Listener};
use crate::lock;
use crate::runtime::Runtime;
use crate::session::{SessionBuilder, SessionJobs, SessionSpec, SCHEDULE_PREFIX};
use crate::sources::{SourceDefinition, SourceLifetime};

/// What the scheduler records for a fire that did not run.
const SKIPPED: &str = "skipped";

/// The prefix of a job's timer source's name.
const TIMER_PREFIX: &str = "schedule-";

/// How long a lane waits to run a failed pass again, doubling with each
/// failure in a row up to [`FAILED_PASS_RETRY_CAP`]. A pass fails when the
/// store's state lock is not acquired: a daemon killed while it held it
/// leaves it behind, and it is taken over once stale (30 s).
const FAILED_PASS_RETRY: Duration = Duration::from_secs(1);
const FAILED_PASS_RETRY_CAP: Duration = Duration::from_secs(10);

/// Which jobs a listing shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobsShown {
    /// The active and paused ones.
    Live,
    /// Every one, the cancelled and finished too.
    All,
}

/// Whether this daemon is the selected one now.
pub(super) type Selected = Arc<dyn Fn() -> bool + Send + Sync>;

/// The runtime daemon's scheduled jobs, by session.
pub(super) struct Schedules<B: SessionBuilder> {
    me: Weak<Self>,
    runtime: Weak<Runtime<B>>,
    open: Arc<OpenSessions>,
    agent_dir: PathBuf,
    selected: Selected,
    /// Where the lanes run (arming is asked on a session's own thread).
    handle: tokio::runtime::Handle,
    armed: Mutex<HashMap<String, Arc<Armed>>>,
    /// The subagent sessions among them: a due job of one the runtime no
    /// longer runs is skipped, never woken as a session of its own.
    subagents: Mutex<HashSet<String>>,
}

/// One session's armed jobs.
struct Armed {
    store: Arc<AgentCronJobStore>,
    /// The session's transcript and working directory its jobs are bound
    /// to: the ones its engine was last built on.
    binding: Mutex<SessionBinding>,
    /// Runs one pass of the session's lane.
    pass: Arc<Notify>,
    /// The timer each job has, by job id: the next run it fires at.
    timers: tokio::sync::Mutex<HashMap<String, String>>,
    /// A job was made, changed or cancelled since the lane's last pass: the
    /// pass publishes `agent/<id>/schedules`.
    changed: Arc<AtomicBool>,
    lane: tokio::task::JoinHandle<()>,
}

impl Armed {
    /// What the session's jobs are bound to now.
    fn binding(&self) -> SessionBinding {
        lock(&self.binding).clone()
    }
}

/// How the session's lane runs a claimed job.
struct Fire<B: SessionBuilder> {
    schedules: Weak<Schedules<B>>,
    session: String,
}

impl<B: SessionBuilder> AgentCronSchedulerHooks for Fire<B> {
    async fn run_job(&self, job: &AgentCronJob) -> anyhow::Result<Option<&'static str>> {
        match self.schedules.upgrade() {
            Some(schedules) => schedules.run_job(&self.session, job).await,
            None => Ok(Some(SKIPPED)),
        }
    }
}

impl<B: SessionBuilder> Schedules<B> {
    /// The scheduled jobs of `runtime`'s sessions; must be called on the
    /// daemon's Tokio runtime, where the lanes run.
    pub(super) fn new(
        runtime: &Arc<Runtime<B>>,
        open: Arc<OpenSessions>,
        agent_dir: PathBuf,
        selected: Selected,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Schedules {
            me: me.clone(),
            runtime: Arc::downgrade(runtime),
            open,
            agent_dir,
            selected,
            handle: tokio::runtime::Handle::current(),
            armed: Mutex::new(HashMap::new()),
            subagents: Mutex::new(HashSet::new()),
        })
    }

    /// Arms the jobs of session `id` (transcript `path`, running in `cwd`),
    /// or answers them armed already.
    fn arm(&self, id: &str, path: &Path, cwd: &Path) -> anyhow::Result<Arc<Armed>> {
        let mut armed = lock(&self.armed);
        if let Some(session) = armed.get(id) {
            // A session that goes on from another transcript (a fork, an
            // import) is armed again as its engine is rebuilt: its jobs move
            // onto that transcript in place. Its lane and timers stay, so no
            // removal of a timer of its past arming can race the timers its
            // lane defines next.
            let bound = session.binding();
            let (file, cwd) = (
                path.to_string_lossy().into_owned(),
                cwd.to_string_lossy().into_owned(),
            );
            if bound.session_file != file || bound.cwd != cwd {
                let binding = SessionBinding {
                    session_file: file,
                    cwd,
                    ..bound
                };
                session.store.rebind_session_jobs(&binding)?;
                *lock(&session.binding) = binding;
            }
            return Ok(Arc::clone(session));
        }
        let partition = partition(path, id)?;
        std::fs::create_dir_all(&partition)?;
        let store = AgentCronJobStore::for_session_artifacts();
        store.register_session_artifact(id, &partition);
        let binding = SessionBinding {
            active_session_id: id.to_string(),
            session_id: id.to_string(),
            session_file: path.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
        };
        store.rebind_session_jobs(&binding)?;
        let store = Arc::new(store);
        let scheduler = AgentCronScheduler::new(
            Arc::clone(&store),
            Arc::new(Fire {
                schedules: self.me.clone(),
                session: id.to_string(),
            }),
        );
        let pass = Arc::new(Notify::new());
        // The first pass fires what is overdue and defines the timers.
        pass.notify_one();
        let lane = self.handle.spawn({
            let (pass, jobs, id) = (Arc::clone(&pass), self.me.clone(), id.to_string());
            async move {
                // After a pass that failed, how long until it is run again.
                let mut retry: Option<Duration> = None;
                loop {
                    match retry {
                        Some(delay) => {
                            tokio::select! {
                                () = pass.notified() => {}
                                () = tokio::time::sleep(delay) => {}
                            }
                        }
                        None => pass.notified().await,
                    }
                    retry = match scheduler.run_due().await {
                        Ok(_) => None,
                        // Nothing else runs the pass again: its timer has
                        // fired, and the job's next run did not move.
                        Err(error) => {
                            tracing::warn!("session {id}'s scheduled jobs did not run: {error:#}");
                            Some(retry.map_or(FAILED_PASS_RETRY, |delay| {
                                (delay * 2).min(FAILED_PASS_RETRY_CAP)
                            }))
                        }
                    };
                    let Some(jobs) = jobs.upgrade() else {
                        return;
                    };
                    jobs.follow_store(&id).await;
                }
            }
        });
        let session = Arc::new(Armed {
            store,
            binding: Mutex::new(binding),
            pass,
            timers: tokio::sync::Mutex::new(HashMap::new()),
            changed: Arc::new(AtomicBool::new(false)),
            lane,
        });
        armed.insert(id.to_string(), Arc::clone(&session));
        Ok(session)
    }

    /// Arms the jobs of the idle session `id` the boot holds down; whether
    /// it keeps an active one (one without is disarmed again).
    pub(super) fn arm_dormant(&self, id: &str, path: &Path, cwd: &Path) -> bool {
        let armed = match self.arm(id, path, cwd) {
            Ok(armed) => armed,
            Err(error) => {
                tracing::warn!("session {id}'s scheduled jobs were not armed: {error:#}");
                return false;
            }
        };
        let active = armed
            .store
            .list()
            .iter()
            .any(|job| job.status == JobStatus::Active);
        if !active {
            self.disarm(id);
        }
        active
    }

    /// Disarms session `id`'s jobs: its lane stops and its timers go. Its
    /// jobs stay stored, armed again when it is next opened.
    pub(super) fn disarm(&self, id: &str) {
        let Some(armed) = lock(&self.armed).remove(id) else {
            return;
        };
        armed.lane.abort();
        let runtime = self.runtime.clone();
        self.handle.spawn(async move {
            let Some(runtime) = runtime.upgrade() else {
                return;
            };
            for job in armed.timers.lock().await.keys() {
                remove_timer(&runtime, job).await;
            }
        });
    }

    /// The jobs of session `id` (every session the daemon holds open or
    /// dormant when `None`), soonest first; active and paused ones only
    /// unless `shown` is [`JobsShown::All`]. The client's `schedule list`.
    pub(super) fn jobs(&self, id: Option<&str>, shown: JobsShown) -> Vec<AgentCronJob> {
        let stores: Vec<Arc<AgentCronJobStore>> = lock(&self.armed)
            .iter()
            .filter(|(session, _)| id.is_none_or(|id| id == session.as_str()))
            .map(|(_, armed)| Arc::clone(&armed.store))
            .collect();
        let mut jobs: Vec<AgentCronJob> = stores
            .iter()
            .flat_map(|store| store.list())
            .filter(|job| match shown {
                JobsShown::Live => matches!(job.status, JobStatus::Active | JobStatus::Paused),
                JobsShown::All => true,
            })
            .collect();
        jobs.sort_by(|left, right| left.next_run_at.cmp(&right.next_run_at));
        jobs
    }

    /// Schedules `prompt` on open session `id` at `schedule` (the store's
    /// grammar), as the old daemon's `cron_add`; its timer follows at the
    /// lane's next pass.
    ///
    /// # Errors
    ///
    /// The session's jobs are not armed (it is not open), or the store
    /// refused the job (an empty prompt, a schedule it cannot read).
    pub(super) fn add(
        &self,
        id: &str,
        schedule: &str,
        prompt: &str,
    ) -> anyhow::Result<AgentCronJob> {
        let armed = lock(&self.armed)
            .get(id)
            .cloned()
            .with_context(|| format!("session {id} is not open in the runtime daemon"))?;
        let runtime_kind = self.runtime_kind(id);
        let binding = armed.binding();
        let job = armed.store.create(&CreateAgentCronJobInput {
            active_session_id: binding.active_session_id,
            session_id: binding.session_id,
            session_file: binding.session_file,
            cwd: binding.cwd,
            runtime_kind: Some(runtime_kind.to_string()),
            prompt: prompt.to_string(),
            schedule_text: schedule.to_string(),
            ..Default::default()
        })?;
        armed.changed.store(true, Ordering::Release);
        armed.pass.notify_one();
        Ok(job)
    }

    /// Cancels job `job` of whichever session the daemon holds keeps it, as
    /// the old daemon's `cron_cancel`; its timer goes at the lane's next
    /// pass.
    ///
    /// # Errors
    ///
    /// No held session keeps the job (`No cron job found: <job>`), or the
    /// store could not be written.
    pub(super) fn cancel(&self, job: &str) -> anyhow::Result<AgentCronJob> {
        let sessions: Vec<Arc<Armed>> = lock(&self.armed).values().cloned().collect();
        for armed in sessions {
            if let Some(cancelled) = armed.store.cancel(job, pa_core::autonomous::now_millis())? {
                armed.changed.store(true, Ordering::Release);
                armed.pass.notify_one();
                return Ok(cancelled);
            }
        }
        anyhow::bail!("No cron job found: {job}")
    }

    /// Session `id`'s armed jobs.
    fn armed(&self, id: &str) -> Option<Arc<Armed>> {
        lock(&self.armed).get(id).cloned()
    }

    /// A change to `armed`'s jobs: the lane's next pass moves their timers
    /// and tells the session's clients.
    fn changed(armed: &Armed) {
        armed.changed.store(true, Ordering::Release);
        armed.pass.notify_one();
    }

    /// Withdraws heartbeat `job`'s queued fire on session `id` when a change
    /// by `action` does (pa-core's `withdraws_queued_fire`).
    fn withdraw_fire(&self, id: &str, job: &AgentCronJob, action: HeartbeatManagementAction) {
        if !(withdraws_queued_fire(action) && is_heartbeat_cron_job(job)) {
            return;
        }
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.subagents().withdraw_heartbeat_fire(id, &job.id);
        }
    }

    /// `heartbeat_get`: open session `id`'s live or paused heartbeat (none
    /// for one that keeps no jobs).
    pub(super) fn heartbeat(&self, id: &str) -> Option<AgentCronJob> {
        self.armed(id)?.store.get_heartbeat(id)
    }

    /// `heartbeat_set`: replaces open session `id`'s heartbeat; the previous
    /// one's queued fire is withdrawn.
    ///
    /// # Errors
    ///
    /// The session keeps no transcript (`Heartbeats require a persisted
    /// session file`), or the store refused the heartbeat.
    pub(super) fn set_heartbeat(
        &self,
        id: &str,
        set: &HeartbeatSet,
    ) -> anyhow::Result<AgentCronJob> {
        let armed = self.armed(id).context(HEARTBEATS_NEED_A_SESSION_FILE)?;
        let previous = armed.store.get_heartbeat(id);
        let job = armed.store.create_heartbeat(&set.input(
            armed.binding(),
            self.runtime_kind(id),
            previous.as_ref(),
        ))?;
        if let Some(previous) = &previous {
            self.withdraw_fire(id, previous, HeartbeatManagementAction::Stop);
        }
        Self::changed(&armed);
        Ok(job)
    }

    /// `heartbeat_update`: pauses, resumes or clears open session `id`'s
    /// heartbeat; the job it changed (none for a session without one).
    ///
    /// # Errors
    ///
    /// The store could not be written.
    pub(super) fn update_heartbeat(
        &self,
        id: &str,
        update: HeartbeatUpdate,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let Some(armed) = self.armed(id) else {
            return Ok(None);
        };
        let job = update.apply(&armed.store, id, pa_core::autonomous::now_millis())?;
        if let Some(job) = &job {
            self.withdraw_fire(id, job, update.as_management());
        }
        Self::changed(&armed);
        Ok(job)
    }

    /// `heartbeat_manage`: pauses, resumes or stops heartbeat `job` of
    /// session `id`, open or saved at `path` (its partition's store, as the
    /// old supervisor managed a passive job; one resumed is armed so it
    /// fires).
    ///
    /// # Errors
    ///
    /// The session has no live heartbeat `job` (`No active heartbeat found:
    /// <job>`), or the store could not be written.
    pub(super) fn manage_heartbeat(
        &self,
        id: &str,
        path: &Path,
        job: &str,
        action: HeartbeatManagementAction,
    ) -> anyhow::Result<AgentCronJob> {
        let now = pa_core::autonomous::now_millis();
        let managed = if let Some(armed) = self.armed(id) {
            let managed = armed.store.manage_heartbeat(id, job, action, now);
            if matches!(managed, Ok(Some(_))) {
                Self::changed(&armed);
            }
            managed
        } else {
            let store = AgentCronJobStore::for_session_artifacts();
            store.register_session_artifact(id, &partition(path, id)?);
            let managed = store.manage_heartbeat(id, job, action, now);
            if let Ok(Some(job)) = &managed {
                if job.status == JobStatus::Active {
                    if let Some(header) = pa_core::session::manager::read_session_header(path) {
                        self.arm_dormant(id, path, Path::new(&header.cwd));
                    }
                }
            }
            managed
        };
        let managed = managed?.with_context(|| no_active_heartbeat(job))?;
        self.withdraw_fire(id, &managed, action);
        Ok(managed)
    }

    /// The kind a job of session `id` records (`subagent` or `top-level`).
    fn runtime_kind(&self, id: &str) -> &'static str {
        if lock(&self.subagents).contains(id) {
            "subagent"
        } else {
            "top-level"
        }
    }

    /// Stops every session's lane, for the daemon's shutdown; the jobs and
    /// their timers stay stored for the next daemon.
    pub(super) fn stop(&self) {
        for (_, armed) in lock(&self.armed).drain() {
            armed.lane.abort();
        }
    }

    /// Defines, moves and removes session `id`'s timers so each active job
    /// has one at its next run.
    async fn follow_store(&self, id: &str) {
        let Some(armed) = lock(&self.armed).get(id).cloned() else {
            return;
        };
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let wanted: HashMap<String, String> = armed
            .store
            .list()
            .into_iter()
            .filter(|job| job.status == JobStatus::Active)
            .filter_map(|job| Some((job.id, job.next_run_at?)))
            .collect();
        let changed = armed.changed.swap(false, Ordering::AcqRel);
        let mut timers = armed.timers.lock().await;
        let moved: Vec<String> = timers
            .iter()
            .filter(|(job, at)| wanted.get(*job) != Some(*at))
            .map(|(job, _)| job.clone())
            .collect();
        for job in moved {
            remove_timer(&runtime, &job).await;
            timers.remove(&job);
        }
        for (job, at) in wanted {
            if timers.contains_key(&job) {
                continue;
            }
            match add_timer(&runtime, id, &job, &at).await {
                Ok(()) => {
                    timers.insert(job, at);
                }
                Err(error) => tracing::warn!("job {job}'s timer was not defined: {error:#}"),
            }
        }
        if changed {
            // With its timers in place, the clients following the session
            // learn its jobs changed (the old daemon's `heartbeats_changed`).
            let changed = async {
                runtime
                    .publish(NewEvent {
                        topic: Topic::new(format!("agent/{id}/schedules"))?,
                        source: "schedules".to_string(),
                        session: Some(id.to_string()),
                        payload: json!({ "active": timers.len() }),
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await
            };
            if let Err(error) = changed.await {
                tracing::warn!("session {id}'s schedule change was not published: {error:#}");
            }
        }
    }

    /// Runs one claimed `job` of `session`: wakes the session when it is
    /// down, queues the job's task as the old daemon routed it, and waits
    /// for its `done`.
    async fn run_job(
        &self,
        session: &str,
        job: &AgentCronJob,
    ) -> anyhow::Result<Option<&'static str>> {
        if !(self.selected)() {
            return Ok(Some(SKIPPED));
        }
        let Some(armed) = lock(&self.armed).get(session).cloned() else {
            return Ok(Some(SKIPPED));
        };
        let Some(runtime) = self.runtime.upgrade() else {
            return Ok(Some(SKIPPED));
        };
        // A job of a session that no longer exists cancels the session's
        // jobs: a fire never brings a deleted session back.
        let binding = armed.binding();
        let path = PathBuf::from(&binding.session_file);
        if !pa_core::session::store::store().exists(&path) {
            armed.store.cancel_jobs_for_session(
                &CancelJobsFilter {
                    active_session_id: None,
                    session_id: None,
                    session_file: Some(binding.session_file.clone()),
                },
                pa_core::autonomous::now_millis(),
            )?;
            return Ok(Some(SKIPPED));
        }
        // A session the runtime runs (a subagent among them) takes its job
        // where it runs; a subagent it no longer runs is not woken.
        if runtime.session_engine(session).is_none() {
            if lock(&self.subagents).contains(session) {
                return Ok(Some(SKIPPED));
            }
            self.wake(&runtime, session, &path, Path::new(&binding.cwd))
                .await?;
        }
        let lane = runtime.lane(session);
        let activity = HeartbeatSessionActivity {
            is_streaming: lane.running.is_some(),
            has_pending_session_work: !lane.queued.is_empty(),
            unfinished_action_count: lane.queued.len(),
            ..HeartbeatSessionActivity::default()
        };
        if should_defer_heartbeat_cron_job(job, &activity) {
            return Ok(Some(SKIPPED));
        }
        let mut listener = runtime.listen();
        let input = if is_heartbeat_cron_job(job) {
            let row = pa_core::session_engine::messages::create_heartbeat_prompt_message(
                job,
                pa_core::autonomous::now_millis(),
            );
            runtime.deliver_row(session, &row).await?;
            AgentInput {
                text: String::new(),
                images: Vec::new(),
                priority: None,
            }
        } else {
            AgentInput {
                text: job.prompt.clone(),
                images: Vec::new(),
                priority: Some(Priority::FollowUp),
            }
        };
        let published = runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{session}/input"))?,
                source: format!("{TIMER_PREFIX}{}", job.id),
                session: Some(session.to_string()),
                payload: serde_json::to_value(input)?,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await?;
        let task = crate::wire::accepted(&published, session)
            .context("the job's task was not accepted")?
            .task;
        match tokio::time::timeout(
            Duration::from_millis(FIRE_SETTLE_TIMEOUT_MS),
            done_of(&runtime, &mut listener, session, &task),
        )
        .await
        {
            // A failed turn counts as a run (the store's failure backoff
            // stretches the next one); a cancelled one is a clean run.
            Ok(Some(AgentDone {
                outcome: TaskState::Failed,
                error,
                ..
            })) => Err(anyhow::anyhow!(
                error.unwrap_or_else(|| "the job's turn failed".to_string())
            )),
            Ok(Some(_) | None) => Ok(None),
            // The settle window expired: the fire did not run.
            Err(_) => Ok(Some(SKIPPED)),
        }
    }

    /// Opens session `id` under its lease when the daemon does not hold it
    /// open: the scheduled wake.
    async fn wake(
        &self,
        runtime: &Runtime<B>,
        id: &str,
        path: &Path,
        cwd: &Path,
    ) -> anyhow::Result<()> {
        if self.open.contains(id) {
            return Ok(());
        }
        let lease = {
            let (path, agent_dir) = (path.to_path_buf(), self.agent_dir.clone());
            tokio::task::spawn_blocking(move || acquire_runtime_session_lease(&path, &agent_dir))
                .await
                .context("the lease's thread panicked")??
        };
        runtime
            .create_session(SessionSpec {
                id: Some(id.to_string()),
                name: None,
                cwd: cwd.to_path_buf(),
                env: super::session_env(&self.agent_dir, id),
                subagent: None,
                transcript: Some(path.to_path_buf()),
                options: super::session_options(runtime.log(), id),
                ephemeral: false,
            })
            .await?;
        self.open
            .insert(
                id.to_string(),
                OpenSession {
                    path: path.to_path_buf(),
                    cwd: cwd.to_path_buf(),
                    lease: Some(lease),
                    owner: None,
                    scoped_models: Vec::new(),
                },
            )
            .await;
        tracing::info!("session {id} has scheduled work due: woken to run it");
        Ok(())
    }
}

impl<B: SessionBuilder> SessionJobs for Schedules<B> {
    fn wiring(&self, id: &str, spec: &SessionSpec) -> Option<KernelCronWiring> {
        // A subagent's transcript is the one its spawn writes under its
        // parent's artifacts (a parent kept in memory gives it none, and it
        // keeps no jobs).
        let path = match &spec.subagent {
            Some(subagent) => {
                subagent.parent_session_file.as_ref()?;
                lock(&self.subagents).insert(id.to_string());
                spec.transcript.clone().unwrap_or_else(|| {
                    pa_core::session::manager::get_session_file_path(
                        &self
                            .agent_dir
                            .join("session-artifacts")
                            .join(&subagent.parent)
                            .join(&subagent.child_id),
                        id,
                    )
                })
            }
            None => spec.transcript.clone()?,
        };
        let armed = match self.arm(id, &path, &spec.cwd) {
            Ok(armed) => armed,
            Err(error) => {
                tracing::warn!("session {id}'s scheduled jobs were not armed: {error:#}");
                return None;
            }
        };
        let (pass, changed) = (Arc::clone(&armed.pass), Arc::clone(&armed.changed));
        let (runtime, session) = (self.runtime.clone(), id.to_string());
        let binding = armed.binding();
        Some(KernelCronWiring {
            store: Arc::clone(&armed.store),
            binding: Some(KernelCronBinding {
                active_session_id: binding.active_session_id,
                session_id: binding.session_id,
                session_file: binding.session_file,
                cwd: binding.cwd,
            }),
            // A job made, changed or cancelled: the lane's next pass moves
            // its timer, and a heartbeat's change withdraws its queued fire.
            mutation_hook: Some(Arc::new(move |mutation| {
                if mutation.drop_queued && is_heartbeat_cron_job(&mutation.job) {
                    if let Some(runtime) = runtime.upgrade() {
                        runtime
                            .subagents()
                            .withdraw_heartbeat_fire(&session, &mutation.job.id);
                    }
                }
                changed.store(true, Ordering::Release);
                pass.notify_one();
                Box::pin(std::future::ready(()))
            })),
        })
    }

    fn fired(&self, session: &str) {
        if let Some(armed) = lock(&self.armed).get(session) {
            armed.pass.notify_one();
        }
    }
}

/// The scheduled-jobs partition of session `id` (transcript `path`): the old
/// daemon's `<sessions>/../session-artifacts/<id>`.
fn partition(path: &Path, id: &str) -> anyhow::Result<PathBuf> {
    path.parent()
        .and_then(Path::parent)
        .map(|root| root.join("session-artifacts").join(id))
        .with_context(|| format!("{} is in no sessions dir", path.display()))
}

/// Defines job `job`'s timer at `at` for session `session`, with the
/// subscription that hears it.
async fn add_timer<B: SessionBuilder>(
    runtime: &Runtime<B>,
    session: &str,
    job: &str,
    at: &str,
) -> anyhow::Result<()> {
    let name = format!("{TIMER_PREFIX}{job}");
    // A definition left by an earlier daemon goes first, with its claim.
    runtime.undefine_source(&name).await?;
    runtime
        .define_source(SourceDefinition {
            name: name.clone(),
            kind: SourceKind::Timer,
            options: json!({ "at": at }),
            owner: Some(session.to_string()),
            lifetime: SourceLifetime::Kept,
        })
        .await?;
    let subscription = format!("{SCHEDULE_PREFIX}{job}");
    let _ = runtime.remove_subscription(&subscription);
    runtime.add_subscription(Subscription {
        id: subscription,
        scope: Scope::Global,
        filter: TopicFilter::new(format!("timer/{name}"))?,
        when: None,
        throttle_ms: 0,
        debounce_ms: 0,
        route: Route::Agent {
            target: AgentTarget::Session {
                session: session.to_string(),
            },
            prompt: String::new(),
            priority: Priority::Background,
        },
        trust: Trust::Owner,
    })?;
    Ok(())
}

/// Removes job `job`'s timer and its subscription.
async fn remove_timer<B: SessionBuilder>(runtime: &Runtime<B>, job: &str) {
    let _ = runtime.remove_subscription(&format!("{SCHEDULE_PREFIX}{job}"));
    if let Err(error) = runtime
        .undefine_source(&format!("{TIMER_PREFIX}{job}"))
        .await
    {
        tracing::warn!("job {job}'s timer was not removed: {error:#}");
    }
}

/// The `done` of task `task` of `session`, as `listener` hears it; `None`
/// when the listener fell behind (the session's lane is waited out
/// instead) or the bus closed.
async fn done_of<B: SessionBuilder>(
    runtime: &Runtime<B>,
    listener: &mut Listener,
    session: &str,
    task: &str,
) -> Option<AgentDone> {
    let done = Topic::new(format!("agent/{session}/done")).ok()?;
    loop {
        match listener.recv().await? {
            Heard::Event(event) if event.event.topic == done => {
                let Ok(ended) = serde_json::from_value::<AgentDone>(event.event.payload.clone())
                else {
                    continue;
                };
                if ended.task == task {
                    return Some(ended);
                }
            }
            Heard::Event(_) => {}
            Heard::Lagged { .. } => {
                runtime.idle(session).await;
                return None;
            }
        }
    }
}
