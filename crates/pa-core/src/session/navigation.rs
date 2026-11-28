//! The session-tree moves and the reads beside them, as both daemons answer
//! them: where a `navigate_tree` lands and the text it gives back to the
//! editor, the user messages the `/fork` selector lists
//! (`get_user_messages_for_forking`), and the reply `/copy` and the agents
//! view's headline read (`get_last_assistant_text`). The old worker
//! (`pa-daemon` `branch_navigation.rs`, `worker/commands.rs`) and the runtime
//! daemon (`pa-runtime` `daemon/handlers/tree.rs`) both apply these rules to
//! the transcript's rows as they are stored (`{type, id, parentId, ...}`).

use serde_json::{json, Value};

/// A `navigate_tree` command's members, as the old worker read them: a
/// member that is missing or of another type is its default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavigateRequest {
    /// The entry the session's leaf moves to.
    pub target_id: String,
    /// Summarize the abandoned branch first.
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    /// The custom instructions replace the summary prompt's own.
    pub replace_instructions: bool,
    /// A label for the summary entry, or for the target when there is none.
    pub label: Option<String>,
    /// Put the workspace files back to the target's checkpoint too.
    pub restore_files: bool,
}

impl NavigateRequest {
    /// The request `payload` (the command's members) makes.
    #[must_use]
    pub fn from_payload(payload: &Value) -> Self {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        let flag = |key: &str| payload.get(key).and_then(Value::as_bool) == Some(true);
        NavigateRequest {
            target_id: text("targetId").unwrap_or_default(),
            summarize: flag("summarize"),
            custom_instructions: text("customInstructions"),
            replace_instructions: flag("replaceInstructions"),
            label: text("label"),
            restore_files: flag("restoreFiles"),
        }
    }
}

/// Where a navigation to the entry `target` (a stored row) puts the leaf,
/// and the text it hands back to the editor: a user message, or a custom
/// message with text, is re-entered with the leaf at its parent; any other
/// entry becomes the leaf itself.
#[must_use]
pub fn navigation_point(target: &Value) -> (Option<String>, Option<String>) {
    let parent = || target["parentId"].as_str().map(str::to_string);
    if let Some(text) = user_row_text(target) {
        return (parent(), Some(text));
    }
    if target["type"] == "custom_message" {
        let text = match target.get("content") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Array(_)) => Some(message_text(target)).filter(|text| !text.is_empty()),
            _ => None,
        };
        return (parent(), text);
    }
    (target["id"].as_str().map(str::to_string), None)
}

/// The `get_user_messages_for_forking` reply over the transcript's `rows`
/// (file order): `{messages: [{entryId, text}]}`, one per user message
/// with text.
#[must_use]
pub fn fork_messages<'a>(rows: impl IntoIterator<Item = &'a Value>) -> Value {
    let messages: Vec<Value> = rows
        .into_iter()
        .filter_map(|row| {
            let text = user_row_text(row).filter(|text| !text.is_empty())?;
            Some(json!({ "entryId": row["id"].as_str().unwrap_or_default(), "text": text }))
        })
        .collect();
    json!({ "messages": messages })
}

/// The `get_last_assistant_text` reply over the session's context
/// `messages` (the branch as the model sees it, oldest first): `{text}`, the
/// newest assistant message's text that is not empty (a turn's final
/// assistant message may hold only its tool call), `null` when there is
/// none.
#[must_use]
pub fn last_assistant_text<'a>(messages: impl DoubleEndedIterator<Item = &'a Value>) -> Value {
    let text = messages
        .rev()
        .filter(|message| message["role"] == "assistant")
        .map(|message| crate::session::catalog::content_to_text(&message["content"]))
        .find(|text| !text.is_empty());
    json!({ "text": text })
}

/// The text of a user-message row (its string content, or its text blocks
/// joined), `None` for any other row: the text a fork before it hands back.
#[must_use]
pub fn user_row_text(row: &Value) -> Option<String> {
    let message = row.get("message").filter(|_| row["type"] == "message")?;
    (message["role"] == "user").then(|| message_text(message))
}

/// The text of a message's (or a custom message row's) `content`: the
/// string, or its text blocks joined without a separator.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_navigation_lands_by_the_targets_kind() {
        let user = json!({ "type": "message", "id": "u", "parentId": "p",
            "message": { "role": "user", "content": [
                { "type": "text", "text": "a" }, { "type": "image" }, { "type": "text", "text": "b" },
            ] } });
        let custom = json!({ "type": "custom_message", "id": "c", "parentId": "u",
            "content": [{ "type": "image" }] });
        let reply = json!({ "type": "message", "id": "r", "parentId": "c",
            "message": { "role": "assistant", "content": "x" } });
        assert_eq!(
            [&user, &custom, &reply].map(navigation_point),
            [
                (Some("p".to_string()), Some("ab".to_string())),
                (Some("u".to_string()), None),
                (Some("r".to_string()), None),
            ]
        );
    }

    #[test]
    fn the_fork_list_is_the_user_messages_with_text() {
        let row = |id: &str, role: &str, text: &str| json!({ "type": "message", "id": id, "message": { "role": role, "content": text } });
        let rows = [
            row("u1", "user", "first prompt"),
            row("a1", "assistant", "answer"),
            row("u2", "user", ""),
            row("u3", "user", "second prompt"),
        ];
        assert_eq!(
            fork_messages(&rows),
            json!({ "messages": [
                { "entryId": "u1", "text": "first prompt" },
                { "entryId": "u3", "text": "second prompt" },
            ] })
        );
    }

    #[test]
    fn the_copy_text_is_the_newest_assistant_text() {
        let messages = [
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "one" },
                                                     { "type": "text", "text": "two" }] }),
            json!({ "role": "user", "content": "later" }),
            json!({ "role": "assistant", "content": [{ "type": "toolCall" }] }),
        ];
        assert_eq!(
            (
                last_assistant_text(messages.iter()),
                last_assistant_text([].iter())
            ),
            (json!({ "text": "one two" }), json!({ "text": null }))
        );
    }
}
