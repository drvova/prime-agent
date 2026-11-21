//! The `eval` tool: the model evaluates Scheme in the session's in-process
//! kernel ([`crate::kernel::scheme`]). Results render through the same path
//! as the Python kernel's cells, so the transcript shows both alike.

use std::sync::Arc;

use serde_json::json;

use crate::tools::ipython::{
    execute_ipython, timeout_arg_ms, IpythonKernelProvisioner, IpythonToolOptions,
    PYTHON_REPL_TIMEOUT_MAX_S,
};
use crate::tools::tool_definition::{ExecuteFn, ExecutionMode, ToolDefinition};

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
        Box::pin(async move {
            let code = params
                .get("code")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("eval tool requires a code string"))?
                .to_string();
            execute_ipython(
                &options,
                &code,
                timeout_arg_ms(&params),
                signal,
                on_update,
                None,
            )
            .await
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
                        "Seconds before the evaluation is interrupted. Default: no limit, max {PYTHON_REPL_TIMEOUT_MAX_S}."
                    )
                }
            }
        }),
        prepare_arguments: None,
        execute,
    }
}
