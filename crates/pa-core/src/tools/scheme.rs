//! The `eval` tool, a session's kernel tool: the model evaluates Scheme in
//! the session's in-process kernel ([`crate::kernel::scheme`]). Results are
//! composed by [`crate::tools::kernel_cell`].

use std::sync::Arc;

use serde_json::json;

use crate::tools::kernel_cell::{
    execute_kernel_cell, timeout_arg_ms, KernelToolOptions, CELL_TIMEOUT_MAX_S,
};
use crate::tools::tool_definition::{
    ExecuteFn, ExecutionMode, OnUpdate, ToolContentBlock, ToolDefinition, ToolUpdate,
};

/// Characters of a running cell's newest output its card shows.
const LIVE_OUTPUT_CHARS: usize = 8_000;

/// The `eval` tool definition over a session's Scheme kernel. An agent
/// message a cell sends after its call returned (a cell in the background)
/// reaches `options.on_late_sent_agent_message` with the call's id.
#[must_use]
pub fn create_scheme_tool_definition(options: KernelToolOptions) -> ToolDefinition {
    let options = Arc::new(options);
    let execute: ExecuteFn = Arc::new(move |tool_call_id, params, signal, on_update| {
        let options = Arc::clone(&options);
        let call = tool_call_id.to_string();
        let on_late_sent_agent_message =
            options.on_late_sent_agent_message.as_ref().map(|handler| {
                let handler = Arc::clone(handler);
                let tool_call_id = tool_call_id.to_string();
                Arc::new(move |message| handler(&tool_call_id, message))
                    as crate::kernel::shared::LateSentAgentMessageCallback
            });
        // The kernel streams a cell's output piece by piece and a card shows
        // only its latest update, so each update carries the output so far.
        let on_update = on_update.map(|show| {
            let printed = std::sync::Mutex::new(String::new());
            Arc::new(move |update: ToolUpdate| {
                let piece = update
                    .details
                    .as_ref()
                    .filter(|details| details["status"] == json!("ok"))
                    .and_then(|_| update.content.first())
                    .and_then(ToolContentBlock::as_text);
                let Some(piece) = piece else {
                    return show(update);
                };
                let mut printed = printed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                printed.push_str(piece);
                let excess = printed.chars().count().saturating_sub(LIVE_OUTPUT_CHARS);
                if let Some((cut, _)) = printed.char_indices().nth(excess).filter(|_| excess > 0) {
                    printed.drain(..cut);
                }
                show(ToolUpdate {
                    content: vec![ToolContentBlock::text(printed.clone())],
                    details: update.details,
                });
            }) as OnUpdate
        });
        Box::pin(async move {
            let code = params
                .get("code")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("eval tool requires a code string"))?
                .to_string();
            execute_kernel_cell(
                &options,
                &call,
                &code,
                timeout_arg_ms(&params),
                signal,
                on_update,
                on_late_sent_agent_message,
            )
            .await
        })
    });
    ToolDefinition {
        name: pa_types::ai::SCHEME_TOOL_NAME.to_string(),
        label: pa_types::ai::SCHEME_TOOL_NAME.to_string(),
        description: "Evaluate Scheme (Steel, mostly R5RS) in the session's persistent kernel. The forms run in order and definitions persist across calls; the result is the value of the last form, after whatever `display`/`displayln` printed. Every other tool (`bash`, the `fs-*` file tools, subagents, memory) is a Scheme function called from here: the system prompt lists them, and `(api-help)` prints their reference.".to_string(),
        prompt_snippet: "eval - evaluate Scheme inside the running agent; definitions persist".to_string(),
        // One engine per session: its cells run one at a time.
        execution_mode: Some(ExecutionMode::Sequential),
        parameters: json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The cell: Scheme forms to evaluate, in order."
                },
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "Seconds to wait for this cell, max {CELL_TIMEOUT_MAX_S}. In a session that can be woken later, a cell still running then continues in the background and keeps the engine (the result names it and says how to read, wait for, or stop it), and the default is a window learned from this project's recent cells. In any other session the cell is interrupted, and the default is no limit."
                    )
                }
            }
        }),
        prepare_arguments: None,
        execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
    use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
    use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};

    /// A card shows its latest update only, so a running cell's updates
    /// each carry everything it has printed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_cells_updates_carry_its_output_so_far() {
        let shown = Arc::new(tokio::sync::Notify::new());
        let waiting = Arc::clone(&shown);
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "update-shown",
            host_handler(move |_payload: HostRequestPayload| {
                let waiting = Arc::clone(&waiting);
                async move {
                    waiting.notified().await;
                    Ok(json!({}))
                }
            }),
        );
        let tool = create_scheme_tool_definition(KernelToolOptions {
            provisioner: Arc::new(SchemeKernelProvisioner::new(handlers, "/")),
            on_late_sent_agent_message: None,
        });
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&updates);
        let on_update: OnUpdate = Arc::new(move |update: ToolUpdate| {
            seen.lock()
                .unwrap()
                .push(update.content[0].as_text().map(str::to_string));
            shown.notify_one();
        });
        (tool.execute)(
            "call-1",
            json!({"code": "(display \"one \") (%host-request \"update-shown\" (hash)) (display \"two\")"}),
            None,
            Some(on_update),
        )
        .await
        .unwrap();
        assert_eq!(
            *updates.lock().unwrap(),
            vec![Some("one ".to_string()), Some("one two".to_string())]
        );
    }

    /// The result of a cell that went to the background says what holds for
    /// a session's one Scheme engine.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_background_cells_result_says_what_holds_for_a_scheme_cell() {
        let dir = tempfile::tempdir().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let held = Arc::clone(&release);
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "gate-wait",
            host_handler(move |_payload: HostRequestPayload| {
                let held = Arc::clone(&held);
                async move {
                    held.notified().await;
                    Ok(json!({}))
                }
            }),
        );
        let tool = create_scheme_tool_definition(KernelToolOptions {
            provisioner: Arc::new(SchemeKernelProvisioner::with_options(SchemeKernelOptions {
                host_handlers: handlers,
                cwd: dir.path().to_path_buf(),
                cell_window: CellWindow::for_session(CellWindowConfig {
                    daemonless: false,
                    configured_ms: Some(100),
                    agent_dir: dir.path(),
                    project_dir: dir.path(),
                    on_armed: None,
                }),
                ..SchemeKernelOptions::default()
            })),
            on_late_sent_agent_message: None,
        });
        let result = (tool.execute)(
            "call-1",
            json!({"code": "(display \"started\") (%host-request \"gate-wait\" (hash))"}),
            None,
            None,
        )
        .await
        .unwrap();
        release.notify_one();
        let details = result.details.unwrap();
        let cell = details["backgroundCell"].as_str().unwrap();
        assert_eq!(
            (
                result.content[0].as_text(),
                details["status"].as_str(),
                result.is_error
            ),
            (
                Some(
                    format!(
                        "started\n[cell {cell} continues in the background: it outlived its yield \
window and was not stopped.] The session's Scheme engine runs one cell at a time and this cell \
holds it: until it ends, every other cell waits for it and is then refused without having run. \
End your turn or do work that needs no `eval`; when the cell ends, a [cell-done] notice brings its \
value or error and the tail of its output. Meanwhile each of these is answered at once when it is \
the whole cell: `(cell-status \"{cell}\")` for its state and what it has printed, \
`(cell-wait \"{cell}\" seconds)` for a short wait, `(cell-cancel \"{cell}\")` to stop it. A cell \
that must end inside this call can pass a larger `timeout`."
                    )
                    .as_str()
                ),
                Some("backgrounded"),
                false
            )
        );
    }
}
