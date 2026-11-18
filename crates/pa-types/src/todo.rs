//! The session todo list's wire state, carried to every attached surface by the `todo_update`
//! session event and the attach snapshot's `state.todos` field, and persisted as the
//! `session_todo_list` custom entry (the newest entry on the branch wins). The list's host-request
//! handling lives in pa-core; the kernel reaches it through `todo.*`.

use serde::{Deserialize, Serialize};

/// The custom-entry type a todo list snapshot persists under.
pub const TODO_LIST_CUSTOM_TYPE: &str = "session_todo_list";

/// The session event type a todo list change rides.
pub const TODO_UPDATE_EVENT: &str = "todo_update";

/// One item's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    /// The wire slug (`"pending"`, `"in_progress"`, `"done"`).
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Done => "done",
        }
    }

    /// Parse a wire slug.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "done" => Some(TodoStatus::Done),
            _ => None,
        }
    }
}

/// One todo item: a stable id the kernel addresses it by, its text, status, and an optional note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The session todo list (empty when the session has none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// The number of `done` items.
    #[must_use]
    pub fn done_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == TodoStatus::Done)
            .count()
    }

    /// The `done/total` progress readout (`3/7`).
    #[must_use]
    pub fn progress(&self) -> String {
        format!("{}/{}", self.done_count(), self.items.len())
    }

    /// The item being worked on: the first `in_progress` item.
    #[must_use]
    pub fn active(&self) -> Option<&TodoItem> {
        self.items
            .iter()
            .find(|item| item.status == TodoStatus::InProgress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_shape_round_trips_with_snake_case_statuses() {
        let list = TodoList {
            items: vec![
                TodoItem {
                    id: "1".to_string(),
                    text: "read".to_string(),
                    status: TodoStatus::Done,
                    note: None,
                },
                TodoItem {
                    id: "2".to_string(),
                    text: "write".to_string(),
                    status: TodoStatus::InProgress,
                    note: Some("half".to_string()),
                },
            ],
        };
        let wire = serde_json::to_value(&list).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"items": [
                {"id": "1", "text": "read", "status": "done"},
                {"id": "2", "text": "write", "status": "in_progress", "note": "half"},
            ]})
        );
        assert_eq!(serde_json::from_value::<TodoList>(wire).unwrap(), list);
        assert_eq!(list.progress(), "1/2");
        assert_eq!(list.active().map(|item| item.id.as_str()), Some("2"));
    }
}
