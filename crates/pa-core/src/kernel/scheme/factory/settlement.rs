//! What happens when a child settles: its answer is captured into the
//! state's declared outputs, a failure is retried or handed to the state's
//! failure policy, a dead worker is verified and re-spawned once, and a
//! stop or a fail-fast cancels what is still running.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;

use super::executor::{is_infra_error, locked, Collected, Executor, RunCell, POLL_TIMEOUT};
use super::python_text::repr_str;
use super::run::{Place, Run, RunState, Status};
use super::spec::{field, RUN_FAILURE_POLICY_DEFAULT};

/// Quiet period before a worker that settled with an infra error is read
/// again: the re-check must not race the probe that reported the death.
const INFRA_VERIFY_QUIET: Duration = POLL_TIMEOUT;

impl Executor {
    /// Apply one child's settle to its instance.
    ///
    /// # Errors
    ///
    /// The re-read of a worker that settled with an infra error failed.
    pub(super) async fn apply_settlement(
        &self,
        cell: &RunCell,
        place: Place,
        mut result: Collected,
    ) -> anyhow::Result<()> {
        if locked(cell).instance(place).status != Status::Running {
            // Cancelled (a stop, a fail-fast) while the collect was in flight.
            return Ok(());
        }
        let infra = |result: &Collected| {
            result.status == "error" && is_infra_error(result.error.as_deref().unwrap_or_default())
        };
        if infra(&result) {
            // The executor verifies a worker death itself instead of
            // failing the entry on the first error row.
            let Some(fresh) = self.recheck_infra_death(cell, place, &result).await? else {
                return Ok(());
            };
            result = fresh;
            let respawn = {
                let run = locked(cell);
                run.states[place.state].entries[place.entry].status == Status::Running
                    && !run.instance(place).infra_respawned
                    && infra(&result)
            };
            if respawn {
                self.respawn_infra_dead(cell, place, &result).await;
                return Ok(());
            }
            // A second verified death takes the normal path below.
        }
        let failure: Option<(String, bool)> = {
            let mut run = locked(cell);
            let budget_ms = field(&run.states[place.state].spec, "budget_ms").as_u64();
            run.settle_count += 1;
            run.tool_use_total += result.tool_use_count;
            let instance = run.instance_mut(place);
            instance.duration_ms = result.duration_ms;
            instance.tool_uses = result.tool_use_count;
            let child_reason = match result.status.as_str() {
                "done" => None,
                "error" => Some(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "child settled with status 'error'".to_string()),
                ),
                "cancelled" => Some("child was cancelled".to_string()),
                other => Some(format!(
                    "child settled with unexpected status {}",
                    repr_str(other)
                )),
            };
            let elapsed_ms = instance
                .spawned_at
                .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX));
            if let Some(reason) = child_reason {
                // An exited child's last text is a provisional answer: it
                // may have finished its work and died without reporting.
                if let Some(answer) = result.answer.clone() {
                    instance.answer = Some(answer);
                    instance.provisional = true;
                    instance.capture_failed = false;
                }
                // Child failures retry while attempts remain.
                Some((reason, true))
            } else if let Some((budget_ms, elapsed_ms)) = budget_ms
                .zip(elapsed_ms)
                .filter(|(budget_ms, elapsed_ms)| elapsed_ms > budget_ms)
            {
                // The budget is spent, so no retry.
                Some((
                    format!(
                        "state budget_ms {budget_ms} exceeded ({elapsed_ms}ms from admission to settlement)"
                    ),
                    false,
                ))
            } else {
                instance.status = Status::Done;
                instance.answer.clone_from(&result.answer);
                instance.provisional = false;
                instance.capture_failed = false;
                let duration_ms = instance.duration_ms;
                run.instance_event(
                    "settled",
                    place,
                    None,
                    json!({"status": "done", "duration_ms": duration_ms}),
                );
                if let Some(answer) = &result.answer {
                    Self::record_answer(&mut run, place, answer);
                }
                None
            }
        };
        if let Some((reason, retry)) = failure {
            self.apply_instance_failure(cell, place, reason, retry)
                .await;
            return Ok(());
        }
        let entry_settled = {
            let mut run = locked(cell);
            let entry = &run.states[place.state].entries[place.entry];
            let settled = entry.status == Status::Running
                && entry
                    .instances
                    .iter()
                    .all(|instance| instance.status == Status::Done);
            if settled {
                let answer = run.entry_answer(place.state, place.entry);
                run.states[place.state].entries[place.entry].answer = answer;
            }
            settled
        };
        if !entry_settled {
            return Ok(());
        }
        let capture_failure = self.capture_outputs(cell, place.state, place.entry).await;
        let reason = {
            let mut run = locked(cell);
            if run.states[place.state].entries[place.entry].status != Status::Running {
                // A stop or a fail-fast claimed the entry mid-capture.
                return Ok(());
            }
            match capture_failure {
                None => {
                    run.states[place.state].entries[place.entry].status = Status::Done;
                    run.queue_settle(place.state, place.entry);
                    return Ok(());
                }
                // A declared output that never bound is the producing
                // entry's task failure, not a silent valueless settle.
                Some(reason) => {
                    let instance = run.instance_mut(place);
                    instance.provisional = true;
                    instance.capture_failed = true;
                    reason
                }
            }
        };
        self.apply_instance_failure(cell, place, reason, /*retry*/ true)
            .await;
        Ok(())
    }

    /// Verify one infra-flavoured error settle before it is applied: after
    /// a quiet period the worker is read again. Returns the result to
    /// apply (a settle that landed meanwhile, or the death, now verified),
    /// or `None` when a stop claimed the instance or the worker answered
    /// alive.
    async fn recheck_infra_death(
        &self,
        cell: &RunCell,
        place: Place,
        result: &Collected,
    ) -> anyhow::Result<Option<Collected>> {
        let claimed = || {
            let run = locked(cell);
            let instance = run.instance(place);
            instance.status != Status::Running
                || instance.child_id.as_deref() != Some(result.child_id.as_str())
        };
        tokio::time::sleep(INFRA_VERIFY_QUIET).await;
        if claimed() {
            return Ok(None);
        }
        let results = self
            .collect(std::slice::from_ref(&result.child_id), Duration::ZERO)
            .await?;
        if claimed() {
            return Ok(None);
        }
        // No fresh row at all: the child vanished from the supervisor, as
        // dead as the error said.
        Ok(
            match results
                .into_iter()
                .find(|row| row.child_id == result.child_id)
            {
                Some(row) if !row.settled => None,
                Some(row) => Some(row),
                None => Some(result.clone()),
            },
        )
    }

    /// Re-spawn one worker verified dead, free of charge: the dead attempt
    /// counts as a settle but spends neither a retry nor a `max_entries`
    /// entry, once per instance.
    async fn respawn_infra_dead(&self, cell: &RunCell, place: Place, result: &Collected) {
        // The worker may be alive but unreachable: retire it so the
        // respawn cannot run beside it. An already-gone child or a dead
        // bridge must not wedge the respawn, so a failed delete is let go.
        let _retired = self.delete_child(&result.child_id).await;
        let mut run = locked(cell);
        if run.instance(place).status != Status::Running
            || !matches!(run.state, RunState::Running | RunState::Paused)
        {
            return;
        }
        run.settle_count += 1;
        run.tool_use_total += result.tool_use_count;
        let instance = run.instance_mut(place);
        instance.duration_ms = result.duration_ms;
        instance.tool_uses = result.tool_use_count;
        instance.infra_respawned = true;
        instance.retry_base += 1;
        instance.status = Status::Pending;
        instance.error = None;
        instance.answer = None;
        instance.provisional = false;
        instance.capture_failed = false;
        let reason = result
            .error
            .clone()
            .unwrap_or_else(|| "child worker unreachable".to_string());
        run.instance_event(
            "settled",
            place,
            None,
            json!({"status": "error", "error": reason, "duration_ms": result.duration_ms}),
        );
        run.instance_event(
            "infra_respawn",
            place,
            Some(format!(
                "worker verified dead ({reason}); re-spawning the same rendered prompt \
                 once (no retry, no max_entries)"
            )),
            Value::Null,
        );
    }

    /// Capture the settle's outputs, retrying the capture once: a settle
    /// can race the child's answer hand-off on the host side. Returns the
    /// joined port errors when a declared port still binds nothing.
    async fn capture_outputs(&self, cell: &RunCell, state: usize, entry: usize) -> Option<String> {
        let unbound = |run: &Run| -> Vec<String> {
            run.declared_outputs(state)
                .into_iter()
                .filter(|name| !run.states[state].entries[entry].outputs.contains_key(name))
                .collect()
        };
        {
            let mut run = locked(cell);
            run.capture_ports(state, entry);
            // Presence, not truthiness: a port that parsed as JSON null is
            // a captured value.
            if unbound(&run).is_empty() {
                return None;
            }
        }
        let refreshed = self.refresh_entry_answers(cell, state, entry).await;
        let mut run = locked(cell);
        if refreshed {
            run.capture_ports(state, entry);
        }
        let mut failures: Vec<String> = Vec::new();
        for name in unbound(&run) {
            let quoted = repr_str(&name);
            let error = run.states[state].entries[entry]
                .output_errors
                .get(&name)
                .and_then(Value::as_str)
                .map_or_else(
                    || format!("output {quoted} captured no value from the upstream answer"),
                    str::to_string,
                );
            run.entry_event(
                "output_capture_failed",
                state,
                entry,
                Some(format!(
                    "output {quoted} did not bind after one capture retry"
                )),
                json!({"port": name, "error": error}),
            );
            failures.push(error);
        }
        (!failures.is_empty()).then(|| failures.join("; "))
    }

    /// Read the entry's settled children again, waiting one bounded poll
    /// window for a missing answer hand-off. Returns whether any answer
    /// changed.
    async fn refresh_entry_answers(&self, cell: &RunCell, state: usize, entry: usize) -> bool {
        let child_ids: Vec<String> = locked(cell).states[state].entries[entry]
            .instances
            .iter()
            .filter(|instance| instance.status == Status::Done)
            .filter_map(|instance| instance.child_id.clone())
            .collect();
        if child_ids.is_empty() {
            return false;
        }
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut changed = false;
        while locked(cell).states[state].entries[entry].status == Status::Running {
            let collected = self.collect(&child_ids, Duration::ZERO).await;
            let remaining = {
                let mut run = locked(cell);
                let results = match collected {
                    Ok(results) => results,
                    Err(error) => {
                        run.entry_event(
                            "output_capture_failed",
                            state,
                            entry,
                            Some("the first capture stands".to_string()),
                            json!({"error": format!(
                                "capture retry could not re-collect the settled children: {error:#}"
                            )}),
                        );
                        return changed;
                    }
                };
                if run.states[state].entries[entry].status != Status::Running {
                    return changed;
                }
                for instance in &mut run.states[state].entries[entry].instances {
                    let fresh = results
                        .iter()
                        .find(|row| Some(row.child_id.as_str()) == instance.child_id.as_deref())
                        .and_then(|row| row.answer.as_ref());
                    if let Some(answer) = fresh.filter(|answer| {
                        instance
                            .answer
                            .as_ref()
                            .is_none_or(|known| answer.chars().count() > known.chars().count())
                    }) {
                        instance.answer = Some(answer.clone());
                        changed = true;
                    }
                }
                if changed {
                    let answer = run.entry_answer(state, entry);
                    run.states[state].entries[entry].answer = answer;
                }
                let missing = run.states[state].entries[entry]
                    .instances
                    .iter()
                    .any(|instance| {
                        instance.status == Status::Done
                            && instance.child_id.is_some()
                            && instance.answer.as_deref().is_none_or(str::is_empty)
                    });
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !missing || remaining.is_zero() {
                    return changed;
                }
                remaining
            };
            tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
        }
        changed
    }

    /// Fail one instance: a retry is queued while attempts remain and the
    /// entry can still admit it, otherwise the entry fails now (a foreach
    /// entry does not wait for its remaining instances).
    pub(super) async fn apply_instance_failure(
        &self,
        cell: &RunCell,
        place: Place,
        reason: String,
        retry: bool,
    ) {
        let permanent = {
            let mut run = locked(cell);
            let retries = field(&run.states[place.state].spec, "retries")
                .as_u64()
                .unwrap_or(0);
            let entry_running =
                run.states[place.state].entries[place.entry].status == Status::Running;
            let instance = run.instance_mut(place);
            instance.status = Status::Error;
            instance.error = Some(reason.clone());
            let (duration_ms, attempt) = (instance.duration_ms, instance.attempt);
            // Only the admissions since the last budget reset are charged.
            let requeue =
                retry && attempt.saturating_sub(instance.retry_base) <= retries && entry_running;
            if requeue {
                instance.status = Status::Pending;
                instance.error = None;
            }
            run.instance_event(
                "settled",
                place,
                None,
                json!({"status": "error", "error": reason, "duration_ms": duration_ms}),
            );
            if requeue {
                run.instance_event(
                    "retry",
                    place,
                    Some(format!(
                        "attempt {attempt} failed; re-spawning (retries {retries})"
                    )),
                    Value::Null,
                );
            }
            !requeue
        };
        if permanent {
            self.apply_entry_failure_policy(cell, place.state, place.entry, reason)
                .await;
        }
    }

    /// Fail one entry and apply its state's failure policy: `fail_fast`
    /// cancels every child and fails the run, `continue` settles the
    /// failed entry so guard-less transitions still fire, and `escalate`
    /// (the default) pauses the run before the failure propagates.
    pub(super) async fn apply_entry_failure_policy(
        &self,
        cell: &RunCell,
        state: usize,
        entry: usize,
        reason: String,
    ) {
        enum Next {
            Nothing,
            FailFast,
            Escalated,
        }
        let (next, state_id, run_id) = {
            let mut run = locked(cell);
            // The verify mark is oversight, not policy bookkeeping: a late
            // sibling exit can carry a provisional answer after the entry
            // already went terminal, so it is checked before that guard.
            let failed = &run.states[state].entries[entry];
            let provisional = failed.instances.iter().any(|instance| {
                instance.provisional
                    && instance
                        .answer
                        .as_deref()
                        .is_some_and(|text| !text.is_empty())
            });
            if !failed.needs_verify && provisional {
                let detail = if failed
                    .instances
                    .iter()
                    .any(|instance| instance.capture_failed)
                {
                    "output capture failed; the settled answer is preserved as a \
                     provisional answer - fix the declared output or verify by hand"
                } else if failed
                    .instances
                    .iter()
                    .any(|instance| instance.infra_respawned)
                {
                    "worker verified dead by the executor after its automatic respawn; \
                     last assistant text preserved as a provisional answer"
                } else {
                    "child exit captured a provisional answer - verify the remote state"
                };
                run.states[state].entries[entry].needs_verify = true;
                run.entry_event(
                    "needs_verify",
                    state,
                    entry,
                    Some(detail.to_string()),
                    Value::Null,
                );
            }
            if !run.states[state].entries[entry].status.in_flight() {
                // The policy already ran for this entry.
                return;
            }
            let policy = field(&run.states[state].spec, "failure_policy")
                .as_str()
                .unwrap_or(RUN_FAILURE_POLICY_DEFAULT)
                .to_string();
            let state_id = run.states[state].state_id.clone();
            run.states[state].entries[entry].status = Status::Error;
            run.states[state].entries[entry].error = Some(reason.clone());
            run.states[state].error = Some(reason.clone());
            run.last_error = Some(format!("state {} failed: {reason}", repr_str(&state_id)));
            run.entry_event(
                "node_error",
                state,
                entry,
                Some(format!("failure_policy {policy}")),
                json!({ "error": reason }),
            );
            // Prepared but never admitted instances can never run now.
            for instance in 0..run.states[state].entries[entry].instances.len() {
                let place = Place {
                    state,
                    entry,
                    instance,
                };
                if run.instance(place).status == Status::Pending {
                    run.instance_mut(place).status = Status::Cancelled;
                    run.instance_event(
                        "cancelled",
                        place,
                        Some("entry failed before admission".to_string()),
                        Value::Null,
                    );
                }
            }
            if policy != "escalate" {
                run.queue_settle(state, entry);
            }
            let next = match policy.as_str() {
                // A stop (or another transition) owns the run state now.
                _ if run.state != RunState::Running => Next::Nothing,
                "fail_fast" => Next::FailFast,
                "continue" => Next::Nothing,
                _ => {
                    run.state = RunState::Paused;
                    run.pause_reason = Some(reason.clone());
                    Next::Escalated
                }
            };
            (next, state_id, run.id.clone())
        };
        match next {
            Next::Nothing => {}
            Next::FailFast => {
                self.halt_nonterminal(cell, "run failed (fail_fast)").await;
                let cancelled = {
                    let mut run = locked(cell);
                    if run.state != RunState::Running {
                        // A stop landed during the cancellations; it wins.
                        return;
                    }
                    run.state = RunState::Failed;
                    run.places()
                        .into_iter()
                        .filter(|place| run.instance(*place).status == Status::Cancelled)
                        .count()
                };
                self.milestone(
                    cell,
                    "failed",
                    format!(
                        "state {state_id} failed: {reason}; cancelled {cancelled} in-flight child(ren)"
                    ),
                    Some(state_id),
                )
                .await;
            }
            Next::Escalated => {
                self.milestone(
                    cell,
                    "paused",
                    format!(
                        "state {state_id} failed: {reason}; resume with (factory-resume \"{run_id}\") \
                         to re-admit the failed entry as a fresh attempt"
                    ),
                    Some(state_id),
                )
                .await;
            }
        }
    }

    /// Delete every running child and cancel every entry still in flight;
    /// never-entered states are marked cancelled. Returns the states that
    /// took part.
    pub(super) async fn halt_nonterminal(&self, cell: &RunCell, reason: &str) -> Vec<String> {
        let stopped: Vec<usize> = {
            let run = locked(cell);
            (0..run.states.len())
                .filter(|state| {
                    let state = &run.states[*state];
                    state.entries.iter().any(|entry| entry.status.in_flight())
                        || (state.entries.is_empty() && !state.cancelled)
                })
                .collect()
        };
        self.cancel_running(cell).await;
        let mut run = locked(cell);
        for state in &stopped {
            let state = *state;
            if !run.states[state].entries.is_empty()
                && !run.states[state]
                    .entries
                    .iter()
                    .any(|entry| entry.status.in_flight())
            {
                continue;
            }
            run.states[state].cancelled = true;
            for entry in 0..run.states[state].entries.len() {
                if !run.states[state].entries[entry].status.in_flight() {
                    continue;
                }
                run.states[state].entries[entry].status = Status::Cancelled;
                run.entry_event(
                    "node_cancelled",
                    state,
                    entry,
                    Some(reason.to_string()),
                    Value::Null,
                );
                for instance in 0..run.states[state].entries[entry].instances.len() {
                    let place = Place {
                        state,
                        entry,
                        instance,
                    };
                    if run.instance(place).status == Status::Pending {
                        run.instance_mut(place).status = Status::Cancelled;
                        run.instance_event(
                            "cancelled",
                            place,
                            Some("cancelled before admission".to_string()),
                            Value::Null,
                        );
                    }
                }
            }
        }
        stopped
            .into_iter()
            .map(|state| run.states[state].state_id.clone())
            .collect()
    }

    /// Stop every running python call: they have no owner once their run
    /// stops or its control loop fails.
    pub(super) async fn cancel_python(&self, cell: &RunCell) {
        loop {
            let claimed = {
                let mut run = locked(cell);
                let running = run.places().into_iter().find(|place| {
                    let instance = run.instance(*place);
                    instance.status == Status::Running && instance.call.is_some()
                });
                running.and_then(|place| {
                    let instance = run.instance_mut(place);
                    instance.status = Status::Cancelled;
                    let process = instance.call.take();
                    let entry = &mut run.states[place.state].entries[place.entry];
                    if entry.status == Status::Running {
                        entry.status = Status::Cancelled;
                    }
                    process.map(|process| (place, process))
                })
            };
            let Some((place, process)) = claimed else {
                return;
            };
            process.stop().await;
            locked(cell).instance_event("cancelled", place, None, Value::Null);
        }
    }

    /// Delete every running child. Each instance is claimed before its
    /// delete is sent, so a concurrent pass never deletes it twice and a
    /// settle landing meanwhile is dropped.
    async fn cancel_running(&self, cell: &RunCell) {
        self.cancel_python(cell).await;
        let places = locked(cell).places();
        for place in places {
            let child_id = {
                let mut run = locked(cell);
                let instance = run.instance_mut(place);
                match &instance.child_id {
                    Some(child_id) if instance.status == Status::Running => {
                        instance.status = Status::Cancelled;
                        child_id.clone()
                    }
                    Some(_) | None => continue,
                }
            };
            let deleted = self.delete_child(&child_id).await;
            Self::record_cancellation(&mut locked(cell), place, &child_id, deleted);
        }
    }
}
