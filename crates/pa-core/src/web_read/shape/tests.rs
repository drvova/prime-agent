//! Shaping an HTML page: `spider_transformations`' formats and transforms,
//! and the nesting bound.

use std::time::Duration;

use serde_json::json;
use url::Url;

use super::*;
use crate::web_read::{finish, render, Fetched, ReadRequest};

const PAGE: &str = "<!doctype html><html><head><title>Shaped</title></head><body><nav><a href=\"/\">Home</a></nav><article><h1>The article</h1><p>Its <b>first</b> paragraph.</p><aside class=\"ad\">Buy now</aside></article><footer>Footer text</footer></body></html>";

fn url() -> Url {
    Url::parse("https://example.org/post").unwrap()
}

fn shaped(options: &serde_json::Value) -> anyhow::Result<Shaped> {
    shape(PAGE, &url(), &Shaping::parse(options, "web.read").unwrap())
}

/// Each format is spider's rendering of the page, fenced as its language
/// (`CommonMark` is markdown, not fenced).
#[test]
fn each_format_is_spiders_rendering_of_the_page() {
    let formats: Vec<_> = ["commonmark", "text", "html2text", "html", "xml"]
        .into_iter()
        .map(|format| {
            (
                format,
                shaped(&json!({ "format": format })).unwrap().formatted,
            )
        })
        .collect();
    let text = |language, text: &str| Some((Some(language), text.to_string()));
    assert_eq!(
        formats,
        vec![
            ("commonmark", Some((None, "Shaped# The article\nIts **first** paragraph.\nBuy now".to_string()))),
            ("text", text("text", "Shaped\nHome The article Its first paragraph. Buy now Footer text")),
            ("html2text", text("text", "[Home][1]\n\n# The article\n\nIts first paragraph.\n\nBuy nowFooter text\n\n[1]: /\n")),
            ("html", text("html", PAGE)),
            ("xml", text("xml", "<!DOCTYPE html><html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>Shaped</title></head><body><nav><a href=\"/\">Home</a></nav><article><h1>The article</h1><p>Its <b>first</b> paragraph.</p><aside class=\"ad\">Buy now</aside></article><footer>Footer text</footer></body></html>")),
        ]
    );
}

/// A transform runs on the HTML first: a root and an exclude selector leave
/// the article without its aside (under the page's own title, in this
/// reader's markdown), and readability keeps the article's words only.
#[test]
fn a_transform_runs_before_the_format() {
    let rooted = shaped(&json!({"transform": {"root": "article", "exclude": ".ad"}})).unwrap();
    let readable = shaped(&json!({"transform": {"readability": true}, "format": "text"})).unwrap();
    assert_eq!(
        (
            rooted.rendered.title,
            rooted.rendered.markdown,
            rooted.formatted,
            readable.formatted
        ),
        (
            Some("Shaped".to_string()),
            "# The article\n\nIts **first** paragraph.".to_string(),
            None,
            Some((
                Some("text"),
                "Shaped\nThe article Its first paragraph.".to_string()
            )),
        )
    );
}

/// spider's parsers recurse with the nesting, so a page nested past the
/// bound is refused instead of risking the daemon's stack.
#[test]
fn a_page_nested_past_the_bound_is_refused() {
    let deep = format!(
        "<html><body>{}</body></html>",
        "<div>".repeat(MAX_NESTING + 10)
    );
    let error = shape(
        &deep,
        &url(),
        &Shaping::parse(&json!({"format": "text"}), "web.read").unwrap(),
    )
    .err()
    .map(|error| format!("{error:#}"));
    assert_eq!(
        error,
        Some(format!("the page nests {} elements deep, past the {MAX_NESTING} spider's parsers are given; read it without #:format, #:transform or extraction", MAX_NESTING + 12))
    );
}

/// The options are checked before anything is read: an unknown format or
/// transform key, and a root that is not CSS, are refused by name.
#[test]
fn unknown_formats_and_transforms_are_refused() {
    let refused = |options| {
        Shaping::parse(&options, "web.read")
            .err()
            .map(|error| error.to_string())
    };
    assert_eq!(
        [
            refused(json!({"format": "pdf"})),
            refused(json!({"transform": {"main_content": true}})),
            refused(json!({"transform": {"root": "//div"}})),
        ],
        [
            Some("web.read format must be markdown, commonmark, text, html2text, html or xml; got \"pdf\"".to_string()),
            Some("web.read transform has no \"main_content\"; it takes readability, main-content, filter-images, filter-svg, clean-html, root and exclude".to_string()),
            Some("web.read transform root: \"//div\" is not a CSS selector".to_string()),
        ]
    );
}

/// A shaped HTML page reads as `html-transformed` in a line-addressable code
/// view; a page of another kind keeps its own rendering, with a note.
#[tokio::test]
async fn a_read_shapes_html_and_notes_other_kinds() {
    let shaping = Shaping::parse(&json!({"format": "text"}), "web.read").unwrap();
    let read = |media: &str, body: &str| {
        let fetched = Fetched {
            final_url: url(),
            media_type: Some(media.to_string()),
            body: body.as_bytes().to_vec(),
        };
        let shaping = shaping.clone();
        async move {
            let document = render(fetched, &shaping).await.unwrap();
            let request = ReadRequest {
                url: url(),
                max_chars: 1_000,
                section: None,
                timeout: Duration::from_secs(5),
                shaping,
            };
            let page = finish(&request, document, Duration::ZERO).unwrap();
            (page.method.id(), page.markdown, page.notes)
        }
    };
    assert_eq!(
        (
            read("text/html", PAGE).await,
            read("text/markdown", "# Plain").await
        ),
        (
            (
                "html-transformed",
                "```text\nShaped\nHome The article Its first paragraph. Buy now Footer text\n```"
                    .to_string(),
                Vec::new(),
            ),
            (
                "markdown",
                "# Plain".to_string(),
                vec!["#:format and #:transform shape HTML pages; this text/markdown is shown as usual".to_string()],
            ),
        )
    );
}
