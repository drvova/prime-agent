//! `session.mutate_queued_message {session, lane, index, expectedText,
//! mutation}` (the old daemon's `mutate_queued_message`): one delete, move
//! or replace of the row at `index` of `lane` (`"steering"` or `"followUp"`)
//! in the session's queue view, which must still show `expectedText`
//! (`crate::runtime::Runtime::edit_queued`). `mutation` is
//! `{"type": "delete"}`, `{"type": "move", "direction": <rows>}` or
//! `{"type": "replace", "text", "images"?, "lane"?}`.
//!
//! The answer is the old reply's data, `{"status": "applied" | "rejected"}`:
//! an edit that finds another row there, or cannot apply, is rejected. Only
//! a malformed request is an error, with the old daemon's message.

use pa_types::ai::ImageContent;
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::runtime::QueueEdit;
use crate::session::{QueueLane, SessionBuilder};

/// The lane a wire lane name names.
fn wire_lane(value: Option<&Value>) -> Option<QueueLane> {
    match value.and_then(Value::as_str) {
        Some("steering") => Some(QueueLane::Steering),
        Some("followUp") => Some(QueueLane::FollowUp),
        _ => None,
    }
}

/// The images a replace carries (the old daemon's `parse_prompt_images`:
/// each `{"type": "image", "data", "mimeType"}`, the rest skipped).
fn images(mutation: &Value) -> Vec<ImageContent> {
    mutation
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|image| image["type"] == "image")
        .filter_map(|image| {
            Some(ImageContent {
                data: image.get("data")?.as_str()?.to_string(),
                mime_type: image.get("mimeType")?.as_str()?.to_string(),
                rest: serde_json::Map::default(),
            })
        })
        .collect()
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.mutate_queued_message` (see the module documentation).
    pub(super) async fn mutate_queued_message(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let lane = wire_lane(args.get("lane")).ok_or_else(|| {
            anyhow::anyhow!("mutate_queued_message requires lane \"steering\" or \"followUp\"")
        })?;
        let index = args
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| anyhow::anyhow!("mutate_queued_message requires an index"))?;
        let expected = args
            .get("expectedText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mutation = args
            .get("mutation")
            .ok_or_else(|| anyhow::anyhow!("mutate_queued_message requires a mutation"))?;
        let edit = match mutation.get("type").and_then(Value::as_str) {
            Some("delete") => QueueEdit::Delete,
            Some("move") => QueueEdit::Move {
                direction: mutation
                    .get("direction")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            },
            Some("replace") => {
                // A replace without its text cannot apply.
                let Some(text) = mutation.get("text").and_then(Value::as_str) else {
                    return Ok(json!({ "status": "rejected" }));
                };
                QueueEdit::Replace {
                    text: text.to_string(),
                    images: mutation.get("images").map(|_| images(mutation)),
                    lane: wire_lane(mutation.get("lane")),
                }
            }
            _ => anyhow::bail!(
                "mutate_queued_message requires mutation.type \"delete\", \"move\", or \"replace\""
            ),
        };
        let edited = self
            .runtime
            .edit_queued(&id, lane, index, expected, edit)
            .await?;
        Ok(json!({ "status": edited.status() }))
    }
}
