//! The `todo.get` / `todo.set` / `todo.update` host requests: validation, the persisted snapshot
//! row, and the change announcement.
//!
//! Payloads (camelCase, the kernel's `rlm.todo` client):
//! - `todo.set {items: [{text, id?, status?, note?}]}` replaces the list (an empty list clears
//!   it); a missing id becomes the next free number, a missing status `pending`.
//! - `todo.update {id, status?, text?, note?}` patches one item; `note: ""` clears its note.
//!
//! Every request replies with the whole list (`{items: [...]}`).

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use pa_types::session::FileEntry;
use pa_types::todo::{TodoItem, TodoList, TodoStatus, TODO_LIST_CUSTOM_TYPE};
use serde_json::Value;

use super::{lock, InteractionEvent, InteractionHub};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::session::manager::SessionManager;

/// The most items one list holds: a plan, not a backlog.
const MAX_ITEMS: usize = 100;

/// The longest item text or note, in characters.
const MAX_TEXT_CHARS: usize = 500;

/// The todo list a `session_todo_list` custom entry persisted, if `entry` is one.
pub(crate) fn persisted_todo_list(entry: &FileEntry) -> Option<TodoList> {
    let FileEntry::Custom { payload, .. } = entry else {
        return None;
    };
    if payload.custom_type != TODO_LIST_CUSTOM_TYPE {
        return None;
    }
    serde_json::from_value(payload.data.clone()?).ok()
}

pub(super) fn register(
    hub: &Arc<InteractionHub>,
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    handlers: &mut HostRequestHandlers,
) {
    for request_type in ["todo.get", "todo.set", "todo.update"] {
        let hub = Arc::clone(hub);
        let session = Arc::clone(session);
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let hub = Arc::clone(&hub);
                let session = Arc::clone(&session);
                async move {
                    let list = match request_type {
                        "todo.set" => commit(&hub, &session, |_| parse_list(&payload.data)).await?,
                        "todo.update" => {
                            commit(&hub, &session, |current| patched(current, &payload.data))
                                .await?
                        }
                        _ => hub.todos(),
                    };
                    Ok(serde_json::to_value(list)?)
                }
            }),
        );
    }
}

/// Build the next list from the current one, persist it as the branch's newest snapshot, then
/// adopt and announce it. The session lock serializes concurrent mutations (a kernel can issue
/// several host requests at once), and the row lands first: a failed write leaves the previous
/// list in force and surfaces the error to the kernel.
async fn commit(
    hub: &InteractionHub,
    session: &tokio::sync::Mutex<SessionManager>,
    next: impl FnOnce(&TodoList) -> anyhow::Result<TodoList>,
) -> anyhow::Result<TodoList> {
    let mut session = session.lock().await;
    let list = next(&hub.todos())?;
    session
        .append_custom_entry(TODO_LIST_CUSTOM_TYPE, Some(serde_json::to_value(&list)?))
        .context("persisting the todo list")?;
    drop(session);
    let previous = std::mem::replace(&mut *lock(&hub.todos), list.clone());
    if let Some(counters) = hub.counters() {
        counters.note_todo_change(&previous, &list);
    }
    hub.publish(InteractionEvent::TodosChanged(list.clone()));
    Ok(list)
}

fn parse_list(data: &Value) -> anyhow::Result<TodoList> {
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("todo.set requires an items list"))?;
    if items.len() > MAX_ITEMS {
        bail!("a todo list holds at most {MAX_ITEMS} items");
    }
    let mut seen = HashSet::new();
    for item in items {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            if !seen.insert(id.to_string()) {
                bail!("duplicate todo id \"{id}\"");
            }
        }
    }
    let mut next_id = 1usize;
    let mut parsed = Vec::with_capacity(items.len());
    for item in items {
        let id = match item.get("id") {
            None | Some(Value::Null) => loop {
                let candidate = next_id.to_string();
                next_id += 1;
                if seen.insert(candidate.clone()) {
                    break candidate;
                }
            },
            Some(Value::String(id)) if !id.trim().is_empty() => id.clone(),
            Some(_) => bail!("a todo id must be a non-empty string"),
        };
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("todo \"{id}\" needs text"))?;
        parsed.push(TodoItem {
            text: checked_text(text, "text")?,
            status: match item.get("status") {
                None | Some(Value::Null) => TodoStatus::Pending,
                Some(value) => parse_status(value)?,
            },
            note: match item.get("note") {
                None | Some(Value::Null) => None,
                Some(value) => parse_note(value)?,
            },
            id,
        });
    }
    Ok(TodoList { items: parsed })
}

fn patched(current: &TodoList, data: &Value) -> anyhow::Result<TodoList> {
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("todo.update requires an id"))?;
    let mut list = current.clone();
    let Some(item) = list.items.iter_mut().find(|item| item.id == id) else {
        let known: Vec<&str> = current.items.iter().map(|item| item.id.as_str()).collect();
        bail!(
            "no todo with id \"{id}\"; the list holds: {}",
            if known.is_empty() {
                "nothing (call todo.set first)".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    if let Some(value) = data.get("status").filter(|value| !value.is_null()) {
        item.status = parse_status(value)?;
    }
    if let Some(text) = data.get("text").filter(|value| !value.is_null()) {
        let text = text
            .as_str()
            .ok_or_else(|| anyhow!("todo text must be a string"))?;
        item.text = checked_text(text, "text")?;
    }
    if let Some(value) = data.get("note").filter(|value| !value.is_null()) {
        item.note = parse_note(value)?;
    }
    Ok(list)
}

fn parse_status(value: &Value) -> anyhow::Result<TodoStatus> {
    value
        .as_str()
        .and_then(TodoStatus::from_slug)
        .ok_or_else(|| anyhow!("todo status must be one of pending, in_progress, done"))
}

/// A note: an empty string clears it.
fn parse_note(value: &Value) -> anyhow::Result<Option<String>> {
    let note = value
        .as_str()
        .ok_or_else(|| anyhow!("todo note must be a string"))?;
    if note.trim().is_empty() {
        return Ok(None);
    }
    checked_text(note, "note").map(Some)
}

fn checked_text(text: &str, field: &str) -> anyhow::Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("todo {field} must not be empty");
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        bail!("todo {field} is longer than {MAX_TEXT_CHARS} characters");
    }
    Ok(text.to_string())
}
