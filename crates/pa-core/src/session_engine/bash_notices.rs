//! The kernel notice seam (the TS async-bash-completion wake): the
//! background-job notices (`bash.completed`/`bash.consumed` for a detached
//! command, `cell.completed`/`cell.consumed` for a backgrounded cell) plus
//! the `factory.progress` run-milestone notice that rides the same steering
//! delivery. Here live
//! the shared notice types, the TS payload validation, and the
//! host-handler registration, plus the in-process sinks a daemonless
//! embedding uses — the notice row steers onto the session's own agent
//! and the embedding's wake fires.

use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;

use crate::kernel::shared::{host_handler, HostRequestHandlers, BASH_COMPLETED_REQUEST_TYPE};
use pa_agent::types::AgentMessage;
use pa_types::session::CustomMessage;

use super::messages::{
    self, CellEnd, ASYNC_BASH_COMPLETION_CUSTOM_TYPE, ASYNC_BASH_COMPLETION_PREVIEW_LABEL,
    ASYNC_CELL_COMPLETION_CUSTOM_TYPE, ASYNC_CELL_COMPLETION_PREVIEW_LABEL,
    FACTORY_PROGRESS_PREVIEW_LABEL,
};
use super::{session_message_to_loop, SessionAgentMessage};

/// One kernel notice the host admits as a steering row (TS
/// `_promptInjectedMessage` with `streamingBehavior: "steer"`,
/// `queueIfBusy`, and `resumeIfIdle`): a detached bash completion
/// (`bash.completed`) or a factory run milestone (`factory.progress`).
#[derive(Debug, Clone)]
pub struct KernelNotice {
    /// The durable custom row the woken or steered turn carries.
    pub row: CustomMessage,
    /// The queue strip's label for the row's preview.
    pub preview_label: &'static str,
    pub delivery: NoticeDelivery,
}

/// What admitting a notice does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeDelivery {
    /// The row steers onto the session: a busy turn takes it next, an idle session wakes into it.
    Steer,
    /// The row is only recorded: persisted and shown in the transcript, with no turn behind it.
    /// A daemonless embedding has no transcript to show it in and drops it.
    Record,
}

impl KernelNotice {
    /// The `[bash-done]` notice for one finished detached command.
    #[must_use]
    pub fn bash_completion(
        pid: u32,
        command: &str,
        exit_code: i64,
        output: messages::BackgroundOutputTail<'_>,
    ) -> Self {
        Self {
            row: messages::create_async_bash_completion_message(
                pid,
                command,
                exit_code,
                output,
                crate::autonomous::now_millis(),
            ),
            preview_label: ASYNC_BASH_COMPLETION_PREVIEW_LABEL,
            delivery: NoticeDelivery::Steer,
        }
    }

    /// The `[bash-lost]` notice for one command that was still running when
    /// its kernel ended: it was killed, and no result will come.
    #[must_use]
    pub fn bash_lost(pid: u32, command: &str) -> Self {
        Self {
            row: messages::create_async_bash_lost_message(
                pid,
                command,
                crate::autonomous::now_millis(),
            ),
            preview_label: ASYNC_BASH_COMPLETION_PREVIEW_LABEL,
            delivery: NoticeDelivery::Steer,
        }
    }

    /// The `[cell-done]` notice for one backgrounded cell that ended.
    #[must_use]
    pub fn cell_completion(
        cell: &str,
        end: CellEnd,
        text: &str,
        output: messages::BackgroundOutputTail<'_>,
    ) -> Self {
        Self {
            row: messages::create_async_cell_completion_message(
                cell,
                end,
                text,
                output,
                crate::autonomous::now_millis(),
            ),
            preview_label: ASYNC_CELL_COMPLETION_PREVIEW_LABEL,
            delivery: NoticeDelivery::Steer,
        }
    }
}

/// The queue-admission seam for one kernel notice (the daemon worker's
/// steering lane + runner wake + recovery busy-evidence; the in-process
/// sinks below steer onto the session's own agent).
pub type KernelNoticeSink = Arc<dyn Fn(KernelNotice) + Send + Sync>;

/// The kernel read a finished background job's result before its notice
/// delivered (`bash.consumed`, `cell.consumed`): the queued notice is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundJobConsumed {
    /// A detached `bash()` command, by pid and command (pids alone are reused).
    Bash { pid: u32, command: String },
    /// A backgrounded Python cell, by its cell id.
    Cell { cell: String },
}

impl BackgroundJobConsumed {
    /// Whether one queued row (its `customType` and `details`) is this
    /// job's completion notice.
    #[must_use]
    pub fn matches_row(&self, custom_type: Option<&str>, details: &Value) -> bool {
        match self {
            Self::Bash { pid, command } => {
                custom_type == Some(ASYNC_BASH_COMPLETION_CUSTOM_TYPE)
                    && details.get("pid").and_then(Value::as_u64) == Some(u64::from(*pid))
                    && details.get("command").and_then(Value::as_str) == Some(command.as_str())
            }
            Self::Cell { cell } => {
                custom_type == Some(ASYNC_CELL_COMPLETION_CUSTOM_TYPE)
                    && details.get("cell").and_then(Value::as_str) == Some(cell.as_str())
            }
        }
    }
}

/// Whether a queued row's `customType` is a background job's completion
/// notice (`[bash-done]` or `[cell-done]`): finished work whose wake is
/// still pending delivery.
#[must_use]
pub fn is_background_completion_type(custom_type: Option<&str>) -> bool {
    matches!(
        custom_type,
        Some(ASYNC_BASH_COMPLETION_CUSTOM_TYPE | ASYNC_CELL_COMPLETION_CUSTOM_TYPE)
    )
}

/// The queue-withdrawal seam for one consumed notice.
pub type BackgroundJobConsumedSink = Arc<dyn Fn(BackgroundJobConsumed) + Send + Sync>;

/// The daemonless embedding's wake (TS `resumeIfIdle`): fires after a
/// notice admits its row, so the embedding resumes the idle run.
pub type BashNoticeWake = Arc<dyn Fn() + Send + Sync>;

/// The `bash.completed`/`bash.consumed`/`cell.completed`/`cell.consumed`/
/// `factory.progress` kernel host handlers (TS
/// `createAsyncBashCompletionHostHandler`, `createAsyncBashConsumedHostHandler`,
/// `createFactoryProgressHostHandler`; the cell pair is the same seam for a
/// backgrounded cell).
pub fn register_kernel_notice_host_handlers(
    notice: KernelNoticeSink,
    consumed: BackgroundJobConsumedSink,
    handlers: &mut HostRequestHandlers,
) {
    let completion = Arc::clone(&notice);
    handlers.register(
        BASH_COMPLETED_REQUEST_TYPE,
        host_handler(move |payload| {
            let completion = completion.clone();
            Box::pin(async move {
                let notice = validate_completion(&payload.data)?;
                // Any closed-session gate is the sink's (the daemon
                // worker refuses injections after kill/shutdown like TS
                // `_disposed`; the in-process sinks bind a weak agent).
                completion(notice);
                Ok(serde_json::json!({}))
            })
        }),
    );
    let bash_consumed = Arc::clone(&consumed);
    handlers.register(
        "bash.consumed",
        host_handler(move |payload| {
            let consumed = bash_consumed.clone();
            Box::pin(async move {
                consumed(validate_consumed(&payload.data)?);
                Ok(serde_json::json!({}))
            })
        }),
    );
    let cell_completion = Arc::clone(&notice);
    handlers.register(
        "cell.completed",
        host_handler(move |payload| {
            let completion = cell_completion.clone();
            Box::pin(async move {
                completion(validate_cell_completion(&payload.data)?);
                Ok(serde_json::json!({}))
            })
        }),
    );
    handlers.register(
        "cell.consumed",
        host_handler(move |payload| {
            let consumed = consumed.clone();
            Box::pin(async move {
                consumed(BackgroundJobConsumed::Cell {
                    cell: cell_id(&payload.data)?,
                });
                Ok(serde_json::json!({}))
            })
        }),
    );
    handlers.register(
        "factory.progress",
        host_handler(move |payload| {
            let notice = notice.clone();
            Box::pin(async move {
                notice(validate_factory_progress(&payload.data)?);
                Ok(serde_json::json!({}))
            })
        }),
    );
}

/// TS `createAsyncBashCompletionHostHandler` validation: a positive
/// integer pid, a non-empty string command, an integer exit code, plus the
/// kernel's bounded output tail (a string and its `outputTruncated` flag).
/// A command lost with its kernel (`lost` true) has neither of the last two.
fn validate_completion(data: &Value) -> anyhow::Result<KernelNotice> {
    let (pid, command) = bash_job(data)?;
    if data.get("lost") == Some(&Value::Bool(true)) {
        return Ok(KernelNotice::bash_lost(pid, &command));
    }
    let exit_code = data
        .get("exitCode")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("bash.completed exitCode must be an integer"))?;
    let output = output_tail("bash.completed", data)?;
    Ok(KernelNotice::bash_completion(
        pid, &command, exit_code, output,
    ))
}

/// The kernel's bounded output tail: a string and its `outputTruncated` flag.
fn output_tail<'a>(
    request: &str,
    data: &'a Value,
) -> anyhow::Result<messages::BackgroundOutputTail<'a>> {
    let tail = data
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{request} output must be a string"))?;
    match data.get("outputTruncated").and_then(Value::as_bool) {
        Some(true) => Ok(messages::BackgroundOutputTail::Truncated(tail)),
        Some(false) => Ok(messages::BackgroundOutputTail::Complete(tail)),
        None => anyhow::bail!("{request} outputTruncated must be a boolean"),
    }
}

/// The `cell.completed` payload (`rlm/cells.py` `_notify_completion`): a
/// non-empty cell id, the end status, the value or traceback text, and the
/// bounded output tail.
fn validate_cell_completion(data: &Value) -> anyhow::Result<KernelNotice> {
    let cell = cell_id(data)?;
    let end = match data.get("status").and_then(Value::as_str) {
        Some("ok") => CellEnd::Ok,
        Some("error") => CellEnd::Error,
        Some("interrupted") => CellEnd::Interrupted,
        _ => anyhow::bail!("cell.completed status must be ok, error or interrupted"),
    };
    let text = data
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("cell.completed text must be a string"))?;
    let output = output_tail("cell.completed", data)?;
    Ok(KernelNotice::cell_completion(&cell, end, text, output))
}

/// A `cell.*` request's non-empty cell id.
fn cell_id(data: &Value) -> anyhow::Result<String> {
    data.get("cell")
        .and_then(Value::as_str)
        .filter(|cell| !cell.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("cell request cell must be a non-empty string"))
}

/// TS `createAsyncBashConsumedHostHandler` validation: a positive integer pid and a
/// non-empty command (pids are reused across handles, so the command disambiguates).
fn validate_consumed(data: &Value) -> anyhow::Result<BackgroundJobConsumed> {
    let (pid, command) = bash_job(data)?;
    Ok(BackgroundJobConsumed::Bash { pid, command })
}

/// A `bash.*` request's job: its positive pid and non-empty command.
fn bash_job(data: &Value) -> anyhow::Result<(u32, String)> {
    let pid = data
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0 && u32::try_from(*pid).is_ok())
        .ok_or_else(|| anyhow::anyhow!("bash.completed pid must be a positive integer"))?
        as u32;
    let command = data
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
        .ok_or_else(|| anyhow::anyhow!("bash.completed command must be a non-empty string"))?
        .to_string();
    Ok((pid, command))
}

/// The kernel factory executor's milestone payload (`FactoryExecutor._milestone`
/// in `rlm/factory.py`): non-empty `run_id`, `kind`, and `detail` strings, and
/// an optional non-empty `node` string.
fn validate_factory_progress(data: &Value) -> anyhow::Result<KernelNotice> {
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| anyhow::anyhow!("factory.progress {key} must be a non-empty string"))
    };
    let node = data
        .get("node")
        .is_some()
        .then(|| text("node"))
        .transpose()?;
    Ok(KernelNotice {
        row: messages::create_factory_progress_message(
            text("run_id")?,
            text("kind")?,
            node,
            text("detail")?,
            crate::autonomous::now_millis(),
        ),
        preview_label: FACTORY_PROGRESS_PREVIEW_LABEL,
        delivery: NoticeDelivery::Steer,
    })
}

/// The session's agent through which the in-process sinks steer and
/// withdraw: late-bound because the handlers register before the agent
/// exists, and weak so the handler -> agent -> kernel -> handler graph
/// stays acyclic (a dropped session's handlers bind nothing).
pub type BashNoticeAgentSlot = Arc<Mutex<Option<Weak<pa_agent::agent::Agent>>>>;

/// The in-process notice sink over one session's own agent: a [`NoticeDelivery::Steer`] notice
/// steers its row onto the agent's steering queue and fires the embedding's wake; a
/// [`NoticeDelivery::Record`] notice has no transcript to land in and drops.
#[must_use]
pub fn in_process_notice_sink(
    wake: &BashNoticeWake,
    agent_slot: &BashNoticeAgentSlot,
) -> KernelNoticeSink {
    let notice_wake = Arc::clone(wake);
    let notice_slot = Arc::clone(agent_slot);
    Arc::new(move |notice| {
        match notice.delivery {
            NoticeDelivery::Steer => {}
            NoticeDelivery::Record => return,
        }
        let Some(agent) = bound_agent(&notice_slot) else {
            return;
        };
        let Some(row) = session_message_to_loop(&SessionAgentMessage::Custom(notice.row)) else {
            return;
        };
        agent.steer(row);
        notice_wake();
    })
}

/// The in-process sinks over one session's own agent (the daemonless
/// embeddings): a notice steers its row (`[bash-done]` or
/// `[factory-progress]`) onto the agent's steering queue and fires the
/// embedding's wake (TS `queueIfBusy: true` + `resumeIfIdle: true` — the
/// row rides the steering lane and the embedding resumes the idle run); a
/// consumed notice withdraws the queued completion row.
pub fn register_in_process_kernel_notice_handlers(
    wake: &BashNoticeWake,
    agent_slot: &BashNoticeAgentSlot,
    handlers: &mut HostRequestHandlers,
) {
    let notice = in_process_notice_sink(wake, agent_slot);
    let consumed_slot = Arc::clone(agent_slot);
    let consumed: BackgroundJobConsumedSink = Arc::new(move |notice| {
        let Some(agent) = bound_agent(&consumed_slot) else {
            return;
        };
        // One read withdraws one notice (TS 5865-5869; the daemon's
        // front-most rule): pid reuse can queue an identical key twice
        // and the read belongs to the older handle.
        let withdrawn = std::cell::Cell::new(false);
        agent.remove_queued_messages(|row| {
            !withdrawn.get() && is_queued_completion_row(row, &notice) && {
                withdrawn.set(true);
                true
            }
        });
    });
    register_kernel_notice_host_handlers(notice, consumed, handlers);
}

/// The bound agent behind the late slot, when the session still lives.
fn bound_agent(slot: &BashNoticeAgentSlot) -> Option<Arc<pa_agent::agent::Agent>> {
    slot.lock()
        .expect("bash notice agent slot lock")
        .as_ref()
        .and_then(Weak::upgrade)
}

/// Whether one queued loop row is the completion notice of this job.
fn is_queued_completion_row(row: &AgentMessage, notice: &BackgroundJobConsumed) -> bool {
    let AgentMessage::Custom(custom) = row else {
        return false;
    };
    notice.matches_row(
        custom.payload.get("customType").and_then(Value::as_str),
        custom.payload.get("details").unwrap_or(&Value::Null),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::shared::{HostHandlerFn, HostRequestPayload};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn completed_payload(pid: u64, command: &str, exit_code: i64) -> Value {
        serde_json::json!({
            "type": "bash.completed",
            "pid": pid,
            "command": command,
            "exitCode": exit_code,
            "output": "",
            "outputTruncated": false,
        })
    }

    async fn dispatch(handler: &HostHandlerFn, data: Value) -> anyhow::Result<Value> {
        handler(HostRequestPayload { data }).await
    }

    fn wired_agent() -> (
        Arc<pa_agent::agent::Agent>,
        BashNoticeAgentSlot,
        Arc<AtomicUsize>,
        HostRequestHandlers,
    ) {
        let agent = Arc::new(pa_agent::agent::Agent::new(
            pa_agent::agent::AgentOptions::default(),
        ));
        let slot: BashNoticeAgentSlot = Arc::new(Mutex::new(Some(Arc::downgrade(&agent))));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake = {
            let wakes = Arc::clone(&wakes);
            Arc::new(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            }) as BashNoticeWake
        };
        let mut handlers = HostRequestHandlers::default();
        register_in_process_kernel_notice_handlers(&wake, &slot, &mut handlers);
        (agent, slot, wakes, handlers)
    }

    #[tokio::test]
    async fn completed_queues_the_bash_done_row_and_fires_the_wake() {
        let (agent, _slot, wakes, handlers) = wired_agent();
        let handler = handlers
            .get("bash.completed")
            .expect("the completion handler registered")
            .clone();
        let reply = dispatch(&handler, completed_payload(4321, "sleep 1", 7))
            .await
            .expect("the notice is accepted");
        assert_eq!(reply, serde_json::json!({}));
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "the admission fired the wake"
        );
        let queued = agent.remove_queued_messages(|_| true);
        assert_eq!(queued.len(), 1, "one row queued on the agent");
        let AgentMessage::Custom(custom) = &queued[0] else {
            panic!("the queued row is the custom notice: {queued:?}");
        };
        assert_eq!(
            custom.payload.get("customType").and_then(Value::as_str),
            Some(ASYNC_BASH_COMPLETION_CUSTOM_TYPE)
        );
        assert_eq!(
            custom
                .payload
                .pointer("/details/pid")
                .and_then(Value::as_u64),
            Some(4321)
        );
        assert_eq!(
            custom
                .payload
                .pointer("/details/exitCode")
                .and_then(Value::as_i64),
            Some(7)
        );
        assert_eq!(
            custom
                .payload
                .pointer("/details/command")
                .and_then(Value::as_str),
            Some("sleep 1")
        );
        assert!(
            custom
                .payload
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("[bash-done pid:4321 exit:7]")),
            "the row carries the notice header: {}",
            custom.payload
        );
    }

    #[tokio::test]
    async fn consumed_withdraws_the_front_most_queued_row() {
        let (agent, _slot, _wakes, handlers) = wired_agent();
        let completed = handlers
            .get("bash.completed")
            .expect("the completion handler registered")
            .clone();
        // Pid reuse queues an identical key twice (the read belongs to
        // the older handle).
        dispatch(&completed, completed_payload(4321, "sleep 1", 0))
            .await
            .expect("the first notice is accepted");
        dispatch(&completed, completed_payload(4321, "sleep 1", 0))
            .await
            .expect("the second notice is accepted");
        assert!(agent.has_queued_messages());

        let consumed = handlers
            .get("bash.consumed")
            .expect("the consumed handler registered")
            .clone();
        dispatch(
            &consumed,
            serde_json::json!({ "type": "bash.consumed", "pid": 4321, "command": "sleep 1" }),
        )
        .await
        .expect("the withdrawal is accepted");
        let remaining = agent.remove_queued_messages(|_| true);
        assert_eq!(
            remaining.len(),
            1,
            "one read withdrew the front-most row; the second identical row stayed"
        );
    }

    #[tokio::test]
    async fn cell_completed_queues_the_cell_done_row_and_cell_consumed_withdraws_it() {
        let (agent, _slot, wakes, handlers) = wired_agent();
        let completed = handlers
            .get("cell.completed")
            .expect("the cell completion handler registered")
            .clone();
        let payload = |cell: &str| {
            serde_json::json!({
                "type": "cell.completed",
                "cell": cell,
                "status": "ok",
                "output": "after",
                "outputTruncated": false,
                "text": "'finished'",
            })
        };
        assert!(
            dispatch(
                &completed,
                serde_json::json!({ "cell": "c1", "status": "done" })
            )
            .await
            .is_err(),
            "an unknown status is refused"
        );
        dispatch(&completed, payload("c1"))
            .await
            .expect("the first notice is accepted");
        dispatch(&completed, payload("c2"))
            .await
            .expect("the second notice is accepted");
        assert_eq!(wakes.load(Ordering::SeqCst), 2);

        let consumed = handlers
            .get("cell.consumed")
            .expect("the cell consumed handler registered")
            .clone();
        dispatch(
            &consumed,
            serde_json::json!({ "type": "cell.consumed", "cell": "c1" }),
        )
        .await
        .expect("the withdrawal is accepted");
        let remaining = agent.remove_queued_messages(|_| true);
        let [AgentMessage::Custom(custom)] = remaining.as_slice() else {
            panic!("only the other cell's notice stays queued: {remaining:?}");
        };
        assert_eq!(
            (
                custom.payload.get("customType"),
                custom.payload.get("content"),
                custom.payload.get("details"),
            ),
            (
                Some(&serde_json::json!("async_cell_completion")),
                Some(&serde_json::json!(
                    "[cell-done cell:c2 status:ok]\n\nValue:\n```\n'finished'\n```\n\nOutput:\n```\nafter\n```"
                )),
                Some(&serde_json::json!({ "cell": "c2", "status": "ok" })),
            )
        );
    }

    #[tokio::test]
    async fn factory_progress_queues_the_milestone_row_and_fires_the_wake() {
        let (agent, _slot, wakes, handlers) = wired_agent();
        let handler = handlers
            .get("factory.progress")
            .expect("the factory progress handler registered")
            .clone();
        let missing_run = serde_json::json!({ "kind": "paused", "detail": "state a failed" });
        assert!(
            dispatch(&handler, missing_run).await.is_err(),
            "a milestone without a run id is refused"
        );
        let milestone = serde_json::json!({
            "run_id": "run-7",
            "kind": "budget_exceeded",
            "detail": "run budget_ms 2000 exceeded",
        });
        dispatch(&handler, milestone)
            .await
            .expect("the milestone is accepted");
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "the admission fired the wake"
        );
        let queued = agent.remove_queued_messages(|_| true);
        let [AgentMessage::Custom(custom)] = queued.as_slice() else {
            panic!("one custom notice row queued: {queued:?}");
        };
        assert_eq!(
            (
                custom.payload.get("customType"),
                custom.payload.get("content"),
                custom.payload.get("details"),
            ),
            (
                Some(&serde_json::json!("factory_progress_notice")),
                Some(&serde_json::json!(
                    "[factory-progress run:run-7] budget-exceeded: run budget_ms 2000 exceeded"
                )),
                Some(&serde_json::json!({
                    "runId": "run-7",
                    "kind": "budget_exceeded",
                    "detail": "run budget_ms 2000 exceeded",
                })),
            )
        );
    }

    #[test]
    fn a_lost_command_is_announced_as_lost_with_no_exit_code() {
        let notice = validate_completion(&serde_json::json!({
            "pid": 4321,
            "command": "sleep 600",
            "lost": true,
        }))
        .expect("a lost command needs no exit code or output");
        assert_eq!(
            (
                notice.row.custom_type.as_str(),
                notice.row.content.text(),
                notice.row.details,
                notice.delivery,
            ),
            (
                ASYNC_BASH_COMPLETION_CUSTOM_TYPE,
                "[bash-lost pid:4321]\n\nCommand: \"sleep 600\"\n\n\
                 This command was still running when the session's kernel ended \
                 (the session was stopped or restarted, or its host exited), so it \
                 was killed and has no result. Start it again if you still need it."
                    .to_string(),
                Some(serde_json::json!({
                    "pid": 4321,
                    "command": "sleep 600",
                    "lost": true,
                })),
                NoticeDelivery::Steer,
            )
        );
    }

    #[test]
    fn completion_validation_matches_the_ts_contract() {
        assert!(validate_completion(&completed_payload(4321, "sleep 1", 0)).is_ok());
        assert!(validate_completion(&completed_payload(0, "sleep 1", 0)).is_err());
        assert!(validate_completion(&completed_payload(4321, "", 0)).is_err());
        assert!(validate_completion(&serde_json::json!({
            "type": "bash.completed",
            "pid": 4321,
            "command": "sleep 1",
            "exitCode": "0",
        }))
        .is_err());
    }
}
