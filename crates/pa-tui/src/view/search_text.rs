//! Transcript search's text side: the smart-case literal query, the matches in one row's
//! visible text, and the row restyle that paints them. A match starts and ends on grapheme
//! cluster boundaries, so a painted span never splits a cluster and the row keeps its width.

use crate::width::escape_len;
use crate::{Line, Span};
use ratatui::style::Style;
use std::borrow::Cow;

/// A literal query: case-insensitive unless it holds an uppercase letter (smart case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    needle: String,
    case_sensitive: bool,
}

impl Query {
    /// The query a typed value searches for; `None` when it holds no visible character. Control
    /// characters never match a row (the rows carry none outside escape sequences).
    pub(crate) fn new(value: &str) -> Option<Self> {
        let scrubbed: String = value.chars().filter(|c| !c.is_control()).collect();
        if scrubbed.is_empty() {
            return None;
        }
        let case_sensitive = scrubbed.chars().any(char::is_uppercase);
        let needle = if case_sensitive {
            scrubbed
        } else {
            fold(&scrubbed).into_owned()
        };
        Some(Self {
            needle,
            case_sensitive,
        })
    }

    /// The byte ranges of `text` the query matches, in order and not overlapping. `folded` is
    /// `fold(text)` when the caller keeps it; it shares `text`'s byte offsets.
    pub(crate) fn find(&self, text: &str, folded: &str) -> Vec<(usize, usize)> {
        let haystack = if self.case_sensitive { text } else { folded };
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(offset) = haystack[from..].find(self.needle.as_str()) {
            let start = from + offset;
            let end = start + self.needle.len();
            if is_boundary(haystack, start) && is_boundary(haystack, end) {
                found.push((start, end));
                from = end;
            } else {
                // A match inside a cluster is no match; the next try starts one character on.
                from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        found
    }
}

/// `text` case folded with the same byte offsets (each character to its lowercase form when
/// that is one character of the same UTF-8 length); borrowed when nothing folds.
pub(crate) fn fold(text: &str) -> Cow<'_, str> {
    if text.is_ascii() && !text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| {
                let mut lower = c.to_lowercase();
                match (lower.next(), lower.next()) {
                    (Some(single), None) if single.len_utf8() == c.len_utf8() => single,
                    (Some(_) | None, _) => c,
                }
            })
            .collect(),
    )
}

/// Whether `offset` sits on a grapheme cluster boundary of `text`.
fn is_boundary(text: &str, offset: usize) -> bool {
    if offset == 0 || offset >= text.len() {
        return true;
    }
    let bytes = text.as_bytes();
    // Two ASCII characters always split, except a CR LF pair.
    if bytes[offset - 1].is_ascii() && bytes[offset].is_ascii() {
        return !(bytes[offset - 1] == b'\r' && bytes[offset] == b'\n');
    }
    unicode_segmentation::GraphemeCursor::new(offset, text.len(), true)
        .is_boundary(text, 0)
        .unwrap_or(true)
}

/// A row's visible text: its spans' content without escape sequences (hyperlinks, prompt
/// markers). The search index and the painter read rows through this one function, so a match
/// the index counts is the match the painter finds.
pub(crate) fn visible_text(line: &Line) -> String {
    let mut text = String::new();
    for span in line {
        let content = span.content.as_str();
        let mut at = 0;
        while at < content.len() {
            if let Some(skip) = escape_len(&content[at..]) {
                at += skip;
                continue;
            }
            let c = content[at..].chars().next().expect("at is a char boundary");
            text.push(c);
            at += c.len_utf8();
        }
    }
    text
}

/// `line` with every query match patched with `style`, and the match starting at byte
/// `current` of the visible text with `current_style` instead. Text and widths are unchanged:
/// spans split only at the matches' cluster boundaries.
pub(crate) fn paint_matches(
    line: &Line,
    query: &Query,
    current: Option<usize>,
    style: Style,
    current_style: Style,
) -> Line {
    let text = visible_text(line);
    let ranges: Vec<(usize, usize, Style)> = query
        .find(&text, &fold(&text))
        .into_iter()
        .map(|(start, end)| {
            let paint = if current == Some(start) {
                current_style
            } else {
                style
            };
            (start, end, paint)
        })
        .collect();
    if ranges.is_empty() {
        return line.clone();
    }
    let paint_at = |visible: usize, next: &mut usize| -> Option<Style> {
        while ranges.get(*next).is_some_and(|range| range.1 <= visible) {
            *next += 1;
        }
        ranges
            .get(*next)
            .filter(|range| range.0 <= visible)
            .map(|range| range.2)
    };
    let mut out: Line = Vec::with_capacity(line.len() + ranges.len() * 2);
    let mut visible = 0;
    let mut next = 0;
    for span in line {
        let content = span.content.as_str();
        let mut piece_start = 0;
        let mut piece_paint = paint_at(visible, &mut next);
        let mut at = 0;
        while at < content.len() {
            if let Some(skip) = escape_len(&content[at..]) {
                at += skip;
                continue;
            }
            let c = content[at..].chars().next().expect("at is a char boundary");
            let paint = paint_at(visible, &mut next);
            if paint != piece_paint {
                push_piece(&mut out, span, &content[piece_start..at], piece_paint);
                piece_start = at;
                piece_paint = paint;
            }
            visible += c.len_utf8();
            at += c.len_utf8();
        }
        push_piece(&mut out, span, &content[piece_start..], piece_paint);
    }
    out
}

fn push_piece(out: &mut Line, span: &Span, content: &str, paint: Option<Style>) {
    if content.is_empty() {
        return;
    }
    out.push(Span::styled(
        content.to_string(),
        paint.map_or(span.style, |paint| span.style.patch(paint)),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn spans(line: &Line) -> Vec<(String, bool)> {
        line.iter()
            .map(|span| {
                (
                    span.content.clone(),
                    span.style.add_modifier.contains(Modifier::REVERSED),
                )
            })
            .collect()
    }

    /// A lowercase query ignores case and an uppercase letter makes it exact.
    #[test]
    fn smart_case_folds_only_an_all_lowercase_query() {
        let text = "Wait wait WAIT";
        let found = |value: &str| Query::new(value).unwrap().find(text, &fold(text));
        assert_eq!(
            [found("wait"), found("Wait"), found("WAIT")],
            [vec![(0, 4), (5, 9), (10, 14)], vec![(0, 4)], vec![(10, 14)]]
        );
    }

    /// A match never starts or ends inside a cluster: the `e` of a decomposed `é` and half of a
    /// joined emoji are no match, and the painter splits only around whole matches.
    #[test]
    fn a_match_never_splits_a_grapheme_cluster() {
        let text = "e\u{301}e \u{1f469}\u{200d}\u{1f4bb}\u{1f469}";
        let query = Query::new("e").unwrap();
        assert_eq!(query.find(text, &fold(text)), vec![(3, 4)]);
        let woman = Query::new("\u{1f469}").unwrap();
        let line: Line = vec![Span::raw(text)];
        let painted = paint_matches(
            &line,
            &woman,
            None,
            Style::default().add_modifier(Modifier::REVERSED),
            Style::default(),
        );
        assert_eq!(
            spans(&painted),
            vec![
                ("e\u{301}e \u{1f469}\u{200d}\u{1f4bb}".to_string(), false),
                ("\u{1f469}".to_string(), true),
            ]
        );
    }

    /// Escape sequences are invisible to the match and stay in the row.
    #[test]
    fn a_hyperlink_escape_is_skipped_and_kept() {
        let open = "\u{1b}]8;;https://x.test\u{7}";
        let line: Line = vec![Span::raw(format!("{open}find me")), Span::raw(" find")];
        let painted = paint_matches(
            &line,
            &Query::new("find").unwrap(),
            Some(8),
            Style::default(),
            Style::default().add_modifier(Modifier::REVERSED),
        );
        assert_eq!(
            spans(&painted),
            vec![
                (format!("{open}find"), false),
                (" me".to_string(), false),
                (" ".to_string(), false),
                ("find".to_string(), true),
            ]
        );
    }
}
