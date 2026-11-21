//! The `eval` tool: the model evaluates Scheme in the session's in-process
//! kernel ([`crate::kernel::scheme`]). Results render through the same path
//! as the Python kernel's cells, so the transcript shows both alike.

use std::sync::Arc;

use serde_json::json;

use crate::tools::ipython::{
    execute_ipython, timeout_arg_ms, IpythonKernelProvisioner, IpythonToolOptions,
    PYTHON_REPL_TIMEOUT_MAX_S,
};
use crate::tools::tool_definition::{
    ExecuteFn, ExecutionMode, OnUpdate, ToolContentBlock, ToolDefinition, ToolUpdate,
};

/// Characters of a running cell's newest output its card shows.
const LIVE_OUTPUT_CHARS: usize = 8_000;

/// How the shared result path ends its note about a cell that went to the
/// background. That note is worded for the Python kernel (handles, cells
/// running side by side); a Scheme cell's result says what holds for it.
const PYTHON_BACKGROUND_NOTE_END: &str =
    "A cell that must end inside this call can pass a larger `timeout`.";

/// The `eval` tool definition over a session's Scheme kernel.
#[must_use]
pub fn create_scheme_tool_definition(
    provisioner: Arc<dyn IpythonKernelProvisioner>,
) -> ToolDefinition {
    let options = Arc::new(IpythonToolOptions {
        provisioner,
        on_late_sent_agent_message: None,
    });
    let execute: ExecuteFn = Arc::new(move |_tool_call_id, params, signal, on_update| {
        let options = Arc::clone(&options);
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
            let mut result = execute_ipython(
                &options,
                &code,
                timeout_arg_ms(&params),
                signal,
                on_update,
                None,
            )
            .await?;
            let background = result
                .details
                .as_ref()
                .and_then(|details| details["backgroundCell"].as_str())
                .map(str::to_string);
            if let (Some(cell), Some(ToolContentBlock::Text { text })) =
                (background, result.content.first_mut())
            {
                let start = text.find(&format!("[cell {cell} continues in the background"));
                let end = text
                    .find(PYTHON_BACKGROUND_NOTE_END)
                    .map(|at| at + PYTHON_BACKGROUND_NOTE_END.len());
                if let (Some(start), Some(end)) = (start, end) {
                    text.replace_range(
                        start..end,
                        &format!(
                            "[cell {cell} continues in the background: it outlived its yield window \
and was not stopped.] The session's Scheme engine runs one cell at a time and this cell holds it: \
until it ends, every other cell waits for it and is then refused without having run. End your turn \
or do work that needs no `eval`; when the cell ends, a [cell-done] notice brings its value or error \
and the tail of its output. Meanwhile each of these is answered at once when it is the whole cell: \
`(cell-status \"{cell}\")` for its state and what it has printed, `(cell-wait \"{cell}\" seconds)` for \
a short wait, `(cell-cancel \"{cell}\")` to stop it. {PYTHON_BACKGROUND_NOTE_END}"
                        ),
                    );
                }
            }
            Ok(result)
        })
    });
    ToolDefinition {
        name: pa_types::ai::SCHEME_TOOL_NAME.to_string(),
        label: pa_types::ai::SCHEME_TOOL_NAME.to_string(),
        description: "Evaluate Scheme (Steel, mostly R5RS) inside the running agent. Definitions persist across calls for the session. The value of the last form is returned, and `display`/`displayln` output is captured. `(host-request type payload)` sends a hash to the host and returns its reply as a hash, or raises its error. Starts instantly and each call costs well under a millisecond; there are no Python packages here.".to_string(),
        prompt_snippet: "eval - evaluate Scheme inside the running agent; definitions persist".to_string(),
        // One engine per session: its cells run one at a time.
        execution_mode: Some(ExecutionMode::Sequential),
        parameters: json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Scheme forms to evaluate in the session's engine, in order."
                },
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "Seconds to wait for this cell, max {PYTHON_REPL_TIMEOUT_MAX_S}. In a session that can be woken later, a cell still running then continues in the background and keeps the engine (the result names it and says how to read, wait for, or stop it), and the default is a window learned from this project's recent cells. In any other session the cell is interrupted, and the default is no limit."
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
        let tool =
            create_scheme_tool_definition(Arc::new(SchemeKernelProvisioner::new(handlers, "/")));
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
            json!({"code": "(display \"one \") (host-request \"update-shown\" (hash)) (display \"two\")"}),
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

    /// The shared result path describes a background cell in the Python
    /// kernel's terms; the `eval` tool's result must not.
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
        let tool = create_scheme_tool_definition(Arc::new(SchemeKernelProvisioner::with_options(
            SchemeKernelOptions {
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
            },
        )));
        let result = (tool.execute)(
            "call-1",
            json!({"code": "(display \"started\") (host-request \"gate-wait\" (hash))"}),
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
