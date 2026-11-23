//! A kernel cell's result: the types a kernel hands back for one executed
//! cell, the seam a session reaches its kernel through
//! ([`KernelProvisioner`], [`KernelExecutor`]), and the composition of the
//! model-facing tool result from a cell's outcome.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

mod diff;
use crate::tools::tool_definition::{
    AbortSignal, OnUpdate, ToolContentBlock, ToolExecutionResult, ToolUpdate,
};
pub(crate) use diff::diff_lines;
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
    /// The files this cell's `fs-read`s read, absolute, oldest first
    /// (`fileReads` on the tool-result details; a compaction reads the
    /// session's latest files again from them).
    pub file_reads: Vec<String>,
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
    /// The cell's settled `ask(...)` questions (`questions` on the tool-result details).
    pub questions: Vec<serde_json::Value>,
    /// The cell outlived its yield window and continues in the background
    /// under this cell id; its end arrives as a `[cell-done]` notice.
    pub backgrounded_cell: Option<String>,
    /// The cell ended the turn via the sanctioned `rlm.end_turn()` call:
    /// the host marked the execution, and this result terminates the tool
    /// batch (the "turn ended" note on the tool-result text).
    pub ended_turn: bool,
    /// The `[error context]` block of a failed cell or awaited command: the
    /// source lines around each error location, and handles to full outputs.
    pub error_context: Option<String>,
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

/// A cell's run. It may borrow the call's options, so a kernel can call
/// [`KernelExecuteOptions::on_stream`] while the cell runs.
type ExecuteCellFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<ExecuteResult>> + Send + 'a>>;

/// A running kernel able to execute code cells.
pub trait KernelExecutor: Send + Sync {
    /// Execute one cell in the persistent kernel.
    fn execute<'a>(&self, code: &str, options: KernelExecuteOptions<'a>) -> ExecuteCellFuture<'a>;
}

/// Startup progress handler (TS: `KernelBootstrapProgressHandler`).
pub type BootstrapProgressHandler = Arc<dyn Fn(&str) + Send + Sync>;

type EnsureFuture = Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>>;

/// Owns the lazy start of one session's kernel. Implementations memoize one
/// running kernel (concurrent `ensure` calls await the same in-flight startup;
/// a failed startup clears the memo), and `kill` ends the kernel losing all
/// in-memory state. Object-safe on purpose.
pub trait KernelProvisioner: Send + Sync {
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
    if let Some(context) = &result.error_context {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(context);
    }
    text
}

/// What a result says about a cell that outlived its yield window: it was
/// not stopped, it holds the session's one engine, and how to look at it.
fn cell_backgrounded_message(cell: &str) -> String {
    format!(
        "[cell {cell} continues in the background: it outlived its yield window \
and was not stopped.] The session's Scheme engine runs one cell at a time and this cell holds it: \
until it ends, every other cell waits for it and is then refused without having run. End your turn \
or do work that needs no `eval`; when the cell ends, a [cell-done] notice brings its value or error \
and the tail of its output. Meanwhile each of these is answered at once when it is the whole cell: \
`(cell-status \"{cell}\")` for its state and what it has printed, `(cell-wait \"{cell}\" seconds)` for \
a short wait, `(cell-cancel \"{cell}\")` to stop it. A cell that must end inside this call can pass \
a larger `timeout`."
    )
}

async fn execute_cell(
    provisioner: &dyn KernelProvisioner,
    report_startup_progress: BootstrapProgressHandler,
    code: &str,
    execute: KernelExecuteOptions<'_>,
) -> anyhow::Result<ExecuteResult> {
    let manager = provisioner
        .ensure(Some(report_startup_progress), execute.signal.clone())
        .await?;
    manager.execute(code, execute).await
}

/// What the kernel tool needs from its session.
pub struct KernelToolOptions {
    /// Shared provisioner owning the kernel lifecycle.
    pub provisioner: Arc<dyn KernelProvisioner>,
    pub on_late_sent_agent_message: Option<LateSentAgentMessageHandler>,
}

/// The largest per-call `timeout` the model may pass, in seconds (Nodecode
/// clamps its yield window to ten minutes).
pub const CELL_TIMEOUT_MAX_S: u64 = 600;

/// The model's `timeout` argument as a yield window in milliseconds, clamped to
/// 1..=[`CELL_TIMEOUT_MAX_S`] seconds.
pub(crate) fn timeout_arg_ms(params: &serde_json::Value) -> Option<u64> {
    let seconds = params.get("timeout")?.as_f64()?;
    Some((seconds as u64).clamp(1, CELL_TIMEOUT_MAX_S) * 1000)
}

#[tracing::instrument(
    level = "debug",
    name = "tool_kernel_cell_execute",
    skip(options, on_update, on_late_sent_agent_message)
    fields(code),
)]
pub async fn execute_kernel_cell(
    options: &KernelToolOptions,
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
    if !r.file_reads.is_empty() {
        details["fileReads"] = json!(r.file_reads);
    }
    if let Some(list) = &r.todo_list {
        details["todoList"] = list.clone();
    }
    if !r.questions.is_empty() {
        details["questions"] = json!(r.questions);
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
        let mut budget = diff::CELL_BUDGET;
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
                    "diff": generate_diff_string(
                        &diff.old_str,
                        &diff.new_str,
                        4,
                        start_line,
                        &mut budget,
                    ),
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

#[cfg(test)]
mod golden_tests;

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
            Some(CELL_TIMEOUT_MAX_S * 1000)
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

    /// The `details.diffs` of one cell run through a real session kernel in
    /// `dir`.
    async fn cell_diffs(dir: &std::path::Path, code: &str) -> serde_json::Value {
        let options = KernelToolOptions {
            provisioner: Arc::new(crate::kernel::scheme::SchemeKernelProvisioner::new(
                crate::kernel::shared::HostRequestHandlers::new(),
                dir,
            )),
            on_late_sent_agent_message: None,
        };
        let result = execute_kernel_cell(&options, code, None, None, None, None)
            .await
            .unwrap();
        result.details.unwrap()["diffs"].take()
    }

    /// A cell's diffs keep 4,000 rows of one edit and 8,000 of the cell; past
    /// that an edit's diff ends in a row counting the changed lines it left
    /// out, so its `+A -R` stays true, down to an edit with no rows left.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_huge_write_keeps_a_bounded_diff_that_still_counts_every_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let diffs = cell_diffs(
            dir.path(),
            r#"(define big
                 (apply string-append
                   (map (lambda (i) (string-append "line " (number->string i) "\n"))
                        (range 0 20000))))
               (fs-write "a.txt" big)
               (fs-write "b.txt" big)
               (fs-write "c.txt" "one\n")"#,
        )
        .await;
        let kept = (1..=4_000)
            .map(|n| format!("+{n:>5} line {}", n - 1))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = format!("{kept}\n\u{2026} diff cut: 16000 more lines (+16000 -0)");
        let path = |name: &str| root.join(name).to_string_lossy().into_owned();
        assert_eq!(
            diffs,
            json!([
                { "path": path("a.txt"), "diff": cut },
                { "path": path("b.txt"), "diff": cut },
                { "path": path("c.txt"), "diff": "\u{2026} diff cut: 1 more line (+1 -0)" },
            ])
        );
    }

    /// An edit within the limits shows as it always did: its changed lines
    /// and four lines of context, numbered as in the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_normal_edit_shows_its_whole_diff() {
        let dir = tempfile::tempdir().unwrap();
        let text = (1..=30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.path().join("a.txt"), text).unwrap();
        let diffs = cell_diffs(
            dir.path(),
            r#"(let ([view (fs-read "a.txt")])
                 (fs-edit "a.txt" (hash 'replace (fs-anchor view 15) 'text "changed")))"#,
        )
        .await;
        let path = dir.path().canonicalize().unwrap().join("a.txt");
        let diff = [
            "    ...",
            " 11 line 11",
            " 12 line 12",
            " 13 line 13",
            " 14 line 14",
            "-15 line 15",
            "+15 changed",
            " 16 line 16",
            " 17 line 17",
            " 18 line 18",
            " 19 line 19",
            "    ...",
        ]
        .join("\n");
        assert_eq!(
            diffs,
            json!([{ "path": path.to_string_lossy(), "diff": diff }])
        );
    }
}
