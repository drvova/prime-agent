//! Scheme syntax highlighting for the kernel cell card: strings, comments,
//! numbers and booleans, `#:keyword` arguments, quoted symbols, and the head
//! of each form (a special form as a keyword, one of the kernel's own
//! functions as a function name). One linear pass over the cell's tokens,
//! total over any text: a cell still streaming in has unbalanced parens and
//! an unterminated string, and both colour as far as they go.

use super::{Scope, SyntaxPalette};
use crate::code_preview::{is_kernel_function, scheme_tokens, SchemeToken};
use crate::{Line, Span};

/// The forms that are syntax, not calls.
const SPECIAL_FORMS: [&str; 30] = [
    "define",
    "define-values",
    "define-syntax",
    "lambda",
    "let",
    "let*",
    "letrec",
    "letrec*",
    "let-values",
    "if",
    "cond",
    "case",
    "when",
    "unless",
    "else",
    "begin",
    "set!",
    "and",
    "or",
    "do",
    "while",
    "quote",
    "quasiquote",
    "unquote",
    "match",
    "struct",
    "require",
    "provide",
    "with-handler",
    "parameterize",
];

/// Highlight Scheme `code` into one row of spans per source line. Tokens of
/// one colour that touch share a span, so a row wraps at its spaces as the
/// plain text would wherever the colour does not change.
pub(crate) fn highlight_scheme(code: &str, palette: &SyntaxPalette) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Line = Vec::new();
    // The last token that was not space or a comment.
    let mut previous = SchemeToken::Space;
    for (token, text) in scheme_tokens(code) {
        let scope = match token {
            SchemeToken::String => Scope::String,
            SchemeToken::Comment => Scope::Comment,
            SchemeToken::Number | SchemeToken::Literal => Scope::Number,
            SchemeToken::Keyword => Scope::Params,
            SchemeToken::Symbol => match previous {
                SchemeToken::Quote => Scope::Type,
                SchemeToken::Open if SPECIAL_FORMS.contains(&text) => Scope::Keyword,
                SchemeToken::Open if is_kernel_function(text) => Scope::Title,
                SchemeToken::Open
                | SchemeToken::Close
                | SchemeToken::String
                | SchemeToken::Comment
                | SchemeToken::Number
                | SchemeToken::Literal
                | SchemeToken::Keyword
                | SchemeToken::Symbol
                | SchemeToken::Space => Scope::Plain,
            },
            SchemeToken::Open | SchemeToken::Close | SchemeToken::Quote | SchemeToken::Space => {
                Scope::Plain
            }
        };
        match token {
            SchemeToken::Space | SchemeToken::Comment => {}
            SchemeToken::Open
            | SchemeToken::Close
            | SchemeToken::String
            | SchemeToken::Number
            | SchemeToken::Literal
            | SchemeToken::Keyword
            | SchemeToken::Quote
            | SchemeToken::Symbol => previous = token,
        }
        let style = scope.style(palette).unwrap_or_default();
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::take(&mut current));
            }
            match current.last_mut() {
                _ if part.is_empty() => {}
                Some(last) if last.style == style => last.content.push_str(part),
                Some(_) | None => current.push(Span::styled(part.to_string(), style)),
            }
        }
    }
    lines.push(current);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};
    use proptest::prelude::*;

    fn palette() -> SyntaxPalette {
        SyntaxPalette::from_theme(&Theme::builtin("prime", ColorMode::TrueColor))
    }

    /// Every class of token gets its colour, and what is not coloured stays
    /// one span: a row of plain calls wraps exactly as it did unhighlighted.
    #[test]
    fn a_cell_is_coloured_by_token_class() {
        let palette = palette();
        let rows = highlight_scheme(
            "(define n (fs-read \"a.rs\" #:start 1)) ; why\n(list 'ok #t n)",
            &palette,
        );
        let plain = ratatui::style::Style::default();
        let spans: Vec<Vec<(&str, ratatui::style::Style)>> = rows
            .iter()
            .map(|row| row.iter().map(|s| (s.content.as_str(), s.style)).collect())
            .collect();
        assert_eq!(
            spans,
            vec![
                vec![
                    ("(", plain),
                    ("define", palette.keyword),
                    (" n (", plain),
                    ("fs-read", palette.function),
                    (" ", plain),
                    ("\"a.rs\"", palette.string),
                    (" ", plain),
                    ("#:start", palette.variable),
                    (" ", plain),
                    ("1", palette.number),
                    (")) ", plain),
                    ("; why", palette.comment),
                ],
                vec![
                    ("(list '", plain),
                    ("ok", palette.type_),
                    (" ", plain),
                    ("#t", palette.number),
                    (" n)", plain),
                ],
            ]
        );
    }

    proptest! {
        /// Any text highlights without a panic into exactly its own lines:
        /// no byte is dropped, added or moved, whatever is unbalanced or
        /// unterminated in it.
        #[test]
        fn highlighting_is_total_and_lossless(code in "[()\\[\\]\"';#|\\\\ \n\ta-z0-9:.+\\-数é\u{301}🙂]{0,80}") {
            let rows = highlight_scheme(&code, &palette());
            let text: Vec<String> = rows
                .iter()
                .map(|row| row.iter().map(|span| span.content.as_str()).collect())
                .collect();
            let lines: Vec<String> = code.split('\n').map(str::to_string).collect();
            prop_assert_eq!(text, lines);
        }
    }
}
