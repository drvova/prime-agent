//! Provider parsing over representative captured response shapes, the
//! `auto` chain's fallback order against a loopback server, and the
//! `web.search` host-request round trip through a real kernel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

fn source(title: &str, url: &str, snippet: Option<&str>) -> SearchSource {
    SearchSource {
        title: title.to_string(),
        url: url.to_string(),
        snippet: snippet.map(str::to_string),
        published_date: None,
        author: None,
    }
}

#[test]
fn serper_maps_organic_knowledge_graph_and_people_also_ask() {
    let data = json!({
        "knowledgeGraph": {"title": "Rust", "description": "A language.", "attributes": {"Designed by": "Graydon Hoare"}},
        "organic": [
            {"title": "Rust", "link": "https://www.rust-lang.org/", "snippet": "A language  empowering\neveryone.", "date": "2 days ago"},
            {"title": "No link"},
        ],
        "peopleAlsoAsk": [{"question": "Is Rust fast?", "snippet": "Yes."}],
    });
    assert_eq!(
        serp_apis::parse_serper(&data, 10),
        SearchResponse {
            answer: Some(
                "Knowledge Graph: Rust\nA language.\nDesigned by: Graydon Hoare".to_string()
            ),
            sources: vec![SearchSource {
                published_date: Some("2 days ago".to_string()),
                ..source(
                    "Rust",
                    "https://www.rust-lang.org/",
                    Some("A language empowering everyone.")
                )
            }],
            citations: Vec::new(),
            related_questions: vec!["Is Rust fast? — Yes.".to_string()],
        }
    );
}

#[test]
fn brave_merges_description_and_extra_snippets() {
    let data = json!({"web": {"results": [
        {"title": "Tokio", "url": "https://tokio.rs/", "description": "An <strong>async</strong> runtime", "extra_snippets": ["An async runtime", "Reliable."], "age": "2024-05-01"},
        {"title": "bad", "url": "ftp://nope"},
    ]}});
    assert_eq!(
        serp_apis::parse_brave(&data, 10).sources,
        vec![SearchSource {
            published_date: Some("2024-05-01".to_string()),
            ..source(
                "Tokio",
                "https://tokio.rs/",
                Some("An async runtime\nReliable.")
            )
        }]
    );
}

#[test]
fn kagi_tags_buckets_and_lifts_the_direct_answer() {
    let data = json!({"meta": {"trace": "t"}, "data": {
        "search": [{"url": "https://a.example/", "title": "A", "snippet": "first"}],
        "news": [{"url": "https://n.example/", "title": "N"}],
        "related_search": [{"props": {"query": "a b"}}],
        "direct_answer": [{"snippet": "42"}],
    }});
    assert_eq!(
        serp_apis::parse_kagi(&data, 10),
        SearchResponse {
            answer: Some("42".to_string()),
            sources: vec![
                source("A", "https://a.example/", Some("first")),
                source("[News] N", "https://n.example/", None),
            ],
            citations: Vec::new(),
            related_questions: vec!["a b".to_string()],
        }
    );
}

#[test]
fn tavily_carries_its_answer() {
    let data = json!({"answer": " Tokio. ", "results": [{"url": "https://tokio.rs", "title": "Tokio", "content": "runtime", "published_date": "2024-01-02"}]});
    assert_eq!(
        ai_search::parse_tavily(&data, 5),
        SearchResponse {
            answer: Some("Tokio.".to_string()),
            sources: vec![SearchSource {
                published_date: Some("2024-01-02".to_string()),
                ..source("Tokio", "https://tokio.rs", Some("runtime"))
            }],
            ..SearchResponse::default()
        }
    );
}

#[test]
fn exa_synthesizes_the_answer_from_summaries() {
    let data = json!({"results": [
        {"title": "A", "url": "https://a.example", "summary": "Sum A", "author": "x", "publishedDate": "2024-02-03"},
        {"title": "B", "url": "https://b.example", "highlights": ["h1", "h2"]},
    ]});
    assert_eq!(
        ai_search::parse_exa(&data, 10),
        SearchResponse {
            answer: Some("**A**: Sum A".to_string()),
            sources: vec![
                SearchSource {
                    published_date: Some("2024-02-03".to_string()),
                    author: Some("x".to_string()),
                    ..source("A", "https://a.example", Some("Sum A"))
                },
                source("B", "https://b.example", Some("h1 h2")),
            ],
            ..SearchResponse::default()
        }
    );
}

#[test]
fn perplexity_maps_reply_results_and_citations() {
    let data = json!({
        "choices": [{"message": {"content": "Answer [1]."}}],
        "search_results": [{"title": "Doc", "url": "https://doc.example/", "date": "2025-01-01", "snippet": "s"}],
        "citations": ["https://doc.example/", "https://other.example/"],
        "related_questions": ["Why?"],
    });
    assert_eq!(
        ai_search::parse_perplexity(&data, 10),
        SearchResponse {
            answer: Some("Answer [1].".to_string()),
            sources: vec![SearchSource {
                published_date: Some("2025-01-01".to_string()),
                ..source("Doc", "https://doc.example/", Some("s"))
            }],
            citations: vec![
                SearchCitation {
                    url: "https://doc.example/".to_string(),
                    title: "Doc".to_string(),
                    cited_text: None
                },
                SearchCitation {
                    url: "https://other.example/".to_string(),
                    title: "https://other.example/".to_string(),
                    cited_text: None
                },
            ],
            related_questions: vec!["Why?".to_string()],
        }
    );
}

#[test]
fn jina_reads_the_envelope_and_rejects_a_failure_code() {
    let ok = json!({"code": 200, "data": [{"title": "J", "url": "https://j.example", "description": "d"}]});
    assert_eq!(
        ai_search::parse_jina(&ok, 5).expect("parse").sources,
        vec![source("J", "https://j.example", Some("d"))]
    );
    let failed = ai_search::parse_jina(&json!({"code": 402, "data": null}), 5).unwrap_err();
    assert_eq!(failed.status, Some(402));
}

#[test]
fn firecrawl_reads_nested_web_results() {
    let data = json!({"success": true, "data": {"web": [{"url": "https://f.example", "title": "F", "description": "d"}]}});
    assert_eq!(
        ai_search::parse_firecrawl(&data, 10)
            .expect("parse")
            .sources,
        vec![source("F", "https://f.example", Some("d"))]
    );
}

#[test]
fn parallel_joins_excerpts() {
    let data = json!({"search_id": "s", "results": [{"url": "https://p.example", "title": "P", "excerpts": ["one", "two"], "publish_date": "2025-03-04"}]});
    assert_eq!(
        ai_search::parse_parallel(&data, 10).sources,
        vec![SearchSource {
            published_date: Some("2025-03-04".to_string()),
            ..source("P", "https://p.example", Some("one\n\ntwo"))
        }]
    );
}

#[test]
fn hackernews_links_items_and_splits_slots_between_kinds() {
    let stories = json!({"hits": [
        {"objectID": "1", "title": "Show HN: x", "url": "https://x.example", "points": 10, "num_comments": 2, "author": "pg", "created_at": "2024-01-01T00:00:00Z"},
        {"objectID": "2", "title": "Second", "points": 1, "num_comments": 0},
    ]});
    let comments = json!({"hits": [
        {"objectID": "3", "author": "dang", "comment_text": "<p>Nice &amp; tidy</p>", "story_title": "Show HN: x"},
    ]});
    assert_eq!(
        open_indexes::merge_hackernews(&stories, &comments, 2).sources,
        vec![
            SearchSource {
                published_date: Some("2024-01-01T00:00:00Z".to_string()),
                author: Some("pg".to_string()),
                ..source(
                    "Show HN: x",
                    "https://news.ycombinator.com/item?id=1",
                    Some("10 points, 2 comments, by pg — https://x.example")
                )
            },
            SearchSource {
                author: Some("dang".to_string()),
                ..source(
                    "Comment by dang on: Show HN: x",
                    "https://news.ycombinator.com/item?id=3",
                    Some("Nice & tidy")
                )
            },
        ]
    );
}

#[test]
fn searxng_maps_results_and_answers() {
    let data = json!({"results": [{"url": "https://s.example", "title": "S", "content": "c"}], "answers": ["42", {"answer": "forty-two"}]});
    assert_eq!(
        open_indexes::parse_searxng(&data, 10),
        SearchResponse {
            answer: Some("42\nforty-two".to_string()),
            sources: vec![source("S", "https://s.example/", Some("c"))],
            ..SearchResponse::default()
        }
    );
}

const DUCKDUCKGO_HTML: &str = r#"<html><body>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=abc">Tokio <b>docs</b></a></h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">An event-driven, <b>non-blocking</b> I/O platform.</a>
  </div>
</div>
<div class="result results_links result--ad"><a class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x">Ad</a></div>
<div class="result results_links"><h2><a class="result__a" href="https://tokio.rs/">Tokio</a></h2></div>
<div class="nav-link"></div>
</body></html>"#;

#[test]
fn duckduckgo_unwraps_redirects_and_skips_ads() {
    assert_eq!(
        scrapers::parse_duckduckgo(DUCKDUCKGO_HTML),
        vec![
            source(
                "Tokio docs",
                "https://docs.rs/tokio",
                Some("An event-driven, non-blocking I/O platform.")
            ),
            source("Tokio", "https://tokio.rs/", None),
        ]
    );
}

#[test]
fn startpage_reads_result_links_and_drops_internal_ones() {
    let html = r#"<div class="w-gl"><div class="result css-o7i03b">
      <a class="result-link" href="https://www.rust-lang.org/" target="_blank"><h2 class="wgl-title">Rust Programming Language</h2><span>rust-lang.org</span></a>
      <p class="description">A language empowering everyone.</p></div>
      <div class="result"><a class="result-link" href="/sp/search?page=2">Next</a></div></div>"#;
    assert_eq!(
        scrapers::parse_startpage(html),
        vec![source(
            "Rust Programming Language",
            "https://www.rust-lang.org/",
            Some("A language empowering everyone.")
        )]
    );
}

#[test]
fn mojeek_reads_the_standard_results_list() {
    let html = r#"<ul class="results-standard"><li class="r1"><a class="ob" href="https://serde.rs/">serde.rs</a><h2><a class="title" href="https://serde.rs/">Overview &amp; Serde</a></h2><p class="s">Serde is a framework.</p></li>
      <li><h2><a class="title" href="https://www.mojeek.com/search?q=x">More</a></h2></li></ul>"#;
    assert_eq!(
        scrapers::parse_mojeek(html),
        vec![source(
            "Overview & Serde",
            "https://serde.rs/",
            Some("Serde is a framework.")
        )]
    );
}

#[test]
fn ecosia_reads_organic_articles() {
    let html = r#"<article data-test-id="organic-result" class="result"><div class="result__header"><a href="https://crates.io/" data-test-id="result-link"><h2 data-test-id="result-title" class="result-title">crates.io: Rust Package Registry</h2></a></div><div data-test-id="result-description"><p data-test-id="web-result-description">The Rust community's crate registry</p></div></article>
      <article data-test-id="ad-google"><a href="https://ad.example"><h2 data-test-id="result-title">Ad</h2></a></article>"#;
    assert_eq!(
        scrapers::parse_ecosia(html),
        vec![source(
            "crates.io: Rust Package Registry",
            "https://crates.io/",
            Some("The Rust community's crate registry")
        )]
    );
}

#[test]
fn the_report_renders_notes_answer_and_numbered_sources() {
    let report = SearchReport {
        provider: ProviderId::Tavily,
        response: SearchResponse {
            answer: Some("Tokio is a runtime.".to_string()),
            sources: vec![SearchSource {
                published_date: Some("2024-01-02".to_string()),
                ..source("Tokio", "https://tokio.rs", Some("runtime"))
            }],
            related_questions: vec!["Why?".to_string()],
            ..SearchResponse::default()
        },
        failures: Vec::new(),
        notes: vec!["answered by Tavily after Serper failed (401)".to_string()],
        elapsed: Duration::ZERO,
    };
    assert_eq!(
        report.text(),
        "Note: answered by Tavily after Serper failed (401)\nTokio is a runtime.\n\n## Sources\n1 source\n[1] Tokio (2024-01-02)\n    https://tokio.rs\n    runtime\n\n## Related\n1 question\n- Why?"
    );
}

/// A loopback server answering `/<provider id>/...` from per-provider
/// scripted (status, body) replies, recording each request's provider in
/// arrival order.
struct ProviderServer {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl ProviderServer {
    async fn start(replies: HashMap<&'static str, (u16, String)>) -> Self {
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
                    // Read the head, then the body its Content-Length names.
                    let (head_end, content_length) = loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buffer[..at]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while buffer.len() < head_end + content_length {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let provider = path
                        .trim_start_matches('/')
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    recorded.lock().unwrap().push(provider.clone());
                    let (status, body) = replies
                        .get(provider.as_str())
                        .cloned()
                        .unwrap_or((404, String::new()));
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
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

fn auto_request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        provider: WebSearchProvider::Auto,
        num_results: None,
        recency: None,
        timeout: Duration::from_secs(10),
    }
}

#[tokio::test]
async fn auto_tries_keyed_providers_first_then_falls_through_the_keyless_engines() {
    let mojeek = r#"<ul class="results-standard"><li><h2><a class="title" href="https://tokio.rs/">Tokio</a></h2><p class="s">runtime</p></li></ul>"#;
    let server = ProviderServer::start(HashMap::from([
        (
            "serper",
            (401, r#"{"message":"Unauthorized."}"#.to_string()),
        ),
        ("duckduckgo", (200, "<html>no results</html>".to_string())),
        (
            "startpage",
            (
                200,
                "<script>component---src-pages-captcha</script>".to_string(),
            ),
        ),
        ("mojeek", (200, mojeek.to_string())),
    ]))
    .await;
    let engine = WebSearchEngine::for_test_server(&server.base);
    let credentials = Credentials::default().with_key(ProviderId::Serper, "k");
    let report = engine
        .search(&auto_request("tokio"), &credentials)
        .await
        .expect("mojeek answers");
    assert_eq!(report.provider, ProviderId::Mojeek);
    assert_eq!(
        report
            .failures
            .iter()
            .map(|failure| (failure.provider, failure.status))
            .collect::<Vec<_>>(),
        vec![
            (ProviderId::Serper, Some(401)),
            (ProviderId::Duckduckgo, Some(204)),
            (ProviderId::Startpage, Some(429)),
        ]
    );
    // Startpage hits twice (homepage token, then the search); nothing past
    // the answering engine runs, and unkeyed providers never run.
    assert_eq!(
        server.hits(),
        vec!["serper", "duckduckgo", "startpage", "startpage", "mojeek"]
    );
    assert!(report.notes[0]
        .starts_with("answered by Mojeek after Serper failed (Serper authorization failed (401)"));
    assert_eq!(
        report.response.sources,
        vec![source("Tokio", "https://tokio.rs/", Some("runtime"))]
    );
}

#[tokio::test]
async fn an_explicit_keyed_provider_without_a_key_explains_the_setup() {
    let engine = WebSearchEngine::for_test_server("http://127.0.0.1:9");
    let request = SearchRequest {
        provider: WebSearchProvider::Engine(ProviderId::Brave),
        ..auto_request("q")
    };
    let error = engine
        .search(&request, &Credentials::default())
        .await
        .unwrap_err();
    assert!(error.failures.is_empty());
    assert!(
        error
            .message
            .contains("choose \"Brave Search (web search)\""),
        "{}",
        error.message
    );
}

/// The `web.search` host request through a REAL kernel: the runtime's
/// `web.search(...)` client sends it, the registered native handler answers
/// from the loopback server, and the cell sees the structured result.
/// Skipped without a kernel Python carrying a current prime-agent-runtime
/// (`PA_CORE_KERNEL_PYTHON`, else `~/.prime/agent/kernel-venv`).
#[cfg(unix)]
#[tokio::test]
async fn the_kernel_client_round_trips_through_the_native_handler() {
    use crate::kernel::manager::{KernelStartOptions, ReplKernelManager};
    use crate::kernel::shared::{
        ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions,
        KernelShutdownOptions,
    };

    let python = std::env::var_os("PA_CORE_KERNEL_PYTHON")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                std::path::PathBuf::from(home).join(".prime/agent/kernel-venv/bin/python")
            })
        })
        .filter(|python| python.exists());
    let Some(python) = python else {
        eprintln!("no kernel python; skipping the live web.search round trip");
        return;
    };
    let ddg = r#"<div class="result"><a class="result__a" href="https://tokio.rs/">Tokio</a><a class="result__snippet">runtime</a></div>"#;
    let server =
        ProviderServer::start(HashMap::from([("duckduckgo", (200, ddg.to_string()))])).await;
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let mut handlers = HostRequestHandlers::new();
    WebSearchHost {
        engine: Arc::new(WebSearchEngine::for_test_server(&server.base)),
        auth: Arc::new(tokio::sync::Mutex::new(crate::auth::AuthStorage::create(
            agent_dir.path(),
        ))),
        default_provider: WebSearchProvider::Engine(ProviderId::Duckduckgo),
        observer: None,
    }
    .register(&mut handlers);
    let bootstrap = crate::kernel::bootstrap::build_rlm_bootstrap_code(&[], false);
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        cwd: Some(std::env::temp_dir()),
        env: HashMap::new(),
        session_id: Some("web-search-test".to_string()),
        host_handlers: handlers,
        python_skills: Vec::new(),
        on_background_work_settled: None,
        snapshot: None,
        bootstrap_code: Some(bootstrap.clone()),
        stderr_log_path: None,
    });
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("kernel start");
    let booted = manager
        .execute(&bootstrap, ExecuteOptions::default())
        .await
        .expect("bootstrap");
    assert_eq!(booted.status, ExecuteStatus::Ok, "{}", booted.stderr);
    let result = manager
        .execute(
            "r = await web.search('tokio', num_results=3)\n(r.provider, r.sources[0].url, str(r).splitlines()[0])",
            ExecuteOptions::default(),
        )
        .await
        .expect("execute");
    assert_eq!(result.status, ExecuteStatus::Ok, "{:?}", result.error);
    assert_eq!(
        result.result.as_deref(),
        Some("('duckduckgo', 'https://tokio.rs/', '[1] Tokio')")
    );
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}
