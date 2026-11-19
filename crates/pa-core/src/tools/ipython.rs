//! The `ipython` tool: persistent Python REPL execution through a kernel —
//! the model-facing definition, result text composition, busy-kernel choice,
//! and the rlm bootstrap code. Kernel process management lives behind the
//! [`IpythonKernelProvisioner`] trait, owned by the kernel manager module.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

mod diff;
use crate::tools::tool_definition::{
    AbortSignal, ExecutionMode, OnUpdate, ToolContentBlock, ToolDefinition, ToolExecutionResult,
    ToolUpdate,
};
use diff::generate_diff_string;

/// Mime types the model context accepts as images.
pub const IMAGE_MIME_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

// Kernel execution types

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KernelErrorInfo {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

/// A media attachment loaded into context (e.g. by the attach-image skill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelAttachment {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExecuteStatus {
    #[default]
    Ok,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteResult {
    pub status: ExecuteStatus,
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub duration_ms: Option<u64>,
    /// Output that arrived without this cell's id, shown separately.
    pub background_output: Option<String>,
    pub error: Option<KernelErrorInfo>,
    pub attachments: Vec<KernelAttachment>,
    /// File edits displayed by this cell, in order.
    pub diffs: Vec<crate::kernel::shared::KernelDiffDisplay>,
    /// Agent messages sent from this cell, in order (TS
    /// `sentAgentMessages` on the tool-result details).
    pub sent_agent_messages: Vec<crate::kernel::shared::KernelSentAgentMessage>,
    /// The `bash()` commands this cell started, summarized for display
    /// (`bashCommands` on the tool-result details).
    pub bash_commands: Option<crate::kernel::shared::KernelBashCommands>,
    /// The cell's bounded `bash()` await outcomes (`bashWaits` on the
    /// tool-result details when any ran; the run telemetry folds them).
    pub bash_waits: crate::kernel::shared::KernelBashWaits,
    /// The cell's `rlm.session.messages` reads (`transcriptReads` on the
    /// tool-result details).
    pub transcript_reads: Vec<serde_json::Value>,
    /// The todo list the cell's last `todo.set`/`todo.update` left (`todoList` on the
    /// tool-result details).
    pub todo_list: Option<serde_json::Value>,
    /// The cell outlived its yield window and continues in the background
    /// under this cell id; its end arrives as a `[cell-done]` notice.
    pub backgrounded_cell: Option<String>,
    /// The cell ended the turn via the sanctioned `rlm.end_turn()` call:
    /// the host marked the execution, and this result terminates the tool
    /// batch (the "turn ended" note on the tool-result text).
    pub ended_turn: bool,
}

/// The wire form of one sent agent message (TS `KernelSentAgentMessage`):
/// `id`, `message`, `deliveryStatus`, `receiverRole` when present, and the
/// `target` endpoint (`sessionName` only when present).
#[must_use]
pub fn sent_agent_message_json(
    sent: &crate::kernel::shared::KernelSentAgentMessage,
) -> serde_json::Value {
    use crate::kernel::shared::{SentAgentMessageTarget, SentDeliveryStatus};
    let crate::kernel::shared::KernelSentAgentMessage {
        id,
        message,
        delivery_status,
        receiver_role,
        target:
            SentAgentMessageTarget {
                active_session_id,
                session_id,
                session_name,
            },
    } = sent;
    let delivery = match delivery_status {
        SentDeliveryStatus::Delivered => "delivered",
        SentDeliveryStatus::Queued => "queued",
    };
    let mut value = json!({
        "id": id,
        "message": message,
        "deliveryStatus": delivery,
        "target": {
            "activeSessionId": active_session_id,
            "sessionId": session_id,
        },
    });
    if let Some(role) = receiver_role {
        value["receiverRole"] = json!(role.as_str());
    }
    if let Some(name) = session_name {
        value["target"]["sessionName"] = json!(name);
    }
    value
}

pub type StreamFn<'a> = Option<&'a (dyn Fn(&str, &'static str) + Send + Sync)>;

pub type LateSentAgentMessageHandler =
    std::sync::Arc<dyn Fn(&str, crate::kernel::shared::KernelSentAgentMessage) + Send + Sync>;

pub struct KernelExecuteOptions<'a> {
    pub signal: Option<AbortSignal>,
    /// The model's per-call `timeout`, clamped to the allowed range; `None`
    /// keeps the session's default limit.
    pub timeout_ms: Option<u64>,
    /// Streams cell output while the cell runs.
    pub on_stream: StreamFn<'a>,
    pub on_late_sent_agent_message: Option<crate::kernel::shared::LateSentAgentMessageCallback>,
}

type ExecuteCellFuture = Pin<Box<dyn Future<Output = anyhow::Result<ExecuteResult>> + Send>>;

/// A running kernel able to execute code cells.
pub trait KernelExecutor: Send + Sync {
    /// Execute one cell in the persistent kernel.
    fn execute(&self, code: &str, options: KernelExecuteOptions<'_>) -> ExecuteCellFuture;
}

/// Startup progress handler (TS: `KernelBootstrapProgressHandler`).
pub type BootstrapProgressHandler = Arc<dyn Fn(&str) + Send + Sync>;

type EnsureFuture = Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>>;

/// Owns the lazy create+start+bootstrap of one session's Python kernel.
/// Implementations memoize one running kernel (concurrent `ensure` calls await
/// the same in-flight startup; a failed startup clears the memo), and `kill`
/// terminates the kernel losing all in-memory state. Object-safe on purpose.
pub trait IpythonKernelProvisioner: Send + Sync {
    /// Start (or reuse) the kernel; resolves once it is ready to execute.
    fn ensure(
        &self,
        on_progress: Option<BootstrapProgressHandler>,
        signal: Option<AbortSignal>,
    ) -> EnsureFuture;

    /// Kill the kernel immediately, losing all in-memory state.
    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

// Tool definition

/// Turn kernel image attachments into image blocks; non-images are dropped.
pub fn image_blocks_from_attachments(attachments: &[KernelAttachment]) -> Vec<ToolContentBlock> {
    attachments
        .iter()
        .filter(|a| IMAGE_MIME_TYPES.contains(&a.mime_type.as_str()))
        .map(|a| ToolContentBlock::Image {
            data: a.data.clone(),
            mime_type: a.mime_type.clone(),
        })
        .collect()
}

fn format_execute_text(result: &ExecuteResult, background_output: Option<&str>) -> String {
    let mut text = result.stdout.clone();
    if !result.stderr.is_empty() {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(&result.stderr);
    }
    if let Some(result_text) = &result.result {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(result_text);
    }
    if let Some(cell) = &result.backgrounded_cell {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(&cell_backgrounded_message(cell));
    } else if result.status == ExecuteStatus::Error {
        if let Some(error) = &result.error {
            text.push_str(if text.is_empty() { "" } else { "\n" });
            text.push_str(&error.traceback.join("\n"));
        }
    }
    if let Some(background) = background_output {
        let separator = if text.is_empty() { "" } else { "\n" };
        let _ = write!(
            text,
            "{separator}[background output (unattributed)]\n{background}"
        );
    }
    text
}

/// The running envelope of a cell that outlived its yield window (Nodecode's
/// `exec.lisp:1828` message): it was not stopped, the notice reads its end,
/// and the handle is there for a look in the meantime.
fn cell_backgrounded_message(cell: &str) -> String {
    let encoded = serde_json::to_string(cell).unwrap_or_default();
    format!(
        "[cell {cell} continues in the background: it outlived its yield window and was not \
stopped.] Do not wait for it: go on with other work or end your turn. When it ends, a [cell-done] \
notice brings its value or error and the tail of its output. Its writes go to its handle, never \
into another cell's output. Meanwhile `h = rlm.cells.get({encoded})` gives the handle: `h.poll()` \
is one look, `h.tail()` its newest output, `await h.wait(seconds)` a short wait, `h.cancel()` \
stops it. A cell that must end inside this call can pass a larger `timeout`."
    )
}

async fn execute_cell(
    provisioner: &dyn IpythonKernelProvisioner,
    report_startup_progress: BootstrapProgressHandler,
    code: &str,
    execute: KernelExecuteOptions<'_>,
) -> anyhow::Result<ExecuteResult> {
    let manager = provisioner
        .ensure(Some(report_startup_progress), execute.signal.clone())
        .await?;
    manager.execute(code, execute).await
}

pub struct IpythonToolOptions {
    /// Shared provisioner owning the kernel lifecycle.
    pub provisioner: Arc<dyn IpythonKernelProvisioner>,
    pub on_late_sent_agent_message: Option<LateSentAgentMessageHandler>,
}

pub fn ipython_tool_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["code"],
        "properties": {
            "code": {
                "type": "string",
                "description": "Python code to execute in the persistent Python REPL. Use the target project's own environment for project imports, tests, scripts, CLIs, and dependency checks instead of direct kernel imports."
            },
            "timeout": {
                "type": "integer",
                "description": format!(
                    "Seconds to wait for this cell before it continues in the background (never stopped; a [cell-done] notice reports its end). Default: a window learned from this project's recent cells unless the user configured one, max {PYTHON_REPL_TIMEOUT_MAX_S}."
                )
            }
        }
    })
}

/// The largest per-call `timeout` the model may pass, in seconds (Nodecode
/// clamps its yield window to ten minutes).
pub const PYTHON_REPL_TIMEOUT_MAX_S: u64 = 600;

/// The model's `timeout` argument as a yield window in milliseconds, clamped to
/// 1..=[`PYTHON_REPL_TIMEOUT_MAX_S`] seconds.
fn timeout_arg_ms(params: &serde_json::Value) -> Option<u64> {
    let seconds = params.get("timeout")?.as_f64()?;
    Some((seconds as u64).clamp(1, PYTHON_REPL_TIMEOUT_MAX_S) * 1000)
}

pub fn ipython_tool_description() -> &'static str {
    "Execute Python code in a persistent Python REPL. Top-level `await` is supported. Variables, imports, and loaded data persist across calls, and are revived on a best-effort basis when a session is resumed (objects that cannot be serialized are dropped and reported). Run shell commands with `bash('cmd')` / `await bash('cmd')`. Project imports, tests, scripts, CLIs, and dependency checks should run through the target project's own environment."
}

#[tracing::instrument(
    level = "debug",
    name = "tool_ipython_execute",
    skip(options, on_update, on_late_sent_agent_message)
    fields(code),
)]
pub async fn execute_ipython(
    options: &IpythonToolOptions,
    code: &str,
    timeout_ms: Option<u64>,
    signal: Option<AbortSignal>,
    on_update: Option<OnUpdate>,
    on_late_sent_agent_message: Option<crate::kernel::shared::LateSentAgentMessageCallback>,
) -> anyhow::Result<ToolExecutionResult> {
    let report_startup_progress: BootstrapProgressHandler = {
        let on_update = on_update.clone();
        Arc::new(move |message: &str| {
            if let Some(on_update) = &on_update {
                on_update(ToolUpdate {
                    content: vec![ToolContentBlock::text(message.to_string())],
                    details: Some(json!({ "status": "starting" })),
                });
            }
        })
    };

    // Stream cell output chunks as in-flight updates (TS onStream).
    let stream_update = |chunk: &str, _stream: &'static str| {
        if let Some(on_update) = &on_update {
            on_update(ToolUpdate {
                content: vec![ToolContentBlock::text(chunk.to_string())],
                details: Some(json!({ "status": "ok" })),
            });
        }
    };

    let result = execute_cell(
        options.provisioner.as_ref(),
        report_startup_progress,
        code,
        KernelExecuteOptions {
            signal,
            timeout_ms,
            on_stream: Some(&stream_update),
            on_late_sent_agent_message,
        },
    )
    .await;

    // The full context chain, not just the outermost layer: a bare top-level
    // message hides the actual cause of kernel failures.
    let r = result.map_err(|err| anyhow::anyhow!("{err:#}"))?;

    let mut text = format_execute_text(&r, r.background_output.as_deref());
    // The sanctioned turn-end confirmation: short, and it lands in the
    // transcript for replay of the ended turn.
    if r.ended_turn {
        let separator = if text.is_empty() { "" } else { "\n" };
        let _ = write!(text, "{separator}turn ended");
    }

    let image_blocks = image_blocks_from_attachments(&r.attachments);
    let mut content = vec![ToolContentBlock::text(text)];
    content.extend(image_blocks);

    let status = match r.status {
        ExecuteStatus::Ok if r.backgrounded_cell.is_some() => "backgrounded",
        ExecuteStatus::Ok => "ok",
        ExecuteStatus::Aborted => "aborted",
        ExecuteStatus::Error => "error",
    };
    let mut details = json!({ "status": status });
    if let Some(cell) = &r.backgrounded_cell {
        details["backgroundCell"] = json!(cell);
    }
    if let Some(duration) = r.duration_ms {
        details["durationMs"] = json!(duration);
    }
    if !r.stdout.is_empty() {
        details["stdout"] = json!(r.stdout);
    }
    if !r.stderr.is_empty() {
        details["stderr"] = json!(r.stderr);
    }
    if let Some(result_text) = &r.result {
        details["result"] = json!(result_text);
    }
    if let Some(bash) = &r.bash_commands {
        details["bashCommands"] = json!({
            "first": bash.first,
            "count": bash.count,
            "lines": bash.lines,
        });
    }
    if r.bash_waits != crate::kernel::shared::KernelBashWaits::default() {
        details["bashWaits"] = json!({
            "completed": r.bash_waits.completed,
            "yielded": r.bash_waits.yielded,
        });
    }
    if !r.transcript_reads.is_empty() {
        details["transcriptReads"] = json!(r.transcript_reads);
    }
    if let Some(list) = &r.todo_list {
        details["todoList"] = list.clone();
    }
    if let Some(background) = &r.background_output {
        details["backgroundOutput"] = json!(background);
    }
    if let Some(error) = &r.error {
        details["error"] = json!({
            "ename": error.ename,
            "evalue": error.evalue,
            "traceback": error.traceback,
        });
        details["errorEname"] = json!(error.ename);
    }
    if !r.diffs.is_empty() {
        details["diffs"] = json!(r
            .diffs
            .iter()
            .map(|diff| {
                let start_line = diff
                    .start_line
                    .and_then(|line| usize::try_from(line).ok())
                    .unwrap_or(1);
                json!({
                    "path": diff.path,
                    "diff": generate_diff_string(&diff.old_str, &diff.new_str, 4, start_line),
                })
            })
            .collect::<Vec<_>>());
    }
    if !r.sent_agent_messages.is_empty() {
        details["sentAgentMessages"] = json!(r
            .sent_agent_messages
            .iter()
            .map(sent_agent_message_json)
            .collect::<Vec<_>>());
    }

    // The flagged cell ends the turn: the batch rule terminates the run's
    // turn once every call in the message has executed.
    Ok(ToolExecutionResult {
        content,
        details: Some(details),
        is_error: r.status == ExecuteStatus::Error || r.status == ExecuteStatus::Aborted,
        terminate: r.ended_turn.then_some(true),
    })
}

/// The `python_repl` tool definition: exact name, schema, and description.
#[must_use]
pub fn create_ipython_tool_definition(_cwd: &str, options: IpythonToolOptions) -> ToolDefinition {
    let options = Arc::new(options);
    let execute: crate::tools::tool_definition::ExecuteFn = {
        let options = options;
        Arc::new(move |tool_call_id, params, signal, on_update| {
            let options = options.clone();
            let on_late_sent_agent_message =
                options.on_late_sent_agent_message.as_ref().map(|handler| {
                    let handler = std::sync::Arc::clone(handler);
                    let tool_call_id = tool_call_id.to_string();
                    std::sync::Arc::new(move |message| handler(&tool_call_id, message))
                        as crate::kernel::shared::LateSentAgentMessageCallback
                });
            Box::pin(async move {
                let code = params
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("python_repl tool requires a code string"))?
                    .to_string();
                execute_ipython(
                    &options,
                    &code,
                    timeout_arg_ms(&params),
                    signal,
                    on_update,
                    on_late_sent_agent_message,
                )
                .await
            })
        })
    };
    ToolDefinition {
        name: pa_types::ai::PYTHON_TOOL_NAME.to_string(),
        label: pa_types::ai::PYTHON_TOOL_NAME.to_string(),
        description: ipython_tool_description().to_string(),
        prompt_snippet:
            "python_repl - persistent Python REPL for code, state, and bash() orchestration"
                .to_string(),
        // The kernel is single-threaded; calls must not run in parallel.
        execution_mode: Some(ExecutionMode::Sequential),
        parameters: ipython_tool_schema(),
        prepare_arguments: None,
        execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_arg_is_clamped_to_the_allowed_range() {
        assert_eq!(timeout_arg_ms(&json!({ "code": "" })), None);
        assert_eq!(timeout_arg_ms(&json!({ "timeout": 5 })), Some(5_000));
        assert_eq!(timeout_arg_ms(&json!({ "timeout": 0 })), Some(1_000));
        assert_eq!(
            timeout_arg_ms(&json!({ "timeout": 3600 })),
            Some(PYTHON_REPL_TIMEOUT_MAX_S * 1000)
        );
    }

    #[test]
    fn sent_agent_message_json_matches_ts_wire_shape() {
        let sent = crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_1".to_string(),
            message: "Ping.\nThen report back.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Delivered,
            receiver_role: Some(crate::kernel::shared::ReceiverRole::Parent),
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "worker-active".to_string(),
                session_id: "worker-session".to_string(),
                session_name: Some("Worker".to_string()),
            },
        };
        assert_eq!(
            sent_agent_message_json(&sent),
            json!({
                "id": "agentmsg_1",
                "message": "Ping.\nThen report back.",
                "deliveryStatus": "delivered",
                "receiverRole": "parent",
                "target": {
                    "activeSessionId": "worker-active",
                    "sessionId": "worker-session",
                    "sessionName": "Worker",
                },
            })
        );
        // The queued receipt without a role or session name omits both.
        let queued = crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_2".to_string(),
            message: "Ping.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Queued,
            receiver_role: None,
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "a1".to_string(),
                session_id: "s1".to_string(),
                session_name: None,
            },
        };
        assert_eq!(
            sent_agent_message_json(&queued),
            json!({
                "id": "agentmsg_2",
                "message": "Ping.",
                "deliveryStatus": "queued",
                "target": { "activeSessionId": "a1", "sessionId": "s1" },
            })
        );
    }
}
