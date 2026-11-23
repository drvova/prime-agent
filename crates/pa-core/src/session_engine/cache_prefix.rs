//! Prompt-cache prefix breaks on the loop's provider requests. A provider's
//! prompt cache (automatic prefix caching, or Anthropic breakpoints) reuses
//! a request's leading bytes only while they are byte-identical to what was
//! sent before; a request that changes anything inside the previous
//! request's prefix re-sends everything from that point as uncached input.
//!
//! [`RequestPrefix`] fingerprints a request as its ordered units (the system
//! prompt, each tool definition, each message), and
//! [`RequestPrefix::broken_bytes`] says how much of the previous request the
//! next one did not keep. The session counts those requests
//! (`cache_prefix_break_count` on `agent session ended`); compaction is one
//! of them by design, and nothing else in an append-only session is.

use std::hash::{Hash as _, Hasher as _};
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::stream::{LlmContext, StreamFn};

/// A request as its ordered units, each a fingerprint and a serialized size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RequestPrefix {
    units: Vec<(u64, u64)>,
}

impl RequestPrefix {
    /// Fingerprint `context` (system prompt, tools in order, messages in order).
    pub(crate) fn of(context: &LlmContext) -> Self {
        let unit = |bytes: &[u8]| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut hasher);
            (hasher.finish(), bytes.len() as u64)
        };
        let mut units = vec![unit(
            context.system_prompt.as_deref().unwrap_or("").as_bytes(),
        )];
        // Serializing wire-shaped values cannot fail; a unit that did would
        // fingerprint as empty and only ever over-count a break.
        units.extend(
            context
                .tools
                .iter()
                .map(|tool| unit(&serde_json::to_vec(tool).unwrap_or_default())),
        );
        units.extend(
            context
                .messages
                .iter()
                .map(|message| unit(&serde_json::to_vec(message).unwrap_or_default())),
        );
        Self { units }
    }

    /// The serialized bytes of `self` (the previous request) that `next`
    /// does not repeat as its own prefix: `None` when `next` keeps all of
    /// `self` (it only appends, or repeats it), else the size of the units
    /// from the first changed one to `self`'s end. A shorter `next` that is
    /// a prefix of `self` (a retried or forked request) breaks nothing it
    /// sends, so it is `None` too.
    pub(crate) fn broken_bytes(&self, next: &Self) -> Option<u64> {
        let shared = self
            .units
            .iter()
            .zip(&next.units)
            .take_while(|(previous, next)| previous == next)
            .count();
        (shared < self.units.len().min(next.units.len()))
            .then(|| self.units[shared..].iter().map(|(_, len)| len).sum())
    }
}

/// Observes each request that broke the previous request's prefix, with
/// the previous request's bytes it did not keep (telemetry).
pub(crate) type PrefixBreakObserver = Arc<dyn Fn(u64) + Send + Sync>;

/// Wrap the loop's `stream_fn`: compare each request with the one before
/// it and report a break to `observer`. Pure CPU over the context the loop
/// built (one serialization of it); it never awaits.
pub(crate) fn track_prefix_breaks(stream_fn: StreamFn, observer: PrefixBreakObserver) -> StreamFn {
    let previous: Arc<Mutex<Option<RequestPrefix>>> = Arc::default();
    Arc::new(move |model, context, options| {
        let next = RequestPrefix::of(&context);
        let broken = previous
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(next.clone())
            .and_then(|previous| previous.broken_bytes(&next));
        if let Some(bytes) = broken {
            observer(bytes);
        }
        stream_fn(model, context, options)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(system: &str, texts: &[&str]) -> RequestPrefix {
        RequestPrefix::of(&LlmContext {
            system_prompt: Some(system.to_string()),
            messages: texts
                .iter()
                .map(|text| {
                    serde_json::from_value(
                        serde_json::json!({"role": "user", "content": text, "timestamp": 1}),
                    )
                    .expect("user row")
                })
                .collect(),
            tools: Vec::new(),
        })
    }

    /// Appending, repeating, or sending a prefix of the previous request
    /// breaks nothing; a changed unit breaks the previous request from it
    /// to its end.
    #[test]
    fn only_a_changed_unit_breaks_the_previous_prefix() {
        let previous = request("system", &["a", "b", "c"]);
        // Every one-letter user message serializes to the same size.
        let message = request("", &["a"]).units[1].1;
        assert_eq!(
            [
                previous.broken_bytes(&request("system", &["a", "b", "c", "d"])),
                previous.broken_bytes(&request("system", &["a", "b", "c"])),
                previous.broken_bytes(&request("system", &["a", "b"])),
                previous.broken_bytes(&request("system", &["a", "B", "c", "d"])),
                previous.broken_bytes(&request("system 2", &["a", "b", "c"])),
            ],
            [
                None,
                None,
                None,
                Some(2 * message),
                Some("system".len() as u64 + 3 * message),
            ]
        );
    }
}
