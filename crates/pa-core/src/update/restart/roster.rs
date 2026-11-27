//! The update roster's assembly (spec §8), the parts both daemons share: the
//! durable snapshot of sessions, workers, subagents and heartbeats written at
//! `Snapshotted`, fsynced before the `Prepared` ack. Each daemon builds its
//! session (and, the supervisor, worker) rows from what it hosts; the
//! subagent rows come from the spawn ledger and the heartbeat rows from the
//! scheduled jobs, the same for both.
//!
//! The roster is a *projection*: the durable truth stays where it lives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pa_types::daemon::{
    UpdateHeartbeatDeliveryMode, UpdateHeartbeatStatus, UpdateId, UpdateRoster, UpdateRosterBinary,
    UpdateRosterHeartbeat, UpdateRosterSession, UpdateRosterSubagent, UpdateRosterSubagentStatus,
    UpdateRosterWorker, UpdateSupervisorIdentity, UPDATE_ROSTER_FORMAT_VERSION,
};
use serde_json::Map;

use crate::cron::store::read_scheduled_jobs_artifact;
use crate::session::lease::canonical_session_path;
use crate::session::manager::iso_from_unix_ms;
use crate::session_engine::rlm_ledger::{read_rlm_subagent_display, RlmSpawnLedger};

/// This process's identity, recorded into the roster and the prepared
/// marker (pid + start id + `generation`, the TS `getProcessStartId`
/// contract).
#[must_use]
pub fn process_identity(generation: String) -> UpdateSupervisorIdentity {
    let pid = std::process::id();
    UpdateSupervisorIdentity {
        pid: u64::from(pid),
        process_start_id: crate::session::hosted::process_start_id(pid),
        generation,
    }
}

/// A transcript path's file stem (a session's durable id).
#[must_use]
pub fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map_or_default(|stem| stem.to_string_lossy().to_string())
}

/// The canonical transcripts of the sessions the ledger's live edges are
/// children of: a session row's `rlm_children`.
///
/// # Errors
///
/// The ledger could not be read.
pub fn live_child_parents(ledger: &RlmSpawnLedger) -> Result<HashSet<PathBuf>> {
    Ok(ledger
        .live_edges()?
        .into_iter()
        .map(|edge| canonical_session_path(Path::new(&edge.parent)))
        .collect())
}

/// Subagent rows (spec §8): the ledger's live edges are the topology; the
/// per-child display file carries the durable status. A child whose display
/// is absent reports `running` when its session is resident, `completed`
/// otherwise (the TS passive-hydration split).
fn subagent_rows<S: std::hash::BuildHasher>(
    agent_dir: &Path,
    ledger: &RlmSpawnLedger,
    resident_files: &HashSet<PathBuf, S>,
) -> Result<Vec<UpdateRosterSubagent>> {
    let edges = ledger
        .live_edges()
        .with_context(|| "read the RLM spawn ledger for the update roster")?;
    let mut rows = Vec::new();
    for edge in edges {
        let parent_stem = file_stem(&edge.parent);
        let child_dir = agent_dir
            .join("session-artifacts")
            .join(&parent_stem)
            .join(&edge.child_id);
        let display = read_rlm_subagent_display(&child_dir);
        let status = match display.as_ref().map(|entry| entry.status.as_str()) {
            Some("completed") => UpdateRosterSubagentStatus::Completed,
            Some("running") => UpdateRosterSubagentStatus::Running,
            _ => {
                if resident_files.contains(&canonical_session_path(Path::new(&edge.child))) {
                    UpdateRosterSubagentStatus::Running
                } else {
                    UpdateRosterSubagentStatus::Completed
                }
            }
        };
        let session_file = display
            .as_ref()
            .map(|entry| entry.session_file.clone())
            .filter(|file| !file.is_empty())
            .unwrap_or_else(|| edge.child.clone());
        rows.push(UpdateRosterSubagent {
            child_id: edge.child_id.clone(),
            session_id: file_stem(&session_file),
            parent_session_id: parent_stem,
            name: edge.name.clone(),
            status,
            depth: edge.depth,
            session_file,
            display_file: child_dir
                .join("rlm-subagent.json")
                .to_string_lossy()
                .to_string(),
            rest: Map::default(),
        });
    }
    Ok(rows)
}

/// Heartbeat rows (spec §8): a read-only projection of every
/// `scheduled-jobs.json`; only `active` and `paused` jobs project.
fn heartbeat_rows(agent_dir: &Path) -> Vec<UpdateRosterHeartbeat> {
    let mut rows = Vec::new();
    for job in scan_scheduled_jobs(agent_dir) {
        let status = match job.status {
            crate::cron::JobStatus::Active => UpdateHeartbeatStatus::Active,
            crate::cron::JobStatus::Paused => UpdateHeartbeatStatus::Paused,
            crate::cron::JobStatus::Completed | crate::cron::JobStatus::Cancelled => continue,
        };
        let schedule = schedule_text(&job);
        rows.push(UpdateRosterHeartbeat {
            job_id: job.id,
            session_id: job.session_id,
            label: job.label,
            schedule,
            delivery_mode: match job.delivery_mode {
                Some(crate::cron::DeliveryMode::FollowUp) => UpdateHeartbeatDeliveryMode::FollowUp,
                Some(crate::cron::DeliveryMode::Steer) | None => UpdateHeartbeatDeliveryMode::Steer,
            },
            status,
            next_run_at: job.next_run_at,
            rest: Map::default(),
        });
    }
    rows
}

/// Every scheduled job in the agent's session artifacts (spec §8:
/// `scheduled-jobs.json` is the only write path). Shared by the roster
/// projection and the boot re-arm pass.
#[must_use]
pub fn scan_scheduled_jobs(agent_dir: &Path) -> Vec<crate::cron::AgentCronJob> {
    let artifacts_root = agent_dir.join("session-artifacts");
    let Ok(entries) = std::fs::read_dir(&artifacts_root) else {
        return Vec::new();
    };
    let mut jobs = Vec::new();
    for partition in entries.flatten() {
        for path in scheduled_job_files(&partition.path()) {
            jobs.extend(read_scheduled_jobs_artifact(&path));
        }
    }
    jobs
}

/// The `scheduled-jobs.json` files one level under an artifacts partition
/// root entry (per-session and per-child partitions).
fn scheduled_job_files(partition: &Path) -> Vec<PathBuf> {
    let mut paths = vec![partition.join("scheduled-jobs.json")];
    if let Ok(children) = std::fs::read_dir(partition) {
        for child in children.flatten() {
            paths.push(child.path().join("scheduled-jobs.json"));
        }
    }
    paths
        .into_iter()
        .filter(|path| crate::cron::store::scheduled_jobs_kept(path))
        .collect()
}

/// The schedule's display text (a UX projection; the durable schedule
/// object stays in `scheduled-jobs.json`).
fn schedule_text(job: &crate::cron::AgentCronJob) -> String {
    match job.schedule.kind {
        crate::cron::ScheduleKind::Cron | crate::cron::ScheduleKind::Once => {
            if job.schedule.expression.is_empty() {
                "once".to_string()
            } else {
                job.schedule.expression.clone()
            }
        }
        crate::cron::ScheduleKind::Interval => {
            let millis = job.schedule.interval_ms.unwrap_or(0);
            if millis >= 60_000 && millis.is_multiple_of(60_000) {
                format!("every {}m", millis / 60_000)
            } else if millis >= 1000 && millis.is_multiple_of(1000) {
                format!("every {}s", millis / 1000)
            } else {
                format!("every {millis}ms")
            }
        }
    }
}

/// The static inputs of one roster assembly: everything the daemon knows
/// besides its session rows.
pub struct RosterInputs<'a> {
    pub update_id: &'a UpdateId,
    pub socket_path: &'a str,
    pub agent_dir: &'a Path,
    pub supervisor: UpdateSupervisorIdentity,
    pub from_version: &'a str,
    pub to_version: &'a str,
    pub created_at_ms: u64,
    pub ledger: &'a RlmSpawnLedger,
}

/// Assembles the full update roster (spec §8) from the daemon's session and
/// worker rows (`resident_files`: the canonical transcripts it hosts), the
/// spawn ledger and the scheduled-jobs scan.
///
/// # Errors
///
/// The ledger could not be read.
pub fn assemble<S: std::hash::BuildHasher>(
    inputs: RosterInputs<'_>,
    sessions: Vec<UpdateRosterSession>,
    workers: Vec<UpdateRosterWorker>,
    resident_files: &HashSet<PathBuf, S>,
) -> Result<UpdateRoster> {
    let RosterInputs {
        update_id,
        socket_path,
        agent_dir,
        supervisor,
        from_version,
        to_version,
        created_at_ms,
        ledger,
    } = inputs;
    Ok(UpdateRoster {
        format_version: UPDATE_ROSTER_FORMAT_VERSION,
        update_id: update_id.clone(),
        socket_path: socket_path.to_string(),
        created_at: iso_from_unix_ms(created_at_ms),
        supervisor,
        binary: UpdateRosterBinary {
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
        },
        sessions,
        workers,
        subagents: subagent_rows(agent_dir, ledger, resident_files)?,
        heartbeats: heartbeat_rows(agent_dir),
        rest: Map::default(),
    })
}
