//! The display transcript's row rules, shared by the old daemon's session
//! store (`pa-daemon` `session_store/view.rs`) and the runtime daemon's
//! attach snapshot and `transcript.page` (`pa-runtime`
//! `daemon/handlers/attach.rs`), so both serve the same rows and cut their
//! pages in the same places:
//!
//! - a row's wire message: a `custom_message` row as `role: "custom"`
//!   ([`custom_message_row`]), a compaction as its `compactionSummary`
//!   message ([`compaction_summary_message`]);
//! - where a page may start ([`continues_run`], [`settle_cut`]);
//! - the attach snapshot's image-payload elision
//!   ([`elide_image_payloads`]) and its resume marker's prompt time
//!   ([`prompt_timestamp_ms`]).
//!
//! A row's `fields` here are its stored members other than `type`, `id`,
//! `parentId` and `timestamp`.

use std::borrow::Cow;

use serde_json::{json, Value};

/// One display-transcript row: its entry id and wire message.
pub type DisplayRow<'a> = (&'a str, Cow<'a, Value>);

/// A `custom_message` row's wire message: its fields with `role: "custom"`
/// and the row's `timestamp`.
#[must_use]
pub fn custom_message_row(fields: &Value, timestamp: &str) -> Value {
    let mut message = fields.clone();
    if let Some(object) = message.as_object_mut() {
        object.insert("role".to_string(), Value::String("custom".to_string()));
        object.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_string()),
        );
    }
    message
}

/// The `compactionSummary` message of a compaction row (`timestamp_ms`, its
/// time); the context fold's copy carries its retained count.
#[must_use]
pub fn compaction_summary_message(
    fields: &Value,
    timestamp_ms: u64,
    retained_count: Option<usize>,
) -> Value {
    // The TS `compactionSummary` key order: role, summary, tokensBefore,
    // retainedMessageCount, customInstructions?, harnessDigest?, timestamp —
    // optional keys insert before `timestamp`.
    let mut message = json!({
        "role": "compactionSummary",
        "summary": fields.get("summary").cloned().unwrap_or_default(),
        "tokensBefore": fields.get("tokensBefore").cloned().unwrap_or(json!(0)),
    });
    if let Some(retained_count) = retained_count {
        message["retainedMessageCount"] = json!(retained_count as u64);
    }
    if let Some(custom_instructions) = fields.get("customInstructions") {
        message["customInstructions"] = custom_instructions.clone();
    }
    if let Some(harness_digest) = fields.get("harnessDigest") {
        message["harnessDigest"] = harness_digest.clone();
        if let Some(harness_state_fingerprint) = fields.get("harnessStateFingerprint") {
            message["harnessStateFingerprint"] = harness_state_fingerprint.clone();
        }
    }
    message["timestamp"] = json!(timestamp_ms);
    message
}

/// A user prompt's wall-clock time, mirroring the TUI's numeric/ISO
/// timestamp contract.
#[must_use]
pub fn prompt_timestamp_ms(message: &Value) -> Option<u64> {
    match message.get("timestamp") {
        Some(Value::Number(number)) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite())
                .map(|value| value.max(0.0) as u64)
        }),
        Some(Value::String(iso)) => pa_types::incident::timestamp_to_ms(iso)
            .filter(|ms| *ms > 0)
            .map(|ms| ms as u64),
        _ => None,
    }
}

fn message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// Whether a row continues the run before it: a page never starts at one.
#[must_use]
pub fn continues_run(message: &Value) -> bool {
    matches!(message_role(message), Some("toolResult" | "custom"))
}

/// Move a page boundary earlier until the page below it starts at neither a tool
/// result nor a custom row, and no tool call is split from its result.
///
/// `rows` is a local slice of the branch around the cut, not the whole branch: the
/// page, the run before its first row, and up to `limit` rows past each side, the
/// side toward the request's other anchor stopping at that anchor row. Calls pair
/// with results only inside the slice, so a call more than a page from its result
/// does not move the cut (else an orphan or aborted call drags every page to the root).
#[must_use]
pub fn settle_cut(rows: &[DisplayRow<'_>], mut cut: usize) -> usize {
    while cut > 0 && cut < rows.len() && continues_run(&rows[cut].1) {
        cut -= 1;
    }
    // User bash and other message rows can land between a call and its
    // result. Match the same last-pending call as transcript replay, then
    // move only across pairs intersecting the cut. Reverse result order
    // also catches an earlier pair exposed by moving the boundary back.
    let mut pending: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    let mut pairs = Vec::new();
    for (index, (_, message)) in rows.iter().enumerate() {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        if let Some(id) = block.get("id").and_then(Value::as_str).filter(|_| {
                            block.get("type").and_then(Value::as_str) == Some("toolCall")
                        }) {
                            pending.entry(id).or_default().push(index);
                        }
                    }
                }
            }
            Some("toolResult") => {
                if let Some(call) = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|id| pending.get_mut(id))
                    .and_then(Vec::pop)
                {
                    pairs.push((call, index));
                }
            }
            _ => {}
        }
    }
    for (call, result) in pairs.into_iter().rev() {
        if call < cut && result >= cut {
            cut = call;
        }
    }
    cut
}

/// The attach snapshot's image-payload elision (`elide_snapshot_images`
/// clients): a `toolResult` image block travels with its payload replaced by
/// an empty string plus `elidedBytes` and `widthPx`/`heightPx`. The
/// transcript's image rows are fallback-only metadata, so an image-heavy
/// session stops shipping megabytes of base64 per attach; the store keeps the
/// payload.
pub fn elide_image_payloads(messages: &mut [Value]) {
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("toolResult") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut() {
            let is_image = block.get("type").and_then(Value::as_str) == Some("image");
            // The immutable reads (payload length, the bounded-prefix dimension parse) end
            // here; the marker writes below borrow the block mutably.
            let (payload_chars, dimensions) = match (
                is_image,
                block.get("data").and_then(Value::as_str),
                block.get("mimeType").and_then(Value::as_str),
            ) {
                (true, Some(data), Some(mime)) if !data.is_empty() => (
                    data.len(),
                    crate::get_image_dimensions_prefix(
                        data,
                        mime,
                        crate::IMAGE_DIMENSIONS_PREFIX_BYTES,
                    ),
                ),
                _ => continue,
            };
            let Some(object) = block.as_object_mut() else {
                continue;
            };
            object.insert("elidedBytes".to_string(), json!(payload_chars));
            if let Some(dimensions) = dimensions {
                object.insert("widthPx".to_string(), json!(dimensions.width_px));
                object.insert("heightPx".to_string(), json!(dimensions.height_px));
            }
            object.insert("data".to_string(), json!(""));
        }
    }
}
