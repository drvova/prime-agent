//! The `[bash-quiet]` notice: a session whose background commands have all
//! written nothing for a while hears so, so a command that hangs (waiting for
//! input, a lock, the network) does not leave the agent idle until someone
//! steps in.
//!
//! One task per session watches its outstanding commands: those left
//! running past the cell that started them whose end is still to be
//! announced (`[bash-done]` due), that no one asked to kill, not queued, and
//! not persistent shells (a shell kept to outlive restarts is expected to sit
//! quiet). While none of them has written anything, no cell runs, and no
//! cell has ended for a quiet period, the session is sent one `bash.quiet`
//! request naming every one of them with its run time, quiet time and
//! output tail, and which period of the silence it came after (`step`). The periods back off: [`QUIET_AFTER`], then four times the
//! one before, for [`QUIET_STEPS`] notices; output from any of the commands,
//! or a new one, starts the schedule again. A command is named in at most
//! [`QUIET_STEPS`] notices in its life, so one that prints now and then
//! between long silences is not reported forever. The host delivers the
//! notice only to an idle session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::job::{lock, Job, Notice, Quiet};
use super::{capped, output_tail, Session};
use crate::kernel::shared::{HostRequestPayload, BASH_QUIET_REQUEST_TYPE};

/// The first quiet period: 3 minutes, `PRIME_AGENT_BASH_QUIET_MS` for one
/// session or a test. A `bash` call waits 10 s before it yields and at most
/// 600 s when asked; a command that still prints nothing 18 default windows
/// after the agent last looked, with the agent idle, is more likely stuck
/// than slow, and the first notice comes before the session's default
/// 5-minute heartbeat would.
pub(super) const QUIET_AFTER: Duration = Duration::from_secs(180);
/// Notices per silence, and per command in its life: after 3, 12 and 48
/// minutes. Past an hour of nothing, the `[bash-done]` notice or the user is
/// what is left.
const QUIET_STEPS: u32 = 3;
/// Each quiet period is this many times the one before.
const BACKOFF: u32 = 4;

/// Watch `session`'s background commands for silence, `first` being the
/// first quiet period, until its kernel is killed. Returns at once when the
/// session has no `bash.quiet` handler.
pub(super) async fn watch(session: Arc<Session>, first: Duration) {
    let Some(handler) = session.host.handlers.get(BASH_QUIET_REQUEST_TYPE).cloned() else {
        return;
    };
    let mut ended = session.cell_ended.subscribe();
    // When the last cell ended: the agent was at work until then.
    let mut cell_end = Instant::now();
    // The newest activity seen from the outstanding commands, and how many
    // notices the silence since then has had.
    let mut heard: Option<Instant> = None;
    let mut step = 0;
    loop {
        let ended_cells = *ended.borrow_and_update();
        let cell_running = lock(&session.cells).current > ended_cells;
        let outstanding: Vec<(Arc<Job>, Instant)> = {
            let table = lock(&session.table);
            if table.closed {
                return;
            }
            table
                .order
                .iter()
                .filter_map(|id| table.jobs.get(id))
                .filter(|job| {
                    let state = lock(&job.state);
                    job.lasting.is_none()
                        && job.cell <= ended_cells
                        && !state.reaped
                        && !state.queued
                        && state.quiet != Quiet::KillRequested
                        && state.notice == Notice::Due
                })
                .map(|job| {
                    let written = lock(&job.capture).last_output;
                    (
                        Arc::clone(job),
                        written.map_or(job.started, |at| at.max(job.started)),
                    )
                })
                .collect()
        };
        let latest = outstanding.iter().map(|(_, active)| *active).max();
        if latest > heard {
            heard = latest;
            step = 0;
        }
        let due = match (cell_running, latest) {
            (false, Some(latest)) if step < QUIET_STEPS => {
                Some(latest.max(cell_end) + first * BACKOFF.pow(step))
            }
            (false, Some(_) | None) | (true, _) => None,
        };
        let now = Instant::now();
        if due.is_some_and(|due| due <= now) {
            step += 1;
            let named: Vec<serde_json::Value> = outstanding
                .iter()
                .filter_map(|(job, active)| {
                    {
                        let mut state = lock(&job.state);
                        match state.quiet {
                            Quiet::Named(count) if count < QUIET_STEPS => {
                                state.quiet = Quiet::Named(count + 1);
                            }
                            Quiet::Named(_) | Quiet::KillRequested => return None,
                        }
                    }
                    // Only a persistent shell's text can fail to read, and
                    // none is outstanding.
                    let (output, cut) = output_tail(&job.text().unwrap_or_default());
                    Some(json!({
                        "job": job.id,
                        "pid": job.pid,
                        "command": capped(&job.command),
                        "elapsedMs": now.duration_since(job.started).as_millis() as u64,
                        "quietMs": now.duration_since((*active).max(cell_end)).as_millis() as u64,
                        "output": output,
                        "outputTruncated": cut,
                    }))
                })
                .collect();
            if !named.is_empty() {
                // A refused notice is a silence the session does not hear of;
                // the next step may still reach it.
                // `step`: which quiet period of the silence this is (1-3),
                // for the session's counts.
                let _ = handler(HostRequestPayload {
                    data: json!({ "jobs": named, "step": step }),
                })
                .await;
            }
            continue;
        }
        // Output arriving meanwhile is read at the next look; without a due
        // notice, look again after a first quiet period.
        let next = due.unwrap_or(now + first);
        tokio::select! {
            () = tokio::time::sleep_until(next.into()) => {}
            changed = ended.changed() => {
                if changed.is_err() {
                    return;
                }
                cell_end = Instant::now();
            }
            () = session.killed.cancelled() => return,
        }
    }
}
