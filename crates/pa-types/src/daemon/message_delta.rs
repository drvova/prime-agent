//! Delta-only `message_update` frames: the client-hop codec behind the
//! [`MESSAGE_UPDATE_DELTA_CAPABILITY`] attach capability.
//!
//! A streamed assistant `message_update` normally carries the whole message
//! so far, so a reply's socket traffic grows with the square of its length.
//! A client that advertises the capability instead receives, between full
//! anchors, an event whose `message` is replaced by a `messageDelta`:
//!
//! ```json
//! { "type": "message_update",
//!   "messageDelta": {
//!     "content": [ { "index": 0, "field": "text", "from": 12, "append": "more" },
//!                  { "index": 1, "part": { "type": "toolCall", "...": "..." } } ],
//!     "set": { "usage": { "...": "..." } } },
//!   "assistantMessageEvent": { "...": "..." } }
//! ```
//!
//! - `content[]` ops apply in order: `append` extends the named string field
//!   of an existing part (`from` is the field's byte length before the append,
//!   checked by the decoder); `part` replaces or adds a whole part.
//! - `set` replaces top-level message fields other than `content`.
//!
//! Full frames still go out for `message_start`/`message_end`, for the first
//! update a connection sees for a session (after every attach/reattach), every
//! [`MESSAGE_DELTA_ANCHOR_INTERVAL`] updates, and whenever the change is not
//! expressible as the ops above (a part removed, a field dropped). The encoder
//! diffs against the last frame *this connection wrote*, and the decoder
//! applies against the last frame *it read*, so dropped or lagged frames
//! upstream of the writer cannot desynchronize the two sides.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

/// The attach capability a client advertises to receive delta updates.
pub const MESSAGE_UPDATE_DELTA_CAPABILITY: &str = "message_update_delta";

/// A full `message_update` goes out after this many consecutive deltas
/// (a safety anchor: a desynchronized decoder heals within one interval).
pub const MESSAGE_DELTA_ANCHOR_INTERVAL: u32 = 64;

/// The streamed message events whose payload is a whole assistant message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageEventKind {
    Start,
    Update,
    End,
}

fn message_event_kind(event: &Map<String, Value>) -> Option<MessageEventKind> {
    match event.get("type").and_then(Value::as_str)? {
        "message_start" => Some(MessageEventKind::Start),
        "message_update" => Some(MessageEventKind::Update),
        "message_end" => Some(MessageEventKind::End),
        _ => None,
    }
}

fn is_assistant(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
}

/// The session a `session_event` envelope belongs to, when it is one.
fn session_of(envelope: &Value) -> Option<&str> {
    if envelope.get("type").and_then(Value::as_str) != Some("session_event") {
        return None;
    }
    envelope.get("activeSessionId").and_then(Value::as_str)
}

/// The last message written for one session, and the deltas since its anchor.
struct EncoderSlot {
    base: Value,
    deltas_since_anchor: u32,
}

/// Per-connection encoder on the serving side. One instance per client
/// connection that advertised [`MESSAGE_UPDATE_DELTA_CAPABILITY`]; every
/// `session_event` that connection writes passes through
/// [`MessageDeltaEncoder::encode`] in write order.
#[derive(Default)]
pub struct MessageDeltaEncoder {
    sessions: HashMap<String, EncoderSlot>,
}

impl MessageDeltaEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The envelope to write in place of `envelope`, or `None` when it goes
    /// out unchanged (every frame but a delta-encodable assistant update).
    pub fn encode(&mut self, envelope: &Value) -> Option<Value> {
        let session = session_of(envelope)?;
        let event = envelope.get("event")?.as_object()?;
        let kind = message_event_kind(event)?;
        let message = event
            .get("message")
            .filter(|message| is_assistant(message))?;
        match kind {
            MessageEventKind::End => {
                self.sessions.remove(session);
                None
            }
            MessageEventKind::Start => {
                self.anchor(session, message);
                None
            }
            MessageEventKind::Update => {
                let delta = self
                    .sessions
                    .get(session)
                    .filter(|slot| slot.deltas_since_anchor < MESSAGE_DELTA_ANCHOR_INTERVAL)
                    .and_then(|slot| message_delta(&slot.base, message));
                let Some(delta) = delta else {
                    self.anchor(session, message);
                    return None;
                };
                if let Some(slot) = self.sessions.get_mut(session) {
                    slot.base = message.clone();
                    slot.deltas_since_anchor += 1;
                }
                let mut encoded_event = event.clone();
                encoded_event.remove("message");
                encoded_event.insert("messageDelta".to_string(), delta);
                let mut encoded = envelope.as_object()?.clone();
                encoded.insert("event".to_string(), Value::Object(encoded_event));
                Some(Value::Object(encoded))
            }
        }
    }

    fn anchor(&mut self, session: &str, message: &Value) {
        self.sessions.insert(
            session.to_string(),
            EncoderSlot {
                base: message.clone(),
                deltas_since_anchor: 0,
            },
        );
    }
}

/// The single string field by which `new` extends `old` (every other field
/// equal), as `(field, appended suffix)`.
fn appended_field<'a>(old: &Value, new: &'a Value) -> Option<(&'a str, &'a str)> {
    let (old, new) = (old.as_object()?, new.as_object()?);
    if old.len() != new.len() {
        return None;
    }
    let mut appended = None;
    for (key, new_value) in new {
        let old_value = old.get(key)?;
        if old_value == new_value {
            continue;
        }
        if appended.is_some() {
            return None;
        }
        let (old_text, new_text) = (old_value.as_str()?, new_value.as_str()?);
        appended = Some((key.as_str(), new_text.strip_prefix(old_text)?));
    }
    appended
}

/// The delta taking `base` to `message`, or `None` when only a full frame
/// can express it.
fn message_delta(base: &Value, message: &Value) -> Option<Value> {
    let (base, message) = (base.as_object()?, message.as_object()?);
    if base.keys().any(|key| !message.contains_key(key)) {
        return None;
    }
    let mut set = Map::new();
    for (key, value) in message {
        if key != "content" && base.get(key) != Some(value) {
            set.insert(key.clone(), value.clone());
        }
    }
    let mut ops = Vec::new();
    match (base.get("content"), message.get("content")) {
        (Some(Value::Array(old_parts)), Some(Value::Array(new_parts))) => {
            if new_parts.len() < old_parts.len() {
                return None;
            }
            for (index, part) in new_parts.iter().enumerate() {
                let Some(old_part) = old_parts.get(index) else {
                    ops.push(json!({ "index": index, "part": part }));
                    continue;
                };
                if old_part == part {
                    continue;
                }
                let append = appended_field(old_part, part).and_then(|(field, suffix)| {
                    let from = old_part.get(field)?.as_str()?.len();
                    Some(json!({ "index": index, "field": field, "from": from, "append": suffix }))
                });
                ops.push(append.unwrap_or_else(|| json!({ "index": index, "part": part })));
            }
        }
        (old_content, Some(new_content)) => {
            if old_content != Some(new_content) {
                set.insert("content".to_string(), new_content.clone());
            }
        }
        (_, None) => {}
    }
    let mut delta = Map::new();
    if !ops.is_empty() {
        delta.insert("content".to_string(), Value::Array(ops));
    }
    if !set.is_empty() {
        delta.insert("set".to_string(), Value::Object(set));
    }
    Some(Value::Object(delta))
}

/// What [`MessageDeltaDecoder::decode`] did to one envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaDecodeOutcome {
    /// Not a delta frame: the envelope is unchanged (its full message, if
    /// any, became the session's new base).
    Passthrough,
    /// A delta was applied: the envelope now carries the full `message`.
    Restored,
    /// The delta did not match the base (no base, an index past the end, a
    /// `from` offset mismatch). The event keeps no `message`, so consumers
    /// skip it; the next full frame re-anchors the session.
    Desynchronized,
}

/// Per-connection decoder on the client side: restores the full `message`
/// of every delta `message_update`, so everything downstream sees exactly
/// the frames a non-capable client would receive.
#[derive(Default)]
pub struct MessageDeltaDecoder {
    sessions: HashMap<String, Value>,
}

impl MessageDeltaDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore `envelope` in place; every inbound frame of the connection
    /// passes through here in read order, before any filtering.
    pub fn decode(&mut self, envelope: &mut Value) -> DeltaDecodeOutcome {
        let Some(session) = session_of(envelope).map(str::to_string) else {
            return DeltaDecodeOutcome::Passthrough;
        };
        let Some(event) = envelope.get_mut("event").and_then(Value::as_object_mut) else {
            return DeltaDecodeOutcome::Passthrough;
        };
        if let Some(delta) = event.remove("messageDelta") {
            let restored = self
                .sessions
                .remove(&session)
                .and_then(|base| apply_delta(base, &delta));
            let Some(message) = restored else {
                return DeltaDecodeOutcome::Desynchronized;
            };
            event.insert("message".to_string(), message.clone());
            self.sessions.insert(session, message);
            return DeltaDecodeOutcome::Restored;
        }
        let kind = message_event_kind(event);
        let Some(message) = event.get("message").filter(|message| is_assistant(message)) else {
            return DeltaDecodeOutcome::Passthrough;
        };
        match kind {
            Some(MessageEventKind::End) => {
                self.sessions.remove(&session);
            }
            Some(MessageEventKind::Start | MessageEventKind::Update) => {
                self.sessions.insert(session, message.clone());
            }
            None => {}
        }
        DeltaDecodeOutcome::Passthrough
    }
}

/// Apply one `messageDelta` to its base; `None` on any mismatch.
fn apply_delta(mut base: Value, delta: &Value) -> Option<Value> {
    let message = base.as_object_mut()?;
    if let Some(ops) = delta.get("content") {
        let parts = message.get_mut("content")?.as_array_mut()?;
        for op in ops.as_array()? {
            let index = usize::try_from(op.get("index")?.as_u64()?).ok()?;
            if let Some(part) = op.get("part") {
                match index.cmp(&parts.len()) {
                    std::cmp::Ordering::Less => parts[index] = part.clone(),
                    std::cmp::Ordering::Equal => parts.push(part.clone()),
                    std::cmp::Ordering::Greater => return None,
                }
                continue;
            }
            let field = op.get("field")?.as_str()?;
            let from = usize::try_from(op.get("from")?.as_u64()?).ok()?;
            let suffix = op.get("append")?.as_str()?;
            let Value::String(text) = parts.get_mut(index)?.get_mut(field)? else {
                return None;
            };
            if text.len() != from {
                return None;
            }
            text.push_str(suffix);
        }
    }
    if let Some(set) = delta.get("set") {
        for (key, value) in set.as_object()? {
            message.insert(key.clone(), value.clone());
        }
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn envelope(kind: &str, message: &Value) -> Value {
        json!({
            "type": "session_event",
            "activeSessionId": "s1",
            "event": { "type": kind, "message": message, "assistantMessageEvent": { "type": "text_delta" } },
            "meta": { "sequence": 1 },
        })
    }

    fn assistant(content: &Value) -> Value {
        json!({ "role": "assistant", "model": "m", "content": content, "usage": { "output": 1 } })
    }

    /// Run `frames` through encoder then decoder, returning the bytes on the
    /// wire and the decoded envelopes.
    fn round_trip(frames: &[Value]) -> (Vec<usize>, Vec<Value>) {
        let mut encoder = MessageDeltaEncoder::new();
        let mut decoder = MessageDeltaDecoder::new();
        let mut wire_sizes = Vec::new();
        let mut restored = Vec::new();
        for frame in frames {
            let mut wire = encoder.encode(frame).unwrap_or_else(|| frame.clone());
            wire_sizes.push(serde_json::to_vec(&wire).unwrap().len());
            decoder.decode(&mut wire);
            restored.push(wire);
        }
        (wire_sizes, restored)
    }

    #[test]
    fn a_delta_frame_stays_bounded_however_long_the_prefix_is() {
        let mut frames = Vec::new();
        let mut text = String::new();
        frames.push(envelope("message_start", &assistant(&json!([]))));
        for step in 0..(MESSAGE_DELTA_ANCHOR_INTERVAL - 1) {
            let padding = "x".repeat(400);
            write!(text, "token{step} {padding}").unwrap();
            frames.push(envelope(
                "message_update",
                &assistant(&json!([{ "type": "text", "text": text }])),
            ));
        }
        let (sizes, decoded) = round_trip(&frames);
        assert_eq!(decoded, frames);
        // The first update after the start already rides as a delta; the
        // last one carries ~25 KB of prefix yet stays as small as the first.
        let deltas = &sizes[2..];
        let largest = deltas.iter().copied().max().unwrap();
        assert!(
            largest < 700,
            "delta frame grew with the prefix: {largest} bytes"
        );
        assert!(
            serde_json::to_vec(frames.last().unwrap()).unwrap().len() > 20_000,
            "the full frame carries the whole prefix"
        );
    }

    #[test]
    fn decoded_frames_equal_the_full_snapshots_through_a_non_append_edit() {
        let thinking = |text: &str| json!({ "type": "thinking", "thinking": text });
        let text = |text: &str| json!({ "type": "text", "text": text });
        let call = |args: Value| json!({ "type": "toolCall", "id": "c1", "name": "bash", "arguments": args });
        let scripted = [
            ("message_start", json!([])),
            ("message_update", json!([thinking("Let")])),
            ("message_update", json!([thinking("Let me look")])),
            (
                "message_update",
                json!([thinking("Let me look"), text("The")]),
            ),
            (
                "message_update",
                json!([thinking("Let me look"), text("The answer")]),
            ),
            // A non-append edit of an existing part: the text is rewritten.
            (
                "message_update",
                json!([thinking("Let me look"), text("An answer")]),
            ),
            (
                "message_update",
                json!([thinking("Let me look"), text("An answer"), call(json!({}))]),
            ),
            (
                "message_update",
                json!([
                    thinking("Let me look"),
                    text("An answer"),
                    call(json!({ "command": "ls" }))
                ]),
            ),
            // A part removed: only a full frame can say that.
            ("message_update", json!([text("An answer")])),
            ("message_update", json!([text("An answer, final")])),
            ("message_end", json!([text("An answer, final")])),
            ("message_start", json!([])),
            ("message_update", json!([text("next")])),
        ];
        let frames: Vec<Value> = scripted
            .iter()
            .map(|(kind, content)| envelope(kind, &assistant(content)))
            .collect();
        let (_, decoded) = round_trip(&frames);
        assert_eq!(decoded, frames);
    }

    #[test]
    fn a_delta_without_its_base_is_reported_not_guessed() {
        let mut encoder = MessageDeltaEncoder::new();
        let start = envelope("message_start", &assistant(&json!([])));
        let update = envelope(
            "message_update",
            &assistant(&json!([{ "type": "text", "text": "hi" }])),
        );
        assert_eq!(encoder.encode(&start), None);
        let mut wire = encoder.encode(&update).expect("a delta");
        // A fresh decoder never saw the start frame.
        let mut decoder = MessageDeltaDecoder::new();
        assert_eq!(
            decoder.decode(&mut wire),
            DeltaDecodeOutcome::Desynchronized
        );
        assert_eq!(wire["event"].get("message"), None);
    }
}
