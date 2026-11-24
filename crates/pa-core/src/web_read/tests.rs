//! `web.read` over recorded fixtures (`tests/fixtures/web_read/`, captured
//! from the live sources) served by a loopback server: no live network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::markdown::{self, AnchorStyle};
use super::*;

const BOOK_PAGE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/rust-book-data-types.html"
));
const ARXIV_PDF: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/arxiv-2101.00001.pdf"
));
const ARXIV_ATOM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/arxiv-2101.00001.atom"
));
const GLOBSET_FILE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/globset-pathutil.rs"
));
const SO_QUESTION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/so-question.json"
));
const SO_ANSWERS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/web_read/so-answers.json"
));

/// One scripted answer: status, content type, body, and a redirect target.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    location: Option<&'static str>,
}

fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status: 200,
        content_type,
        body: body.into(),
        location: None,
    }
}

/// A loopback server answering `/<host><path>` (the query ignored) from
/// scripted replies, recording each request's path and query.
struct SiteServer {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl SiteServer {
    async fn start(replies: HashMap<&'static str, Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(replies);
        let recorded = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let replies = Arc::clone(&replies);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer).to_string();
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    recorded.lock().unwrap().push(target.clone());
                    let path = target.split('?').next().unwrap_or_default();
                    let reply = replies.get(path).cloned().unwrap_or(Reply {
                        status: 404,
                        content_type: "text/plain",
                        body: b"not found".to_vec(),
                        location: None,
                    });
                    let location = reply
                        .location
                        .map(|location| format!("Location: {location}\r\n"))
                        .unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n",
                        reply.status,
                        reply.content_type,
                        reply.body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&reply.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { base, hits }
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn request(url: &str, section: Option<&str>) -> ReadRequest {
    ReadRequest {
        url: Url::parse(url).unwrap(),
        max_chars: DEFAULT_MAX_CHARS,
        section: section.map(str::to_string),
        timeout: Duration::from_secs(20),
        shaping: Shaping::default(),
    }
}

/// A redirected HTML page renders readably (chrome gone; headings with their
/// anchors, code fences, tables and links kept), and the URL's `#fragment`
/// selects that heading's span.
#[tokio::test]
async fn an_html_page_reads_as_markdown_with_its_anchors_and_its_fragment() {
    let server = SiteServer::start(HashMap::from([
        (
            "/doc.rust-lang.org/book/data-types",
            Reply {
                status: 301,
                content_type: "text/plain",
                body: Vec::new(),
                location: Some("/book/ch03-02-data-types.html"),
            },
        ),
        (
            "/doc.rust-lang.org/book/ch03-02-data-types.html",
            ok("text/html; charset=utf-8", BOOK_PAGE),
        ),
    ]))
    .await;
    let engine = WebReadEngine::for_test_server(&server.base);

    let whole = engine
        .read(&request("https://doc.rust-lang.org/book/data-types", None))
        .await
        .unwrap();
    assert_eq!(
        (
            whole.final_url.as_str(),
            whole.title.as_deref(),
            whole.method
        ),
        (
            "https://doc.rust-lang.org/book/ch03-02-data-types.html",
            Some("Data Types - The Rust Programming Language"),
            ReadMethod::Html
        )
    );
    let anchors: Vec<(u8, &str)> = whole
        .outline
        .iter()
        .map(|heading| (heading.level, heading.anchor.as_str()))
        .take(4)
        .collect();
    assert_eq!(
        anchors,
        [
            (2, "data-types"),
            (3, "scalar-types"),
            (4, "integer-types"),
            (5, "integer-overflow")
        ]
    );
    assert_eq!(
        whole.outline[2].url.as_deref(),
        Some("https://doc.rust-lang.org/book/ch03-02-data-types.html#integer-types")
    );
    for kept in [
        "## Data Types {#data-types}",
        "| Length | Signed | Unsigned |\n| --- | --- | --- |\n| 8-bit | `i8` | `u8` |",
        "```rust\n#![allow(unused)]\nfn main() {\nlet guess: u32 = \"42\".parse().expect(\"Not a number!\");\n}\n```",
    ] {
        assert!(whole.markdown.contains(kept), "missing {kept:?} in:\n{}", whole.markdown);
    }
    // The sidebar, the menu bar, the help dialog and the scripts are chrome.
    for dropped in [
        "mdbook-sidebar",
        "Keyboard shortcuts",
        "Previous chapter",
        "window.playground",
    ] {
        assert!(!whole.markdown.contains(dropped), "kept chrome {dropped:?}");
    }
    assert!(whole
        .links
        .iter()
        .any(|link| link.url.starts_with("https://doc.rust-lang.org/book/")));

    let section = engine
        .read(&request(
            "https://doc.rust-lang.org/book/data-types#integer-types",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(section.section.as_deref(), Some("integer-types"));
    assert!(section
        .markdown
        .starts_with("#### Integer Types {#integer-types}"));
    assert!(section
        .markdown
        .contains("##### Integer Overflow {#integer-overflow}"));
    assert!(!section.markdown.contains("Floating-Point Types"));
    assert_eq!(
        server.hits(),
        [
            "/doc.rust-lang.org/book/data-types",
            "/doc.rust-lang.org/book/ch03-02-data-types.html",
            "/doc.rust-lang.org/book/data-types",
            "/doc.rust-lang.org/book/ch03-02-data-types.html",
        ]
    );
}

/// An arXiv abstract without an HTML rendering reads the PDF page by page
/// under the paper's metadata; `section="page=2"` returns that page alone.
#[tokio::test]
async fn an_arxiv_paper_without_html_reads_its_pdf_by_page_with_its_metadata() {
    let server = SiteServer::start(HashMap::from([
        (
            "/arxiv.org/pdf/2101.00001",
            ok("application/pdf", ARXIV_PDF),
        ),
        (
            "/export.arxiv.org/api/query",
            ok("application/atom+xml", ARXIV_ATOM),
        ),
    ]))
    .await;
    let engine = WebReadEngine::for_test_server(&server.base);

    let paper = engine
        .read(&request("https://arxiv.org/abs/2101.00001", None))
        .await
        .unwrap();
    assert_eq!(
        (
            paper.final_url.as_str(),
            paper.title.as_deref(),
            paper.method,
            paper.notes.as_slice()
        ),
        (
            "https://arxiv.org/pdf/2101.00001",
            Some("Etat de l'art sur l'application des bandits multi-bras"),
            ReadMethod::ArxivPdf,
            [
                "arXiv has no HTML rendering of 2101.00001; this is the PDF".to_string(),
                "truncated to 20000 of 57339 chars; pass section= one of the outline's anchors, or a larger max_chars".to_string()
            ]
            .as_slice()
        )
    );
    let anchors: Vec<&str> = paper
        .outline
        .iter()
        .map(|heading| heading.anchor.as_str())
        .collect();
    let mut expected = vec![
        "etat-de-lart-sur-lapplication-des-bandits-multi-bras".to_string(),
        "abstract".to_string(),
    ];
    expected.extend((1..=11).map(|page| format!("page={page}")));
    assert_eq!(
        anchors,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(paper
        .markdown
        .starts_with("# Etat de l'art sur l'application des bandits multi-bras\n\n**Authors:** Djallel Bouneffouf\n"));
    assert_eq!(
        paper.outline[3].url.as_deref(),
        Some("https://arxiv.org/pdf/2101.00001#page=2")
    );

    let page = engine
        .read(&request("https://arxiv.org/abs/2101.00001", Some("page=2")))
        .await
        .unwrap();
    assert!(page.markdown.starts_with("## Page 2 {#page=2}\n\n"));
    assert!(!page.markdown.contains("{#page=3}"));
    assert_eq!(
        server.hits()[..3],
        [
            "/arxiv.org/html/2101.00001",
            "/arxiv.org/pdf/2101.00001",
            "/export.arxiv.org/api/query?id_list=2101.00001",
        ]
    );
}

/// A GitHub blob reads the raw file as a fenced code view, and its `#L10-L20`
/// line fragment selects those lines.
#[tokio::test]
async fn a_github_blob_reads_the_raw_file_and_its_line_fragment() {
    let server = SiteServer::start(HashMap::from([(
        "/raw.githubusercontent.com/BurntSushi/ripgrep/master/crates/globset/src/pathutil.rs",
        ok("text/plain; charset=utf-8", GLOBSET_FILE),
    )]))
    .await;
    let engine = WebReadEngine::for_test_server(&server.base);
    let page = engine
        .read(&request(
            "https://github.com/BurntSushi/ripgrep/blob/master/crates/globset/src/pathutil.rs#L10-L20",
            None,
        ))
        .await
        .unwrap();
    let lines: Vec<&str> = GLOBSET_FILE.lines().collect();
    assert_eq!(
        (
            page.method,
            page.section.as_deref(),
            page.final_url.as_str(),
            page.markdown.as_str()
        ),
        (
            ReadMethod::GithubFile,
            Some("L10-L20"),
            "https://github.com/BurntSushi/ripgrep/blob/master/crates/globset/src/pathutil.rs",
            format!(
                "Lines 10-20 of {}\n\n```rust\n{}\n```",
                lines.len(),
                lines[9..20].join("\n")
            )
            .as_str()
        )
    );
}

/// A Stack Overflow question reads the API's question and voted answers,
/// each answer under its page anchor.
#[tokio::test]
async fn a_stack_overflow_question_reads_the_question_and_its_voted_answers() {
    let server = SiteServer::start(HashMap::from([
        (
            "/api.stackexchange.com/2.3/questions/11227809",
            ok("application/json", SO_QUESTION),
        ),
        (
            "/api.stackexchange.com/2.3/questions/11227809/answers",
            ok("application/json", SO_ANSWERS),
        ),
    ]))
    .await;
    let engine = WebReadEngine::for_test_server(&server.base);
    let page = engine
        .read(&request(
            "https://stackoverflow.com/questions/11227809/why-is-processing-a-sorted-array-faster",
            None,
        ))
        .await
        .unwrap();
    let link = "https://stackoverflow.com/questions/11227809/why-is-conditional-processing-of-a-sorted-array-faster-than-of-an-unsorted-array";
    // The posts' own headings sit below their post's section.
    let outline: Vec<(String, Option<String>)> = page
        .outline
        .iter()
        .filter(|heading| heading.level == 2)
        .map(|heading| (heading.anchor.clone(), heading.url.clone()))
        .collect();
    assert_eq!(
        outline,
        ["question", "answer-11227902", "answer-11227877"]
            .iter()
            .map(|anchor| (anchor.to_string(), Some(format!("{link}#{anchor}"))))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (page.method, page.final_url.as_str()),
        (ReadMethod::StackExchange, link)
    );
    assert!(page.markdown.starts_with(
        "# Why is conditional processing of a sorted array faster than of an unsorted array?\n\n**Score "
    ));
    assert!(page.markdown.contains(
        "## Answer by Mysticial · score 35296 · accepted · 2012-06-27 {#answer-11227902}"
    ));
    assert!(
        page.markdown.contains("```"),
        "the answers' code stays fenced"
    );
    assert_eq!(
        server.hits(),
        [
            "/api.stackexchange.com/2.3/questions/11227809?site=stackoverflow&filter=withbody",
            "/api.stackexchange.com/2.3/questions/11227809/answers?site=stackoverflow&filter=withbody&sort=votes&order=desc&pagesize=8",
        ]
    );
}

/// A GitHub issue reads the REST API's thread (an API-shaped reply: the
/// sandbox that recorded the other fixtures cannot reach api.github.com),
/// each comment under its page anchor and the bodies' headings nested below.
#[tokio::test]
async fn a_github_issue_reads_the_api_thread_with_comment_anchors() {
    let issue = json!({
        "title": "Support globs", "state": "closed", "comments": 1,
        "html_url": "https://github.com/acme/tool/issues/7",
        "user": {"login": "ada"}, "created_at": "2024-03-01T10:00:00Z",
        "labels": [{"name": "enhancement"}],
        "body": "## Steps\n\nRun `tool *.rs`.",
    });
    let comments = json!([{
        "id": 99, "user": {"login": "bob"}, "created_at": "2024-03-02T09:00:00Z",
        "body": "Fixed in #8.",
    }]);
    let server = SiteServer::start(HashMap::from([
        (
            "/api.github.com/repos/acme/tool/issues/7",
            ok("application/json", issue.to_string()),
        ),
        (
            "/api.github.com/repos/acme/tool/issues/7/comments",
            ok("application/json", comments.to_string()),
        ),
    ]))
    .await;
    let engine = WebReadEngine::for_test_server(&server.base);
    let page = engine
        .read(&request("https://github.com/acme/tool/issues/7", None))
        .await
        .unwrap();
    assert_eq!(
        (page.method, page.final_url.as_str(), page.markdown.as_str()),
        (
            ReadMethod::GithubIssue,
            "https://github.com/acme/tool/issues/7",
            "# Support globs (#7)\n\n**closed** issue · opened by ada on 2024-03-01 · 1 comments · labels: enhancement\n\n#### Steps\n\nRun `tool *.rs`.\n\n## Comment by bob on 2024-03-02 {#issuecomment-99}\n\nFixed in #8."
        )
    );
    assert_eq!(
        page.outline
            .last()
            .and_then(|heading| heading.url.as_deref()),
        Some("https://github.com/acme/tool/issues/7#issuecomment-99")
    );
}

/// An unknown section is an error naming the anchors, relative markdown links
/// resolve, and a long span is cut at a line with its open code fence closed.
#[test]
fn sections_and_truncation_keep_the_markdown_well_formed() {
    let text = "# Guide\n\n## Install\n\nrun it\n\n## Use\n\n```sh\nline one\nline two\n```\n\n## Install\n\nagain";
    let url = Url::parse("https://example.com/guide").unwrap();
    let outline = markdown::outline(text, AnchorStyle::GithubSlugs, &url);
    let anchors: Vec<&str> = outline
        .iter()
        .map(|heading| heading.anchor.as_str())
        .collect();
    assert_eq!(anchors, ["guide", "install", "use", "install-1"]);
    assert_eq!(
        markdown::select_section(text, &outline, "#use").map(|(span, _)| span),
        Some("## Use\n\n```sh\nline one\nline two\n```".to_string())
    );
    let base = Url::parse("https://github.com/acme/tool/blob/HEAD/").unwrap();
    assert_eq!(
        markdown::resolve_links("See [the guide](GUIDE.md#setup) and [top](#use).", &base),
        (
            "See [the guide](https://github.com/acme/tool/blob/HEAD/GUIDE.md#setup) and [top](#use).".to_string(),
            vec![PageLink {
                text: "the guide".to_string(),
                url: "https://github.com/acme/tool/blob/HEAD/GUIDE.md#setup".to_string(),
            }]
        )
    );
    assert_eq!(
        markdown::truncate("## Use\n\n```sh\nline one\nline two\n```", 22),
        ("## Use\n\n```sh\nline one\n```".to_string(), true)
    );
    let document = Document {
        final_url: url,
        title: None,
        content_type: "text/markdown".to_string(),
        method: ReadMethod::Markdown,
        body: Body::Markdown {
            text: text.to_string(),
            anchors: AnchorStyle::GithubSlugs,
        },
        links: Vec::new(),
        notes: Vec::new(),
    };
    let error = finish(
        &request("https://example.com/guide", Some("missing")),
        document,
        Duration::ZERO,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no section \"missing\" in https://example.com/guide; its anchors: guide, install, use, install-1"
    );
}

/// An ampersand followed by multibyte text within the entity window used to
/// slice inside a character and panic.
#[test]
fn an_ampersand_before_multibyte_text_decodes_without_panicking() {
    assert_eq!(
        crate::web_search::http::decode_entities("R&日本語のテキスト &amp;"),
        "R&日本語のテキスト &"
    );
}

/// The host request parses its payload into a request.
#[test]
fn the_payload_parses_defaults_and_rejects_bad_options() {
    assert_eq!(
        parse_request(&json!({"url": "docs.rs/tokio", "maxChars": 900_000, "section": " setup "}))
            .unwrap(),
        ReadRequest {
            url: Url::parse("https://docs.rs/tokio").unwrap(),
            max_chars: MAX_MAX_CHARS,
            section: Some("setup".to_string()),
            timeout: DEFAULT_TIMEOUT,
            shaping: Shaping::default(),
        }
    );
    assert!(parse_request(&json!({"url": "ftp://example.com/x"})).is_err());
    assert!(parse_request(&json!({"url": "https://a.b", "maxChars": 0})).is_err());
}
