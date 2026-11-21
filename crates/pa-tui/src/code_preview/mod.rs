//! Code-preview extraction for tool-call cards (`ipython` collapsed line).
//! The pa-tui dependency boundary (pa-types only) keeps the session engine
//! out of the UI, so the display-side preview algorithm lives here; the
//! TypeScript golden corpus test lives here too.

pub(crate) mod bash;
mod cell;
#[cfg(test)]
mod golden_tests;

pub use cell::parse_ipython_bash_cell;
mod python;

pub use bash::{preview_bash_command, CodePreview, CodePreviewLanguage};
pub use python::{preview_ipython_code, python_statement_lines};

/// The preview of a Scheme kernel cell: its first line that is not blank.
#[must_use]
pub fn preview_scheme_code(code: &str) -> String {
    code.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}
