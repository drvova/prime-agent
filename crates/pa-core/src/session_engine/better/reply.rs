//! The helper's reply contract: the delimited blocks it must answer with, and their parse.
//!
//! The helper answers with a `<depth>` block (its brief answers to the four questions), an
//! `<unresolved>` block (one reference per line it has not located yet), and the `<rewrite>`
//! block (the rewritten prompt). In brief mode a lone `<unchanged/>` says the brief needs no
//! fix. The last block of each kind wins, so a reply that quotes the format before answering
//! still parses to its answer.

/// One parsed helper reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperReply {
    /// The rewritten prompt, trimmed; `None` when the helper answered `<unchanged/>`.
    pub(crate) rewrite: Option<String>,
    /// References the helper says it has not located yet, in its order.
    pub(crate) unresolved: Vec<String>,
}

/// Why a reply did not parse: the helper returned no `<rewrite>` block (or an empty one) and
/// no `<unchanged/>`.
pub(crate) const MISSING_REWRITE: &str = "the helper's reply has no <rewrite> block";

/// The text between the last `<tag>` and the `</tag>` after it.
fn last_block<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.rfind(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// Parse one reply; the error is [`MISSING_REWRITE`].
pub(crate) fn parse_reply(text: &str) -> Result<HelperReply, &'static str> {
    let unresolved = last_block(text, "unresolved")
        .map(|block| {
            block
                .lines()
                .map(|line| {
                    line.trim()
                        .trim_start_matches(['-', '*', '\u{2022}'])
                        .trim()
                })
                .filter(|line| !line.is_empty() && !line.eq_ignore_ascii_case("none"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    match last_block(text, "rewrite").map(str::trim) {
        Some(rewrite) if !rewrite.is_empty() => Ok(HelperReply {
            rewrite: Some(rewrite.to_string()),
            unresolved,
        }),
        _ if text.contains("<unchanged/>") => Ok(HelperReply {
            rewrite: None,
            unresolved,
        }),
        _ => Err(MISSING_REWRITE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_blocks_win_and_unresolved_lines_are_cleaned() {
        let reply = "Format: <rewrite>...</rewrite>\n<depth>(1) more</depth>\n\
                     <unresolved>\n- `src/lexer.rs`\n* parse_expr\n\nnone\n</unresolved>\n\
                     <rewrite>\n  Fix `parse_expr` in `src/parser.rs`.\n</rewrite>";
        assert_eq!(
            parse_reply(reply),
            Ok(HelperReply {
                rewrite: Some("Fix `parse_expr` in `src/parser.rs`.".to_string()),
                unresolved: vec!["`src/lexer.rs`".to_string(), "parse_expr".to_string()],
            })
        );
    }

    #[test]
    fn unchanged_without_a_rewrite_parses_and_nothing_else_does() {
        assert_eq!(
            parse_reply("<unchanged/>"),
            Ok(HelperReply {
                rewrite: None,
                unresolved: Vec::new(),
            })
        );
        assert_eq!(parse_reply("I fixed the parser for you."), Err(MISSING_REWRITE));
        assert_eq!(parse_reply("<rewrite>   </rewrite>"), Err(MISSING_REWRITE));
    }
}
