//! The frozen-prefix oracle: a streaming cache fed every prefix of real assistant texts renders
//! (and counts) exactly what a cold full render of the same prefix does.

use super::*;

/// The mock provider's long markdown answer (headings, a numbered list, a fenced block, a table,
/// a bullet list, a quote).
const PLAN_ANSWER: &str = "## Plan for the refactor

The parser is doing three jobs at once. Here is how I would split it:

1. **Tokenizer** — turns bytes into tokens
2. **Parser** — builds the AST
3. **Resolver** — binds names

### Example

```rust
fn tokenize(src: &str) -> Vec<Token> {
    let mut out = Vec::new();
    for (i, ch) in src.char_indices() {
        match ch {
            '(' => out.push(Token::LParen(i)),
            ')' => out.push(Token::RParen(i)),
            _ => {}
        }
    }
    out
}
```

### Trade-offs

| Option | Speed | Complexity |
| --- | --- | --- |
| Single pass | fast | high |
| Three passes | medium | low |
| Incremental | fastest | very high |

- Keep the public API stable
- Move tests next to each stage
- Benchmark before and after

> Note: the resolver can stay where it is for now.

That should give a cleaner boundary with little risk.";

/// The pa-perf corpus's assistant shape: paragraphs of plain sentences.
const CORPUS_ANSWER: &str = "the bench corpus session kernel worker daemon roster scan window. \
cache entry branch leaf header row fold usage token model provider stream turn. \n\
tool result call prompt context compaction summary refine harness memory skill label. \
tree spawn child depth fanout seed profile median p95 latency paint frame needle. \n\n\
the quick bench corpus session kernel worker. daemon roster scan window cache entry branch. \n";

/// Constructs that read across blank lines or can reinterpret earlier text: loose lists with
/// lazy and indented continuations, an unclosed then closed fence, tabs, CRLF, escapes, quotes
/// with lazy lines, tables, headings, rules and hard breaks.
const ADVERSARIAL: &[&str] = &[
    "intro\n\n- one\n\n- two\n\n  still two\n\nafter the list\n\nlast",
    "1. first\n\n2. second\n\n3. third\n\ndone\n\n# heading\n\nbody",
    "text\n\n```\nopen fence\n\nstill code\n\n```\n\nafter fence\n\nmore",
    "a\n\n~~~py\nx = 1\n\ny = 2\n~~~\n\nb",
    "tab\there\n\n\tindented\n\nplain",
    "crlf line\r\n\r\nnext\r\n\r\nthird",
    "esc \u{1b}[31mred\u{1b}[0m\n\nafter\n\nend",
    "> quote\nlazy line\n\n> second quote\n\nafter",
    "| a | b |\n| - | - |\n| 1 | 2 |\n\nafter table\n\n| c |\n| - |\n| 3 |",
    "hard break\\\nnext\n\n---\n\n***\n\nend",
    "para\n\n\n\nmany blanks\n\n   spaced\n\nend",
    "- a\n  - nested\n\n    deep\n\n- b\n\ntail",
    "wide 界界界界界界界界界界界界界界界界界界界界界界界\n\nemoji 😀😀😀😀\n\nend",
    "## h\ntext\n# h1\n\n#nothash\n\n####### seven",
];

/// Real markdown documents of this repository, cut at every line end (they are long: every char
/// prefix of them would cost minutes in a debug build; the mid-line appends a stream also makes
/// are covered by the char-prefix corpora above).
const CRATE_README: &str = include_str!("../../README.md");
const CONTRIBUTOR_RULES: &str = include_str!("../../../../AGENTS.md");

/// How a corpus is cut into the prefixes a stream would show.
enum Prefixes {
    EveryChar,
    EveryLine,
}

/// Every prefix of `text` (per `cut`), as the view passes it (trimmed), rendered through one
/// streaming cache at `width`, against the cold full render and the cold count. Returns whether
/// the cache froze a prefix at some point.
fn assert_incremental_matches_full_by(text: &str, width: usize, cut: &Prefixes) -> bool {
    let style = MarkdownStyle::default();
    let mut cache = MarkdownBlockCache::streaming();
    let mut froze = false;
    let mut boundaries: Vec<usize> = match cut {
        Prefixes::EveryChar => text.char_indices().map(|(index, _)| index).collect(),
        Prefixes::EveryLine => text
            .match_indices('\n')
            .map(|(index, _)| index + 1)
            .collect(),
    };
    boundaries.push(text.len());
    for end in boundaries {
        let shown = text[..end].trim();
        let incremental = render_markdown_tagged(shown, width, &style, "", &mut cache);
        let full = render_markdown(shown, width, &style);
        assert_eq!(incremental, full, "width {width}, prefix {shown:?}");
        assert_eq!(
            markdown_row_count_tagged(shown, width, &style, "", &cache),
            full.len(),
            "count, width {width}, prefix {shown:?}"
        );
        froze |= cache
            .1
            .as_ref()
            .is_some_and(|stream| !stream.slots.is_empty());
    }
    froze
}

fn assert_incremental_matches_full(text: &str, width: usize) -> bool {
    assert_incremental_matches_full_by(text, width, &Prefixes::EveryChar)
}

#[test]
fn every_prefix_of_real_answers_renders_like_a_full_render() {
    for width in [36, 100] {
        assert!(
            assert_incremental_matches_full(PLAN_ANSWER, width),
            "the long answer exercises the frozen prefix"
        );
        assert!(assert_incremental_matches_full(CORPUS_ANSWER, width));
        assert!(
            assert_incremental_matches_full_by(CRATE_README, width, &Prefixes::EveryLine),
            "the crate README exercises the frozen prefix"
        );
        assert!(
            assert_incremental_matches_full_by(CONTRIBUTOR_RULES, width, &Prefixes::EveryLine),
            "the contributor rules exercise the frozen prefix"
        );
        for text in ADVERSARIAL {
            assert_incremental_matches_full(text, width);
        }
    }
}

/// The guards: a boundary right after a list, inside an open fence, before an indented line, or
/// behind an escape byte never freezes.
#[test]
fn unsafe_boundaries_never_freeze() {
    assert_eq!(stable_boundary("a\n\n  indented"), None);
    assert_eq!(stable_boundary("a\n\n"), None);
    assert_eq!(stable_boundary("\u{1b}[1ma\n\nb"), None);
    assert_eq!(stable_boundary("a\r\n\r\nb"), None);
    assert_eq!(stable_boundary("a\n\nb"), Some(3));
    let style = MarkdownStyle::default();
    for text in ["- one\n\n- two", "```\ncode\n\nmore"] {
        let mut cache = MarkdownBlockCache::streaming();
        render_markdown_tagged(text, 40, &style, "", &mut cache);
        assert!(
            cache
                .1
                .as_ref()
                .is_some_and(|stream| stream.slots.is_empty()),
            "{text:?} must not freeze"
        );
    }
}
