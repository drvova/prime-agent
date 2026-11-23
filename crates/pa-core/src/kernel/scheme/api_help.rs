//! `(api-help [topic])`: the kernel API reference, read from inside a cell.
//!
//! The system prompt carries each function's call form; its full entry
//! (result shape, options, defaults) is printed here on demand, from the doc
//! files the prompt is spliced from ([`crate::prompts::layers::API_DOCS`]).
//! Without an argument it prints the topics; with a topic's name that topic;
//! with a function's name that function's entry.

use anyhow::anyhow;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// Define `(api-help [topic])` in a session's `engine`.
///
/// # Errors
///
/// The Scheme definition failed to load.
pub(super) fn register(engine: &mut Engine) -> anyhow::Result<()> {
    engine.register_fn("%api-help-text", |topic: String| -> String {
        crate::prompts::layers::api_help(&topic)
    });
    super::contracts::load(
        engine,
        r#"
(define (api-help . topic)
  (display (%api-help-text (cond [(null? topic) ""]
                                 [(symbol? (car topic)) (symbol->string (car topic))]
                                 [else (car topic)]))))
"#,
    )
    .map_err(|error| anyhow!("the Scheme API reference failed to load: {error}"))?;
    Ok(())
}
