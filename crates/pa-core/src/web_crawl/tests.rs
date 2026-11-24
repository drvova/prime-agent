//! `web.crawl` over a small loopback site: no live network.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// The site's pages: a home page linking two levels down, a robots.txt that
/// disallows `/private`, an off-site link, and a docs section.
fn pages() -> HashMap<&'static str, (&'static str, String)> {
    let page = |title: &str, links: &[&str]| {
        let links = links.iter().fold(String::new(), |mut list, link| {
            write!(list, "<li><a href=\"{link}\">{link}</a></li>").unwrap();
            list
        });
        format!(
            "<!doctype html><html><head><title>{title}</title></head><body><main><h1>{title}</h1><p>The {title} page of the fixture site, with enough prose to be kept as the readable part.</p><ul>{links}</ul></main></body></html>"
        )
    };
    HashMap::from([
        (
            "/robots.txt",
            (
                "text/plain",
                "User-agent: *\nDisallow: /private\n".to_string(),
            ),
        ),
        (
            "/",
            (
                "text/html",
                page(
                    "Home",
                    &[
                        "/a",
                        "/b",
                        "/private/secret",
                        "/docs/one",
                        "http://elsewhere.invalid/x",
                    ],
                ),
            ),
        ),
        ("/a", ("text/html", page("A", &["/a/deep", "/b"]))),
        ("/a/deep", ("text/html", page("Deep", &["/a/deep/deeper"]))),
        ("/a/deep/deeper", ("text/html", page("Deeper", &[]))),
        ("/b", ("text/html", page("B", &["/"]))),
        ("/private/secret", ("text/html", page("Secret", &[]))),
        ("/docs/one", ("text/html", page("Docs one", &["/docs/two"]))),
        ("/docs/two", ("text/html", page("Docs two", &[]))),
    ])
}

/// A loopback server for `site()`, recording every path it was asked for.
/// `hang` makes every page but robots.txt wait forever instead.
struct Site {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    asked: Arc<tokio::sync::Notify>,
}

impl Site {
    async fn start(hang: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(tokio::sync::Notify::new());
        let pages = Arc::new(pages());
        let (recorded, notify) = (Arc::clone(&hits), Arc::clone(&asked));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (pages, recorded, notify) = (
                    Arc::clone(&pages),
                    Arc::clone(&recorded),
                    Arc::clone(&notify),
                );
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
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    recorded.lock().unwrap().push(path.clone());
                    notify.notify_one();
                    if hang && path != "/robots.txt" {
                        std::future::pending::<()>().await;
                    }
                    let (status, content_type, body) = match pages.get(path.as_str()) {
                        Some((content_type, body)) => (200, *content_type, body.clone()),
                        None => (404, "text/plain", "not found".to_string()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Site { base, hits, asked }
    }

    fn hits(&self) -> BTreeSet<String> {
        self.hits.lock().unwrap().iter().cloned().collect()
    }
}

fn request(site: &Site, options: &serde_json::Value) -> CrawlRequest {
    let mut payload = json!({"url": format!("{}/", site.base)});
    if let (Some(payload), Some(options)) = (payload.as_object_mut(), options.as_object()) {
        payload.extend(options.clone());
    }
    parse_request(&payload).expect("request")
}

/// The paths of the pages a crawl kept, sorted.
fn paths(crawl: &Crawl) -> Vec<String> {
    let mut paths: Vec<String> = crawl
        .pages
        .iter()
        .map(|page| {
            Url::parse(page["finalUrl"].as_str().unwrap())
                .unwrap()
                .path()
                .to_string()
        })
        .collect();
    paths.sort();
    paths
}

/// A crawl keeps to the start host, never asks for what robots.txt
/// disallows, and stops `maxDepth` links from the start page.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_stays_on_site_obeys_robots_and_stops_at_its_depth() {
    let site = Site::start(false).await;
    let crawl = WebCrawlEngine::direct()
        .crawl(&request(&site, &json!({"maxDepth": 2})), None)
        .await
        .unwrap();
    let hits = site.hits();
    assert_eq!(
        (
            paths(&crawl),
            hits.contains("/private/secret"),
            hits.contains("/a/deep/deeper"),
            crawl.truncated,
        ),
        (
            ["/", "/a", "/a/deep", "/b", "/docs/one", "/docs/two"]
                .map(str::to_string)
                .to_vec(),
            false,
            false,
            false,
        )
    );
}

/// `maxPages` bounds the pages returned, and the result says it stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_stops_at_its_page_limit() {
    let site = Site::start(false).await;
    let crawl = WebCrawlEngine::direct()
        .crawl(&request(&site, &json!({"maxPages": 2})), None)
        .await
        .unwrap();
    assert_eq!(
        (crawl.pages.len(), crawl.truncated, crawl.notes),
        (
            2,
            true,
            vec!["stopped at #:max-pages 2; pass a larger one (at most 200) for more".to_string()]
        )
    );
}

/// Include and exclude patterns pick what is followed past the start page.
#[tokio::test(flavor = "multi_thread")]
async fn include_and_exclude_patterns_pick_the_followed_links() {
    let site = Site::start(false).await;
    let crawl = WebCrawlEngine::direct()
        .crawl(
            &request(
                &site,
                &json!({"maxDepth": 5, "include": ["/$", "/docs/", "/a"], "exclude": "/a/deep"}),
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        paths(&crawl),
        ["/", "/a", "/docs/one", "/docs/two"]
            .map(str::to_string)
            .to_vec()
    );
}

/// A crawled page is exactly what `web.read` makes of the same response.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawled_page_has_web_reads_shape() {
    let site = Site::start(false).await;
    let crawl = WebCrawlEngine::direct()
        .crawl(
            &request(&site, &json!({"maxDepth": 1, "maxPages": 1})),
            None,
        )
        .await
        .unwrap();
    let url = Url::parse(&format!("{}/", site.base)).unwrap();
    let read = ReadRequest {
        url: url.clone(),
        max_chars: DEFAULT_MAX_CHARS,
        section: None,
        timeout: DEFAULT_TIMEOUT,
    };
    let document = crate::web_read::render(Fetched {
        final_url: url,
        media_type: Some("text/html".to_string()),
        body: pages()["/"].1.clone().into_bytes(),
    })
    .await
    .unwrap();
    let read = crate::web_read::finish(&read, document, Duration::ZERO).unwrap();
    assert_eq!(crawl.pages, vec![read.to_json()]);
}

/// Past the result's output budget a page is paged out to the spill store
/// under a handle that reads back the page's whole text.
#[tokio::test(flavor = "multi_thread")]
async fn pages_past_the_output_budget_page_out_to_the_store() {
    let dir = std::env::temp_dir().join(format!("web-crawl-spill-{}", std::process::id()));
    let store = SpillStore::for_session_dir(&dir);
    let site = Site::start(false).await;
    let request = request(&site, &json!({"maxDepth": 1, "maxPages": 1}));
    let mut collected = Collected::new(&request, Some(&store));
    collected.chars = TOTAL_CHARS;
    let page = WebPage {
        url: "https://example.org/".to_string(),
        final_url: "https://example.org/".to_string(),
        title: Some("Example".to_string()),
        content_type: "text/html".to_string(),
        method: crate::web_read::ReadMethod::Html,
        markdown: "# Example\n\nBody".to_string(),
        outline: Vec::new(),
        links: Vec::new(),
        section: None,
        truncated: false,
        total_chars: 15,
        notes: Vec::new(),
        elapsed: Duration::ZERO,
    };
    collected.keep(&page);
    let handle = collected.pages[0]["handle"].as_str().unwrap().to_string();
    let stored = match store.lookup(handle.trim_start_matches("out:")).await {
        crate::kernel::spill::SpillLookup::Stored(stored) => Some(stored.text),
        other => panic!("the page was not stored: {other:?}"),
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        (collected.pages[0]["markdown"].clone(), stored),
        (json!(""), Some(page.text()))
    );
}

/// `render` with no browser to start fails with what to install.
#[tokio::test]
async fn rendering_without_a_browser_says_how_to_get_one() {
    let site = Site::start(false).await;
    let scope = pa_types::session_scope::SessionScope::new(
        "no-browser".to_string(),
        std::env::temp_dir(),
        BTreeMap::from([(
            browser::BROWSER_PATH_VAR.to_string(),
            "/nonexistent/chrome".to_string(),
        )]),
        BTreeSet::new(),
    );
    let _scope = pa_types::session_scope::enter(Some(scope));
    let error = WebCrawlEngine::direct()
        .crawl(&request(&site, &json!({"render": true})), None)
        .await
        .unwrap_err();
    assert_eq!(format!("{error:#}"), browser::NO_BROWSER);
}

/// Every process whose command line names this test's browser profiles.
fn browsers_running() -> Vec<u32> {
    let marker = format!("prime-agent-browser-{}-", std::process::id());
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            String::from_utf8_lossy(&cmdline)
                .contains(&marker)
                .then_some(pid)
        })
        .collect()
}

/// A rendered crawl sees the DOM after the page's scripts ran, and leaves no
/// browser process behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_rendered_crawl_reads_the_scripted_page_and_leaves_no_browser() {
    const CHROMIUM: &str = "/opt/pw-browsers/chromium";
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM} to render with");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let path = String::from_utf8_lossy(&buffer[..read])
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let body = if path == "/" {
                    "<!doctype html><html><head><title>Scripted</title></head><body><main id=\"m\"></main><script>document.getElementById('m').innerHTML='<h1>Written by script</h1><p>This paragraph exists only after the page script ran in a browser.</p>';</script></body></html>"
                } else {
                    ""
                };
                let status = if path == "/" { 200 } else { 404 };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    let scope = pa_types::session_scope::SessionScope::new(
        "render".to_string(),
        std::env::temp_dir(),
        BTreeMap::from([(browser::BROWSER_PATH_VAR.to_string(), CHROMIUM.to_string())]),
        BTreeSet::new(),
    );
    let crawl = {
        let _scope = pa_types::session_scope::enter(Some(scope));
        let request =
            parse_request(&json!({"url": format!("{base}/"), "render": true, "maxPages": 1}))
                .unwrap();
        WebCrawlEngine::direct().crawl(&request, None).await
    }
    .unwrap();
    assert_eq!(
        (
            crawl.pages[0]["markdown"]
                .as_str()
                .unwrap()
                .contains("This paragraph exists only after the page script ran"),
            browsers_running(),
        ),
        (true, Vec::new())
    );
}

/// A stopped cell stops its crawl at once: the cell ends within a second of
/// the stop although the site never answers and the crawl's deadline is ten
/// minutes away.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_cell_stops_its_crawl() {
    use crate::kernel::shared::HostRequestHandlers;
    use crate::tools::kernel_cell::{ExecuteStatus, KernelExecuteOptions, KernelProvisioner};
    use crate::tools::tool_definition::AbortSignal;

    let site = Site::start(true).await;
    let mut handlers = HostRequestHandlers::new();
    WebCrawlHost {
        engine: Arc::new(WebCrawlEngine::direct()),
        store: None,
        observer: None,
    }
    .register(&mut handlers);
    let kernel = crate::kernel::scheme::SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let cell = format!("(web-crawl \"{}/\" #:timeout 600)", site.base);
    let running = kernel.execute(
        &cell,
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            timeout_ms: None,
            on_stream: None,
            on_late_sent_agent_message: None,
            tool_call_id: None,
        },
    );
    let stop = async {
        // The crawl is under way once the site was asked for a page.
        while !site.hits().contains("/") {
            site.asked.notified().await;
        }
        signal.cancel();
        Instant::now()
    };
    let (result, stopped) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(running, stop)
    })
    .await
    .expect("the stopped cell ends");
    assert_eq!(
        (
            result.unwrap().status,
            stopped.elapsed() < Duration::from_secs(1)
        ),
        (ExecuteStatus::Aborted, true)
    );
}
