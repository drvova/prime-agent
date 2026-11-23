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

/// The top-level `(bash …)` calls of a Scheme cell, read from its source.
#[derive(Debug, PartialEq, Eq)]
pub struct SchemeBashCalls {
    /// The first call's command when it is a string literal, else empty.
    pub first: String,
    /// How many top-level forms call `bash`.
    pub count: usize,
    /// How many of the cell's lines those forms span.
    pub lines: usize,
}

/// A Scheme cell's top-level `(bash …)` calls, or `None` when it has none.
/// They are read from the source, never from what the cell went on to run,
/// so a card titled from them reads the same before, during and after the
/// cell: a title that waited for the result changed under the reader.
#[must_use]
pub fn scheme_bash_calls(code: &str) -> Option<SchemeBashCalls> {
    let mut calls = SchemeBashCalls {
        first: String::new(),
        count: 0,
        lines: 0,
    };
    let (mut depth, mut in_string, mut escaped, mut in_comment) = (0usize, false, false, false);
    let (mut line, mut form_start) = (0usize, (0usize, 0usize));
    for (index, ch) in code.char_indices() {
        if ch == '\n' {
            line += 1;
            in_comment = false;
        }
        if in_comment {
            continue;
        }
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            ';' => in_comment = true,
            '(' | '[' => {
                if depth == 0 {
                    form_start = (index, line);
                }
                depth += 1;
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                let form = &code[form_start.0..=index];
                let argument = form.strip_prefix("(bash").filter(|rest| {
                    rest.starts_with(|next: char| next.is_whitespace() || next == '"')
                });
                if let (0, Some(argument)) = (depth, argument) {
                    if calls.count == 0 {
                        // The literal's text with Scheme's string escapes read.
                        let mut literal = argument.trim_start().chars();
                        if literal.next() == Some('"') {
                            while let Some(next) = literal.next() {
                                match next {
                                    '"' => break,
                                    '\\' => calls.first.push(match literal.next() {
                                        Some('n') => '\n',
                                        Some('t') => '\t',
                                        Some(other) => other,
                                        None => break,
                                    }),
                                    other => calls.first.push(other),
                                }
                            }
                        }
                    }
                    calls.count += 1;
                    calls.lines += line - form_start.1 + 1;
                }
            }
            _ => {}
        }
    }
    (calls.count > 0).then_some(calls)
}

/// The preview of a Scheme kernel cell: its first line that is not blank.
#[must_use]
pub fn preview_scheme_code(code: &str) -> String {
    code.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}
