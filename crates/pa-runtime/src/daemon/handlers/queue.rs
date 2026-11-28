//! A running session's turns and queue, as the daemon's calls change them:
//! `session.abort`, `session.abort_and_send_queued`, `session.resume_queue`,
//! `session.withdraw`, `session.restore`, `session.compact`,
//! `session.abort_compaction` and `session.abort_retry`, and the `tasks`
//! query.
//!
//! - **Abort.** The running turn is aborted (its task ends `Cancelled`) and
//!   the steers it took but had not read go back to the front of the lane.
//!   `session.abort` then parks the lane, as the old daemon parks its queue:
//!   nothing queued starts until `session.resume_queue` (input published
//!   meanwhile is admitted `parked`). `session.abort_and_send_queued` keeps
//!   the lane going, resuming it if it was parked: the steers queued run
//!   next, delivered together as one turn, then the rest in order.
//! - **Wait.** `session.wait_idle` answers once the session is at rest (or,
//!   with `"subagents": false`, once its own lane is idle), with its
//!   autonomous accounting and its live background jobs. A subagent's
//!   session is waited on too, as the old daemon's `wait_for_idle` waited on
//!   a child's.
//! - **Withdraw and restore.** `session.withdraw` takes every queued task out
//!   of the lane (each ends `Cancelled`) and answers them in the order they
//!   would have run, for a client to put back in its editor; the running one
//!   runs on. `session.restore` puts messages back as steers at the front of
//!   the lane: delivered together as the session's next turn (or read by
//!   the running turn at its next step).
//! - **Clear.** `session.clear_queue` is the old daemon's `clear_queue`: the
//!   queued tasks withdrawn and the agent messages waiting on the agent
//!   taken, each bounced to its sender; it answers `{steering, followUp}`,
//!   each lane's tasks' prompts then its agent messages'. The running turn
//!   runs on.
//! - **Abort a compaction or a retry.** `session.abort_compaction` aborts the
//!   compaction the session runs, `/compact` or automatic (the summary
//!   request in flight is dropped, nothing commits, and its `compaction_end`
//!   says it was aborted); `session.abort_retry` ends the wait of the
//!   automatic retry its turn is in (the turn settles with the failure it
//!   retried, its retry cancelled). Each answers `null` whether or not one
//!   runs, as the old daemon's `abort_compaction` and `abort_retry` do.
//! - **Compact.** `session.compact` queues `/compact [instructions]` as a
//!   prompt in the session's lane (it runs at the next turn boundary, after
//!   the running turn) and answers once its task is done, with the
//!   `compaction_end` its stream carried.

use anyhow::Context as _;
use pa_types::event_log::{
    AgentAccepted, AgentDone, AgentInput, NewEvent, Priority, Task, Topic, Trust,
};
use serde_json::{json, Value};

use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::bus::Heard;
use crate::runtime::AfterAbort;
use crate::session::SessionBuilder;

/// The `source` of the input the daemon publishes for a call.
const SOURCE: &str = "runtime-daemon";

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// The id of the open session `args.session` names, refused for one the
    /// daemon does not hold open.
    pub(super) fn open_session(&self, args: &Value) -> anyhow::Result<String> {
        let (id, _, open) = self.session(args)?;
        anyhow::ensure!(open, "session {id} is not open in the runtime daemon");
        Ok(id)
    }

    pub(super) async fn abort(&self, args: &Value, after: AfterAbort) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let requeued = self.runtime.abort(&id, after).await?;
        let lane = self.runtime.lane(&id);
        Ok(json!({
            "session": id,
            "requeued": requeued,
            "queued": lane.queued.len(),
            "parked": lane.suspended,
        }))
    }

    /// Answers once the open session is at rest, its subagents' settles
    /// included (the old daemon's `wait_for_idle` with `waitForRlmQuiescence`),
    /// or, with `"subagents": false`, once its own lane is idle (its run
    /// settled, the goal's and the autonomous run's continuations included:
    /// the old daemon's `wait_for_headless_completion`). The answer carries
    /// the autonomous run's accounting (`autonomous`, the old daemon's
    /// `DaemonAutonomousStatus`) and the kernel's live background jobs
    /// (`outstandingBackgroundJobs`), read once the wait is over.
    pub(super) async fn wait_idle(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        anyhow::ensure!(
            self.open.contains(&id) || self.runtime.subagents().lineage(&id).is_some(),
            "session {id} is not open in the runtime daemon"
        );
        match args.get("subagents") {
            None | Some(Value::Null | Value::Bool(true)) => self.runtime.at_rest(&id).await,
            Some(Value::Bool(false)) => self.runtime.idle(&id).await,
            Some(_) => anyhow::bail!("the `subagents` argument must be a boolean"),
        }
        let (autonomous, outstanding_background_jobs) = match self.runtime.session_engine(&id) {
            Some(live) => {
                let status = pa_core::autonomous::autonomous_status(&*live.autonomous.lock().await);
                (status, live.engine.background_bash_count())
            }
            None => (pa_core::autonomous::disabled_autonomous_status(), 0),
        };
        Ok(json!({
            "session": id,
            "autonomous": autonomous,
            "outstandingBackgroundJobs": outstanding_background_jobs,
        }))
    }

    pub(super) fn abort_compaction(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        if let Some(live) = self.runtime.session_engine(&id) {
            live.engine.session.abort_compaction();
        }
        Ok(Value::Null)
    }

    pub(super) fn abort_retry(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        if let Some(live) = self.runtime.session_engine(&id) {
            live.engine.session.abort_retry();
        }
        Ok(Value::Null)
    }

    pub(super) fn resume_queue(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let resumed = self.runtime.resume_queue(&id);
        Ok(json!({ "session": id, "resumed": resumed }))
    }

    pub(super) async fn withdraw(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let withdrawn = self.runtime.withdraw(&id).await?;
        Ok(json!({
            "session": id,
            "withdrawn": withdrawn.iter().map(task_json).collect::<Vec<_>>(),
        }))
    }

    pub(super) async fn clear_queue(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let withdrawn = self.runtime.withdraw(&id).await?;
        let (agent_steering, agent_follow_up) = self
            .runtime
            .subagents()
            .clear_and_bounce_agent_messages(&id)
            .await?;
        let (steering, follow_up): (Vec<Task>, Vec<Task>) = withdrawn
            .into_iter()
            .partition(|task| task.priority == Priority::Steer);
        let prompts = |tasks: Vec<Task>, agent: Vec<String>| {
            tasks
                .into_iter()
                .map(|task| task.prompt)
                .chain(agent)
                .collect::<Vec<_>>()
        };
        Ok(json!({
            "steering": prompts(steering, agent_steering),
            "followUp": prompts(follow_up, agent_follow_up),
        }))
    }

    pub(super) async fn restore(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let messages = match args.get("messages") {
            Some(Value::Array(messages)) => messages
                .iter()
                .map(|message| message.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>(),
            _ => None,
        }
        .context("the `messages` argument must be an array of strings")?;
        let mut tasks = Vec::with_capacity(messages.len());
        for text in messages {
            let accepted = self
                .publish_input(
                    &id,
                    AgentInput {
                        text,
                        images: Vec::new(),
                        priority: Some(Priority::Steer),
                    },
                )
                .await?;
            tasks.push(json!({ "task": accepted.task, "admission": accepted.admission }));
        }
        Ok(json!({ "session": id, "tasks": tasks }))
    }

    pub(super) async fn compact(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        anyhow::ensure!(
            !self.runtime.lane(&id).suspended,
            "session {id}'s queue is parked: resume it before compacting"
        );
        let text = match optional_string_arg(args, "instructions")? {
            Some(instructions) => format!("/compact {instructions}"),
            None => "/compact".to_string(),
        };
        let (stream, done) = (format!("agent/{id}/stream"), format!("agent/{id}/done"));
        // Listening before the publish: the task's events come after it.
        let mut listener = self.runtime.listen();
        let accepted = self
            .publish_input(
                &id,
                AgentInput {
                    text,
                    images: Vec::new(),
                    priority: None,
                },
            )
            .await?;
        let mut compaction = Value::Null;
        loop {
            let event = match listener.recv().await {
                Some(Heard::Event(event)) => event,
                Some(Heard::Lagged { missed }) => anyhow::bail!(
                    "lost {missed} events while waiting for the compaction (task {}): its \
                     agent/{id}/done is in the log",
                    accepted.task
                ),
                None => anyhow::bail!("the runtime stopped before the compaction ended"),
            };
            let topic = event.event.topic.as_str();
            if topic == stream && event.event.payload["type"] == "compaction_end" {
                compaction = event.event.payload.clone();
                // The stream's frame number is no part of the outcome.
                if let Some(outcome) = compaction.as_object_mut() {
                    outcome.remove("meta");
                }
            } else if topic == done {
                let ended: AgentDone = serde_json::from_value(event.event.payload.clone())?;
                if ended.task == accepted.task {
                    return Ok(json!({
                        "session": id,
                        "task": ended.task,
                        "outcome": ended.outcome,
                        "error": ended.error,
                        "compaction": compaction,
                    }));
                }
            }
        }
    }

    /// `tasks`: the session's running task, its queued ones in the order
    /// they would run, and whether its lane is parked.
    pub(super) fn tasks(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let lane = self.runtime.lane(&id);
        Ok(json!({
            "session": id,
            "running": lane.running.as_ref().map(task_json),
            "queued": lane.queued.iter().map(task_json).collect::<Vec<_>>(),
            "parked": lane.suspended,
        }))
    }

    /// Publishes `input` on session `id`'s input as the owner, and answers
    /// how its task was admitted.
    pub(super) async fn publish_input(
        &self,
        id: &str,
        input: AgentInput,
    ) -> anyhow::Result<AgentAccepted> {
        let published = self
            .runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{id}/input"))?,
                source: SOURCE.to_string(),
                session: Some(id.to_string()),
                payload: serde_json::to_value(input)?,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await?;
        crate::wire::accepted(&published, id)
            .with_context(|| format!("session {id}'s input was stored but no task was queued"))
    }
}

/// A task as the daemon's answers show it.
fn task_json(task: &Task) -> Value {
    json!({
        "task": task.id,
        "text": task.prompt,
        "priority": task.priority,
        "state": task.state,
    })
}
