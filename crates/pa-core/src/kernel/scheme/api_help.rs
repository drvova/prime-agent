//! `(api-help [topic])`: the kernel API reference, read from inside a cell.
//!
//! The system prompt carries each function's call form; its full entry
//! (result shape, options, defaults) is printed here on demand, from the doc
//! files the prompt is spliced from ([`crate::prompts::layers::API_DOCS`]).
//! Without an argument it prints the topics; with a topic's name that topic;
//! with a function's name that function's entry.
//!
//! `(api-search query #:limit)`: the passages of the same reference that best match the words of
//! `query`, ranked with BM25 ([`crate::prompts::layers::api_search`]), so a cell finds a function
//! or a detail without knowing its name or topic.

use anyhow::anyhow;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::kernel::shared::{ReferenceLookupFact, ReferenceLookupObserver};

/// Define `(api-help [topic])` and `(api-search query #:limit)` in a session's `engine`;
/// `observer` hears each call.
///
/// # Errors
///
/// The Scheme definition failed to load.
pub(super) fn register(
    engine: &mut Engine,
    observer: Option<ReferenceLookupObserver>,
) -> anyhow::Result<()> {
    let heard = observer.clone();
    engine.register_fn("%api-help-text", move |topic: String| -> String {
        if let Some(observer) = &heard {
            observer(ReferenceLookupFact::Help);
        }
        crate::prompts::layers::api_help(&topic)
    });
    engine.register_fn(
        "%api-search-text",
        move |query: String, limit: usize| -> String {
            let found = crate::prompts::layers::api_search(&query, limit);
            if let Some(observer) = &observer {
                observer(if found.is_some() {
                    ReferenceLookupFact::SearchFound
                } else {
                    ReferenceLookupFact::SearchMissed
                });
            }
            found.unwrap_or_else(|| {
                format!(
                    "api-search: nothing in the reference matches {query:?}.\n{}",
                    crate::prompts::layers::api_help("")
                )
            })
        },
    );
    super::contracts::load(
        engine,
        r##"
(define (api-help . topic)
  (display (%api-help-text (cond [(null? topic) ""]
                                 [(symbol? (car topic)) (symbol->string (car topic))]
                                 [else (car topic)]))))

(define (api-search query #:limit [limit 5])
  (unless (string? query)
    (error "api-search: the query must be a string"))
  (let ([limit (%integer "api-search" "#:limit" limit)])
    (unless (> limit 0)
      (error "api-search: #:limit must be at least 1"))
    (display (%api-search-text query limit))))
"##,
    )
    .map_err(|error| anyhow!("the Scheme API reference functions failed to load: {error}"))?;
    Ok(())
}
