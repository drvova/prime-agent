//! Extraction with spider's CSS and `XPath` selectors.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::web_crawl::{CookieJar, WebCrawlEngine, WebCrawlHost};

/// A page with the usual furniture, and an inline SVG with a namespaced
/// attribute (which XML readers reject unless the page is made XML first).
const PAGE: &str = r##"<!doctype html><html><head><title>Extracted</title><meta name="description" content="A page to extract from"></head><body><h1>The heading</h1><svg xmlns="http://www.w3.org/2000/svg"><use xlink:href="#icon"></use></svg><ul><li><a href="/one">One</a></li><li><a href="/two">Two</a></li></ul><img src="/cat.png" alt="A cat"><p>Unclosed<br>paragraph &amp; more</ul></body></html>"##;

fn selectors(map: &serde_json::Value) -> Selectors {
    Selectors::parse(&json!({ "selectors": map }), "selectors", "web.extract")
        .unwrap()
        .unwrap()
}

/// CSS gives each match's text (a `meta`'s content, an `img`'s source and
/// alt); `XPath` reads the page made well-formed XML, so it answers on a page
/// that is not XML (an inline SVG, unclosed tags); a name nothing matched
/// is an empty list.
#[test]
fn css_and_xpath_find_their_values_on_an_html_page() {
    let found = selectors(&json!({
        "heading": "h1",
        "links": "a",
        "description": "meta[name=description]",
        "image": "img",
        "first-href": "//a/@href",
        "items": "count(//li)",
        "both": ["h1", "string(//title)"],
        "missing": ".nothing-has-this",
    }))
    .extract(PAGE)
    .unwrap();
    assert_eq!(
        found,
        json!({
            "heading": ["The heading"],
            "links": ["One", "Two"],
            "description": ["A page to extract from"],
            "image": ["[/cat.png](\"A cat\")"],
            "first-href": ["/one"],
            "items": ["2"],
            "both": ["The heading", "Extracted"],
            "missing": [],
        })
    );
}

/// A selector that is neither CSS nor `XPath` is refused before anything is
/// read (spider would skip it with a log line), naming it.
#[test]
fn a_selector_that_is_neither_css_nor_xpath_is_refused() {
    let error = Selectors::parse(
        &json!({"selectors": {"title": ["h1", "h1[["]}}),
        "selectors",
        "web.extract",
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "web.extract selectors \"title\": \"h1[[\" is neither a CSS selector nor an XPath expression"
    );
}

/// `(web-extract html selectors)` from a cell: the HTML is read as given and
/// the result is a hash of each name to its values.
#[tokio::test(flavor = "multi_thread")]
async fn web_extract_reads_given_html_from_a_cell() {
    use crate::kernel::shared::HostRequestHandlers;
    use crate::tools::kernel_cell::{ExecuteStatus, KernelExecuteOptions, KernelProvisioner};

    let mut handlers = HostRequestHandlers::new();
    WebCrawlHost {
        engine: Arc::new(WebCrawlEngine::direct()),
        store: None,
        observer: None,
        capture_observer: None,
        extract_observer: None,
        settings: None,
        cookies: CookieJar::default(),
        credentials: None,
        automate_observer: None,
    }
    .register(&mut handlers);
    let kernel = crate::kernel::scheme::SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let cell = format!(
        "(let ([found (web-extract {PAGE:?} (hash 'heading \"h1\" 'hrefs (list \"//a/@href\" \"//li[2]/a/@href\")))]) (list (hash-ref found 'heading) (hash-ref found 'hrefs)))"
    );
    let result = kernel
        .execute(
            &cell,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (result.status, result.result),
        (
            ExecuteStatus::Ok,
            Some("((\"The heading\") (\"/one\" \"/two\"))".to_string())
        )
    );
}
