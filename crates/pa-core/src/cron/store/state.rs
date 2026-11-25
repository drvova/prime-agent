//! State save/load for the cron job store: cross-process file locking
//! (lockfile with stale takeover), state read/write/merge helpers, and the
//! in-state due-claim / interrupted-dispatch recovery transitions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{
    iso_from_millis, AgentCronDispatch, AgentCronDispatchRecord, AgentCronJob, AgentCronJobStore,
    CronJobsState,
};
use crate::agent_state::StateFile;
use crate::cron::{
    is_due_job, next_run_at_for_schedule, parse_iso_millis, JobStatus, ScheduleKind,
};

const LOCK_STALE_MS: u64 = 30_000;

impl AgentCronJobStore {
    pub(crate) fn read_jobs(&self) -> Vec<AgentCronJob> {
        self.read_states()
            .into_iter()
            .flat_map(|state| state.jobs)
            .collect()
    }

    pub(crate) fn read_states(&self) -> Vec<CronJobsState> {
        if self.session_artifact_mode {
            let files = self
                .session_artifact_files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return files.values().map(|path| read_jobs_state(path)).collect();
        }
        vec![read_jobs_state(&self.require_file_path())]
    }

    pub(crate) fn mutate_states(
        &self,
        mut mutator: impl FnMut(&mut CronJobsState) -> Vec<AgentCronDispatch>,
    ) -> anyhow::Result<Vec<AgentCronDispatch>> {
        let paths: Vec<PathBuf> = if self.session_artifact_mode {
            self.session_artifact_files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect()
        } else {
            vec![self.require_file_path()]
        };
        let previous_heartbeats = Self::heartbeat_catalog_signature(&self.read_jobs());
        let mut changed = false;
        let dispatches = with_state_locks(&paths, || {
            let mut dispatches = Vec::new();
            for path in &paths {
                let mut state = load_jobs_state(path)?;
                let before = serde_json::to_string(&state).unwrap_or_default();
                dispatches.extend(mutator(&mut state));
                if serde_json::to_string(&state).unwrap_or_default() != before {
                    write_jobs_state(path, &state)?;
                    changed = true;
                }
            }
            Ok::<_, anyhow::Error>(dispatches)
        })
        .ok_or_else(|| anyhow::anyhow!("cron jobs state lock not acquired; skipped"))??;
        if changed && Self::heartbeat_catalog_signature(&self.read_jobs()) != previous_heartbeats {
            self.notify_heartbeat_change();
        }
        Ok(dispatches)
    }

    pub(crate) fn write_jobs(&self, jobs: &[AgentCronJob]) -> anyhow::Result<()> {
        let previous_heartbeats = Self::heartbeat_catalog_signature(&self.read_jobs());
        if self.session_artifact_mode {
            self.write_jobs_session_artifacts(jobs)?;
        } else {
            let path = self.require_file_path();
            with_state_locks(std::slice::from_ref(&path), || {
                write_jobs_file(&path, jobs, true)
            })
            .ok_or_else(|| anyhow::anyhow!("cron jobs state lock not acquired; skipped"))??;
        }
        if Self::heartbeat_catalog_signature(&self.read_jobs()) != previous_heartbeats {
            self.notify_heartbeat_change();
        }
        Ok(())
    }

    pub(crate) fn require_file_path(&self) -> PathBuf {
        self.file_path
            .clone()
            .expect("Cron job store requires a file path")
    }
}

pub(crate) fn resolve_path(path: &str) -> String {
    let clean = std::path::Path::new(path);
    let mut components: Vec<std::ffi::OsString> = Vec::new();
    for component in clean.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            other => components.push(other.as_os_str().to_os_string()),
        }
    }
    let mut resolved = PathBuf::from("/");
    for component in components {
        resolved.push(component);
    }
    resolved.to_string_lossy().to_string()
}

pub(crate) fn compare_optional_iso(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (left, right) if left == right => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(left), Some(right)) => parse_iso_millis(left)
            .unwrap_or(0)
            .cmp(&parse_iso_millis(right).unwrap_or(0)),
    }
}

pub(crate) fn normalize_optional_label(label: Option<&str>) -> Option<String> {
    let trimmed = label.map(str::trim).filter(|label| !label.is_empty());
    trimmed.map(std::string::ToString::to_string)
}

pub(crate) fn merge_fresh_jobs(
    current_jobs: Vec<AgentCronJob>,
    next_jobs: Vec<AgentCronJob>,
) -> Vec<AgentCronJob> {
    let mut merged: HashMap<String, AgentCronJob> = HashMap::new();
    for job in current_jobs {
        merged.insert(job.id.clone(), job);
    }
    for job in next_jobs {
        let is_fresh = merged
            .get(&job.id)
            .is_none_or(|current| is_at_least_as_fresh(&job, current));
        if is_fresh {
            merged.insert(job.id.clone(), job);
        }
    }
    merged.into_values().collect()
}

fn is_at_least_as_fresh(candidate: &AgentCronJob, current: &AgentCronJob) -> bool {
    let Some(current_time) = parse_iso_millis(&current.updated_at) else {
        return true;
    };
    let Some(candidate_time) = parse_iso_millis(&candidate.updated_at) else {
        return false;
    };
    candidate_time >= current_time
}

pub(crate) fn claim_due_in_state(
    state: &mut CronJobsState,
    due_at: u64,
    claimed_at: u64,
) -> Vec<AgentCronDispatch> {
    let claimed_iso = iso_from_millis(claimed_at);
    let claimed_job_ids: std::collections::HashSet<String> = state
        .dispatches
        .iter()
        .map(|dispatch| dispatch.job_id.clone())
        .collect();
    let mut dispatches = Vec::new();
    let jobs = std::mem::take(&mut state.jobs);
    let mut new_jobs = Vec::with_capacity(jobs.len());
    for mut job in jobs {
        if !is_due_job(&job, due_at) {
            new_jobs.push(job);
            continue;
        }
        let scheduled_for = job.next_run_at.clone().unwrap_or_default();
        let next_run_at = next_run_at_for_schedule(&job.schedule, claimed_at)
            .ok()
            .flatten()
            .map(iso_from_millis);
        job.next_run_at = next_run_at;
        job.updated_at.clone_from(&claimed_iso);
        if claimed_job_ids.contains(&job.id) {
            job.last_skipped_at = Some(claimed_iso.clone());
            new_jobs.push(job);
            continue;
        }
        let dispatch = AgentCronDispatchRecord {
            id: Uuid::new_v4().to_string(),
            job_id: job.id.clone(),
            claimed_at: claimed_iso.clone(),
            scheduled_for,
        };
        state.dispatches.push(dispatch.clone());
        dispatches.push(AgentCronDispatch {
            id: dispatch.id,
            job: job.clone(),
        });
        new_jobs.push(job);
    }
    state.jobs = new_jobs;
    dispatches
}

pub(crate) fn recover_interrupted_in_state(
    state: &mut CronJobsState,
    now: u64,
    recovered: &mut Vec<AgentCronJob>,
    dispatch_ids: Option<&std::collections::HashSet<String>>,
) {
    let interrupted: Vec<AgentCronDispatchRecord> = match dispatch_ids {
        Some(ids) => state
            .dispatches
            .iter()
            .filter(|dispatch| ids.contains(&dispatch.id))
            .cloned()
            .collect(),
        None => state.dispatches.clone(),
    };
    if interrupted.is_empty() {
        return;
    }
    let interrupted_ids: std::collections::HashSet<String> = interrupted
        .iter()
        .map(|dispatch| dispatch.job_id.clone())
        .collect();
    match dispatch_ids {
        Some(ids) => state
            .dispatches
            .retain(|dispatch| !ids.contains(&dispatch.id)),
        None => state.dispatches.clear(),
    }
    state.jobs = state
        .jobs
        .clone()
        .into_iter()
        .map(|mut job| {
            if !interrupted_ids.contains(&job.id) || job.status != JobStatus::Active {
                return job;
            }
            job.status = if job.schedule.kind == ScheduleKind::Once {
                JobStatus::Completed
            } else {
                job.status
            };
            job.last_error = Some("Interrupted before scheduled operation completion".to_string());
            job.updated_at = iso_from_millis(now);
            recovered.push(job.clone());
            job
        })
        .collect();
}

/// Cross-process state locks: lockfile with stale takeover, sorted by path.
pub(crate) fn with_state_locks<T>(paths: &[PathBuf], action: impl FnOnce() -> T) -> Option<T> {
    let mut unique: Vec<PathBuf> = paths.to_vec();
    unique.sort();
    unique.dedup();
    let mut guards: Vec<crate::platform::lock_dir::LockDir> = Vec::new();
    for path in &unique {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // TS `withCronJobsStateLocks`: proper-lockfile, 100 attempts x
        // 10ms, 30s staleness takeover.
        let stale = std::time::Duration::from_millis(LOCK_STALE_MS);
        match crate::platform::lock_dir::LockDir::acquire_retrying(
            path,
            stale,
            100,
            std::time::Duration::from_millis(10),
        ) {
            Ok(guard) => guards.push(guard),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "cron jobs state lock not acquired; skipped"
                );
                return None;
            }
        }
    }
    let result = action();
    drop(guards);
    Some(result)
}

/// The jobs state of the file at `path`, for a read that degrades: a state
/// that cannot be read reads as no jobs, as an unreadable file always did.
pub(crate) fn read_jobs_state(path: &Path) -> CronJobsState {
    load_jobs_state(path).unwrap_or_else(|error| {
        tracing::warn!(
            error = %format!("{error:#}"),
            path = %path.display(),
            "cron jobs state unreadable; read as empty"
        );
        CronJobsState::default()
    })
}

/// The jobs state of the file at `path`, for a change: from the process's
/// database once it holds it, else from the file (a missing or unparseable
/// file is no jobs).
///
/// # Errors
///
/// The database could not be read.
pub(crate) fn load_jobs_state(path: &Path) -> anyhow::Result<CronJobsState> {
    load_jobs_state_in(crate::agent_state::process_log(), path)
}

/// [`load_jobs_state`] from `log`.
fn load_jobs_state_in(log: Option<&pa_log::Log>, path: &Path) -> anyhow::Result<CronJobsState> {
    if let Some(log) = log {
        let scope = StateFile::ScheduledJobs.scope(path);
        if log.partition(&scope)?.is_some() {
            let mut state = CronJobsState::default();
            for row in log.sources(&scope)?.sources {
                // A row this build cannot read is skipped, as a file's is.
                let Ok(job) = serde_json::from_str::<AgentCronJob>(&row.spec) else {
                    continue;
                };
                if let Some(claim) = row.claim {
                    if let Ok(dispatch) = serde_json::from_str(&claim) {
                        state.dispatches.push(dispatch);
                    }
                }
                state.jobs.push(job);
            }
            return Ok(state);
        }
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(CronJobsState::default());
    };
    Ok(serde_json::from_str::<serde_json::Value>(&raw)
        .map(|parsed| jobs_state_from(&parsed))
        .unwrap_or_default())
}

/// [`read_jobs_state`] from `log` (another process's database).
pub(crate) fn read_jobs_state_in(log: Option<&pa_log::Log>, path: &Path) -> CronJobsState {
    load_jobs_state_in(log, path).unwrap_or_default()
}

/// The jobs state a parsed `scheduled-jobs.json` holds: its jobs and
/// dispatches that parse (others are skipped).
pub(crate) fn jobs_state_from(parsed: &serde_json::Value) -> CronJobsState {
    CronJobsState {
        jobs: parsed
            .get("jobs")
            .and_then(|jobs| jobs.as_array())
            .map(|jobs| {
                jobs.iter()
                    .filter_map(|job| serde_json::from_value::<AgentCronJob>(job.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        dispatches: parsed
            .get("dispatches")
            .and_then(|dispatches| dispatches.as_array())
            .map(|dispatches| {
                dispatches
                    .iter()
                    .filter_map(|dispatch| {
                        serde_json::from_value::<AgentCronDispatchRecord>(dispatch.clone()).ok()
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The rows that keep `state`: one timer source per job, in order, holding
/// the job's claimed dispatch. A dispatch of no job in the state is not
/// kept: claiming and recovery only ever act on a dispatch through its job.
///
/// # Errors
///
/// Two jobs share an id, a job has two dispatches, or a row does not
/// serialize.
pub(crate) fn jobs_state_rows(state: &CronJobsState) -> anyhow::Result<pa_log::SourceSet> {
    let mut claims: HashMap<&str, &AgentCronDispatchRecord> = HashMap::new();
    for dispatch in &state.dispatches {
        if claims.insert(&dispatch.job_id, dispatch).is_some() {
            anyhow::bail!("job {} has two dispatches in flight", dispatch.job_id);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut sources = Vec::with_capacity(state.jobs.len());
    for job in &state.jobs {
        if !seen.insert(job.id.as_str()) {
            anyhow::bail!("two jobs have the id {}", job.id);
        }
        sources.push(pa_log::SourceRow {
            id: job.id.clone(),
            kind: pa_log::SourceKind::Timer,
            spec: serde_json::to_string(job)?,
            claim: claims
                .get(job.id.as_str())
                .map(serde_json::to_string)
                .transpose()?,
        });
    }
    Ok(pa_log::SourceSet {
        sources,
        subscriptions: Vec::new(),
    })
}

fn write_jobs_file(path: &Path, jobs: &[AgentCronJob], merge_current: bool) -> anyhow::Result<()> {
    let current = load_jobs_state(path)?;
    let jobs = if merge_current {
        merge_fresh_jobs(current.jobs, jobs.to_vec())
    } else {
        jobs.to_vec()
    };
    write_jobs_state(
        path,
        &CronJobsState {
            jobs,
            dispatches: current.dispatches,
        },
    )
}

/// Replace the jobs state of the file at `path`: in the process's database
/// when it keeps one, else in the file.
///
/// # Errors
///
/// The write did not commit, or the file could not be written.
pub(crate) fn write_jobs_state(path: &Path, state: &CronJobsState) -> anyhow::Result<()> {
    match crate::agent_state::process_log() {
        Some(log) => {
            log.replace_sources(
                &StateFile::ScheduledJobs.scope(path),
                jobs_state_rows(state)?,
                pa_log::Origin::Written,
            )
            .wait_blocking()?;
            Ok(())
        }
        None => write_jobs_state_file(path, state),
    }
}

/// [`write_jobs_state`] to the file (the file transcript store).
fn write_jobs_state_file(path: &Path, state: &CronJobsState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(state)?;
    // Cron state opts into the shared durable write (TS `writeJobsState`
    // passes `{ mode: 0o600, fsync: true }`).
    crate::settings::storage::atomic_write_with(
        path,
        &format!("{serialized}\n"),
        crate::settings::storage::AtomicWriteOptions { fsync: true },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_state_write_propagates_physical_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::create_dir(&path).unwrap();
        assert!(write_jobs_state_file(&path, &CronJobsState::default()).is_err());
    }

    /// The cron state write opts in (the TS test pins `{ fsync: true, mode: 0o600 }`)
    /// `to_string_pretty(state) + "\n"` bytes.
    #[test]
    fn jobs_state_write_keeps_exactly_one_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let state = CronJobsState::default();
        let expected = format!("{}\n", serde_json::to_string_pretty(&state).unwrap());
        let before = crate::settings::storage::opt_in_fsync_calls();
        write_jobs_state_file(&path, &state).unwrap();
        assert_eq!(
            crate::settings::storage::opt_in_fsync_calls(),
            before + 1,
            "the cron state write must keep its one opt-in fsync"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        #[cfg(unix)]
        assert_eq!(
            crate::platform::perms::file_mode(&path),
            Some(0o600),
            "the durable write keeps the private mode"
        );
    }
}
