//! RLM child lifecycle notices: the durable custom rows a parent session receives when
//! a child run ends without an explicit reply, fails, or is cancelled; this module owns
//! the wire vocabulary, the content text, and the details block.

use super::agent_messaging::sanitize_message_header_value;
use pa_types::ai::UserContent;
use pa_types::session::CustomMessage;
use serde_json::json;

/// A child run that failed (TS `rlm_child_failure`).
pub const RLM_CHILD_FAILURE_CUSTOM_TYPE: &str = "rlm_child_failure";
/// A child run that ended without an explicit reply or was cancelled
/// (TS `rlm_child_terminal_notice`).
pub const RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE: &str = "rlm_child_terminal_notice";

/// How a child run ended without an explicit reply (TS
/// `RlmChildTerminalNoticeDetails`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RlmChildTerminalNotice {
    /// The parent deleted a still-running child.
    Cancelled {
        child_id: String,
        session_name: String,
        reason: Option<String>,
    },
    /// The child finished its task without sending an agent message back.
    CompletedWithoutReply {
        child_id: String,
        session_name: String,
        last_assistant_text_preview: Option<String>,
    },
}

/// The terminal-notice row: `[child-exited: cancelled|no-reply child:<name>]`
/// with the reason or the child's last assistant text as the body.
#[must_use]
pub fn create_rlm_child_terminal_notice(
    notice: &RlmChildTerminalNotice,
    timestamp: u64,
) -> CustomMessage {
    let (content, details) = match notice {
        RlmChildTerminalNotice::Cancelled {
            child_id,
            session_name,
            reason,
        } => {
            let name = sanitize_message_header_value(session_name);
            let mut content = format!("[child-exited: cancelled child:{name}]");
            if let Some(reason) = reason.as_deref().filter(|reason| !reason.is_empty()) {
                content.push_str("\n\n");
                content.push_str(reason);
            }
            let details = json!({
                "kind": "cancelled",
                "childId": child_id,
                "sessionName": session_name,
                "reason": reason,
            });
            (content, details)
        }
        RlmChildTerminalNotice::CompletedWithoutReply {
            child_id,
            session_name,
            last_assistant_text_preview,
        } => {
            let name = sanitize_message_header_value(session_name);
            let mut content = format!("[child-exited: no-reply child:{name}]");
            if let Some(preview) = last_assistant_text_preview
                .as_deref()
                .filter(|preview| !preview.is_empty())
            {
                content.push_str("\n\nLast assistant text: ");
                content.push_str(preview);
            }
            let details = json!({
                "kind": "completed_without_reply",
                "childId": child_id,
                "sessionName": session_name,
                "lastAssistantTextPreview": last_assistant_text_preview,
            });
            (content, details)
        }
    };
    CustomMessage {
        custom_type: RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(content),
        display: true,
        details: Some(details),
        timestamp,
        rest: serde_json::Map::default(),
    }
}

/// The failure row: `[child-failed child:<name>]` with the error text.
#[must_use]
pub fn create_rlm_child_failure_message(
    child_id: &str,
    session_name: &str,
    error: &str,
    timestamp: u64,
) -> CustomMessage {
    let name = sanitize_message_header_value(session_name);
    let content = format!("[child-failed child:{name}]\n\n{error}");
    CustomMessage {
        custom_type: RLM_CHILD_FAILURE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(content),
        display: true,
        details: Some(json!({
            "childId": child_id,
            "sessionName": session_name,
            "error": error,
        })),
        timestamp,
        rest: serde_json::Map::default(),
    }
}

/// Cap on the answer preview handed to the parent model.
pub const ANSWER_PREVIEW_MAX_CHARS: usize = 160;
/// Cap on the FULL final-answer text the collect envelope carries (the
/// settle-binding data plane): the roster preview stays
/// [`ANSWER_PREVIEW_MAX_CHARS`], while a consumer that binds outputs from
/// the answer (the factory's output ports) needs the whole fenced JSON,
/// so the lane is bounded instead of unbounded, never ballooning the
/// collect reply for a runaway answer.
pub const ANSWER_TEXT_MAX_CHARS: usize = 65_536;
const ELLIPSIS: &str = "...";

/// Collapse whitespace and cap at the roster limit.
#[must_use]
pub fn compact_rlm_text(text: &str) -> String {
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cap_text(&compact, ANSWER_PREVIEW_MAX_CHARS)
}

/// Whitespace-collapsed text capped at `max` chars with an ellipsis.
#[must_use]
pub fn cap_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max - ELLIPSIS.len()).collect();
    format!("{}{}", kept.trim_end(), ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binding_lane_keeps_far_more_than_the_preview_cap() {
        // The collect envelope's full-answer lane: a fenced JSON output
        // longer than the 160-character roster preview binds whole from
        // it, so the bound stays far above the preview cap and only a
        // genuinely oversized payload (over ANSWER_TEXT_MAX_CHARS) sees
        // the ellipsis.
        let long = "y".repeat(ANSWER_TEXT_MAX_CHARS + 50);
        let capped = cap_text(&long, ANSWER_TEXT_MAX_CHARS);
        assert_eq!(capped.chars().count(), ANSWER_TEXT_MAX_CHARS);
        assert!(capped.ends_with("..."));
        let wide_json = format!(
            "{{\"blob\": \"{}\"}}",
            "z".repeat(ANSWER_PREVIEW_MAX_CHARS * 4)
        );
        let lane = cap_text(&wide_json, ANSWER_TEXT_MAX_CHARS);
        assert_eq!(lane, wide_json, "a wide-but-bounded JSON keeps its tail");
    }

    #[test]
    fn no_reply_notice_matches_the_ts_shape() {
        let notice = RlmChildTerminalNotice::CompletedWithoutReply {
            child_id: "sub-8fb5284a".to_string(),
            session_name: "f20-worker".to_string(),
            last_assistant_text_preview: Some("done with the task".to_string()),
        };
        let message = create_rlm_child_terminal_notice(&notice, 1_000);
        assert_eq!(message.custom_type, "rlm_child_terminal_notice");
        assert!(message.display);
        assert_eq!(
            message.content,
            UserContent::Text(
                "[child-exited: no-reply child:f20-worker]\n\nLast assistant text: done with the task"
                    .to_string()
            )
        );
        let details = message.details.unwrap();
        assert_eq!(details["kind"], "completed_without_reply");
        assert_eq!(details["childId"], "sub-8fb5284a");
        assert_eq!(details["sessionName"], "f20-worker");
        assert_eq!(details["lastAssistantTextPreview"], "done with the task");
    }

    #[test]
    fn cancelled_notice_without_reason_has_no_body() {
        let notice = RlmChildTerminalNotice::Cancelled {
            child_id: "sub-1".to_string(),
            session_name: "worker [x]".to_string(),
            reason: None,
        };
        let message = create_rlm_child_terminal_notice(&notice, 2_000);
        assert_eq!(
            message.content,
            UserContent::Text("[child-exited: cancelled child:worker x]".to_string())
        );
        let details = message.details.unwrap();
        assert_eq!(details["kind"], "cancelled");
        assert_eq!(details["reason"], serde_json::Value::Null);
    }

    #[test]
    fn cancelled_notice_carries_the_reason_body() {
        let notice = RlmChildTerminalNotice::Cancelled {
            child_id: "sub-1".to_string(),
            session_name: "worker".to_string(),
            reason: Some("Deleted by parent orchestrator".to_string()),
        };
        let message = create_rlm_child_terminal_notice(&notice, 3_000);
        assert_eq!(
            message.content,
            UserContent::Text(
                "[child-exited: cancelled child:worker]\n\nDeleted by parent orchestrator"
                    .to_string()
            )
        );
    }

    #[test]
    fn failure_notice_matches_the_ts_shape() {
        let message = create_rlm_child_failure_message("sub-9", "lane", "boom", 4_000);
        assert_eq!(message.custom_type, "rlm_child_failure");
        assert_eq!(
            message.content,
            UserContent::Text("[child-failed child:lane]\n\nboom".to_string())
        );
        let details = message.details.unwrap();
        assert_eq!(details["childId"], "sub-9");
        assert_eq!(details["error"], "boom");
    }
}
