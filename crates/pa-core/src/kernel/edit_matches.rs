//! The `edit.fact` host request: the kernel's `(fs-edit path (hash 'old old-str 'new new-str))`
//! reports how it found the text it replaced (the step of its normalization
//! ladder) or why it found nothing, so the session can count it (adoption
//! telemetry). The kernel sends it without awaiting the reply.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The request each edit's outcome is reported with.
pub(crate) const EDIT_FACT_REQUEST_TYPE: &str = "edit.fact";

/// How one `edit` call went (a count: never a path, a string, or content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditMatch {
    /// Applied: `old-str` matched exactly.
    Exact,
    /// Applied: matched with trailing whitespace ignored per line.
    TrailingWhitespace,
    /// Applied: matched with indentation ignored per line.
    Indentation,
    /// Applied: matched with Unicode punctuation folded to ASCII.
    UnicodePunctuation,
    /// Applied: matched once its literal escapes were unescaped.
    Unescaped,
    /// Refused: `old-str` carries `fs-read` anchor prefixes.
    AnchorPrefix,
    /// Failed: the first step to find `old-str` found it more than once.
    Ambiguous,
    /// Failed: no step found `old-str`.
    NotFound,
    /// Failed: no step found `old-str`, and `new-str` is in the file once
    /// (the edit looks already applied).
    AlreadyApplied,
}

/// Hears each reported edit.
pub(crate) type EditMatchObserver = Arc<dyn Fn(EditMatch) + Send + Sync>;

/// Register `edit.fact` (a session without telemetry answers and counts nothing).
pub(crate) fn register_edit_match_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<EditMatchObserver>,
) {
    handlers.register(
        EDIT_FACT_REQUEST_TYPE,
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let outcome = match payload.data.get("outcome").and_then(Value::as_str) {
                    Some("exact") => EditMatch::Exact,
                    Some("trailing-whitespace") => EditMatch::TrailingWhitespace,
                    Some("indentation") => EditMatch::Indentation,
                    Some("unicode-punctuation") => EditMatch::UnicodePunctuation,
                    Some("unescaped") => EditMatch::Unescaped,
                    Some("anchor-prefix") => EditMatch::AnchorPrefix,
                    Some("ambiguous") => EditMatch::Ambiguous,
                    Some("not-found") => EditMatch::NotFound,
                    Some("already-applied") => EditMatch::AlreadyApplied,
                    other => anyhow::bail!("{EDIT_FACT_REQUEST_TYPE}: unknown outcome {other:?}"),
                };
                if let Some(observer) = observer {
                    observer(outcome);
                }
                Ok(json!({}))
            })
        }),
    );
}
