//! The kernel-reset notice (`ipython_state_restored`): a session's Scheme
//! kernel is an engine inside the process that runs it, so a resumed session
//! (and one whose idle kernel was released) starts on a new engine and
//! nothing its earlier cells defined comes back (Steel cannot serialize
//! closures, and replaying cells would repeat their side effects). The model
//! is told so once, ahead of its next turn: on a resume when the transcript
//! shows it used the kernel since it was last told, on a release when its
//! cells had defined anything.

use pa_types::session::{AgentMessage, CustomMessage};

/// The row's custom type: a wire identifier clients and saved sessions
/// already know, kept from the Python kernel's restore notice.
pub const KERNEL_STATE_RESTORED_CUSTOM_TYPE: &str = "ipython_state_restored";

/// The notice text.
pub const NOTICE_CONTENT: &str = "[kernel-state-reset]\n\nThis session's Scheme kernel was restarted and is a new engine: everything your earlier `eval` cells defined (functions, variables, loaded data) is gone, and so are the cells and shell commands that were running in the background. Define again what you still need before you use it. Files on disk, the session's todo list and persistent shells are unchanged.";

/// The notice for a session resumed with the context `earlier`, or `None`
/// when the model has nothing to lose: it ran no kernel cell (in either
/// kernel language) and saw no kernel-state notice since the last time it
/// was told, or ever.
#[must_use]
pub fn notice_for_resumed(earlier: &[AgentMessage]) -> Option<CustomMessage> {
    let used_since_told = earlier
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result)
                if pa_types::ai::is_kernel_tool_name(&result.tool_name) =>
            {
                Some(true)
            }
            AgentMessage::Custom(row)
                if row.custom_type == super::kernel_state_notice::KERNEL_STATE_CUSTOM_TYPE =>
            {
                Some(true)
            }
            AgentMessage::Custom(row) if row.custom_type == KERNEL_STATE_RESTORED_CUSTOM_TYPE => {
                Some(false)
            }
            AgentMessage::User(_)
            | AgentMessage::Assistant(_)
            | AgentMessage::ToolResult(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_) => None,
        })
        .unwrap_or(false);
    used_since_told.then(notice_message)
}

/// The next-turn notice row. `details.restored` is always false (nothing
/// is ever revived), which clients show as a fresh kernel.
#[must_use]
pub fn notice_message() -> CustomMessage {
    CustomMessage {
        custom_type: KERNEL_STATE_RESTORED_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(NOTICE_CONTENT.to_string()),
        display: true,
        details: Some(serde_json::json!({ "restored": false })),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default(),
        rest: serde_json::Map::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_result(tool_name: &str) -> AgentMessage {
        serde_json::from_value(serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": tool_name,
            "content": [{ "type": "text", "text": "12" }],
            "isError": false,
            "timestamp": 1,
        }))
        .unwrap()
    }

    fn row(custom_type: &str) -> AgentMessage {
        AgentMessage::Custom(CustomMessage {
            custom_type: custom_type.to_string(),
            content: pa_types::ai::UserContent::Text(String::new()),
            display: true,
            details: None,
            timestamp: 1,
            rest: serde_json::Map::default(),
        })
    }

    /// The notice is owed exactly when a kernel cell (of either kernel), or
    /// a compaction's kernel-state row, is newer than the last notice.
    #[test]
    fn a_resumed_session_is_told_once_per_restart_that_lost_kernel_state() {
        let told = row(KERNEL_STATE_RESTORED_CUSTOM_TYPE);
        let owed = |earlier: &[AgentMessage]| {
            notice_for_resumed(earlier).map(|notice| (notice.content, notice.details))
        };
        let notice = Some((
            pa_types::ai::UserContent::Text(NOTICE_CONTENT.to_string()),
            Some(serde_json::json!({ "restored": false })),
        ));
        assert_eq!(
            [
                owed(&[]),
                owed(&[cell_result("bash")]),
                owed(&[cell_result("eval")]),
                owed(&[cell_result("python_repl")]),
                owed(&[row("ipython_state")]),
                owed(&[cell_result("eval"), told.clone()]),
                owed(&[cell_result("eval"), told, cell_result("eval")]),
            ],
            [
                None,
                None,
                notice.clone(),
                notice.clone(),
                notice.clone(),
                None,
                notice,
            ]
        );
    }
}
