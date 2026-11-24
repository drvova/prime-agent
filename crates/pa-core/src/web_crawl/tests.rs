//! `web.crawl` over a small loopback site: no live network.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// A fixture page titled `title` that links `links`.
fn page(title: &str, links: &[&str]) -> String {
    let links = links.iter().fold(String::new(), |mut list, link| {
        write!(list, "<li><a href=\"{link}\">{link}</a></li>").unwrap();
        list
    });
    format!(
        "<!doctype html><html><head><title>{title}</title></head><body><main><h1>{title}</h1><p>The {title} page of the fixture site, with enough prose to be kept as the readable part.</p><ul>{links}</ul></main></body></html>"
    )
}

/// What a fixture site serves: each path's content type and body.
type Pages = HashMap<&'static str, (&'static str, String)>;

/// The site's pages: a home page linking two levels down, a robots.txt that
/// disallows `/private`, an off-site link, and a docs section.
fn pages() -> Pages {
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

/// A loopback server for `pages`, recording every path it was asked for.
/// `hang` makes every page but robots.txt wait forever instead.
struct Site {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    /// Each request's path and lowercased head (request line and headers).
    heads: Arc<Mutex<Vec<(String, String)>>>,
    asked: Arc<tokio::sync::Notify>,
}

impl Site {
    async fn start(hang: bool) -> Self {
        Self::serving(pages(), hang).await
    }

    async fn serving(pages: Pages, hang: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(tokio::sync::Notify::new());
        let pages = Arc::new(pages);
        let (recorded, received, notify) =
            (Arc::clone(&hits), Arc::clone(&heads), Arc::clone(&asked));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (pages, recorded, received, notify) = (
                    Arc::clone(&pages),
                    Arc::clone(&recorded),
                    Arc::clone(&received),
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
                    let target = head.split_whitespace().nth(1).unwrap_or("/");
                    // A proxy is asked for the absolute URL.
                    let path = match Url::parse(target) {
                        Ok(absolute) => absolute.path().to_string(),
                        Err(_) => target.to_string(),
                    };
                    received
                        .lock()
                        .unwrap()
                        .push((path.clone(), head.to_ascii_lowercase()));
                    recorded.lock().unwrap().push(path.clone());
                    notify.notify_one();
                    if hang && path != "/robots.txt" {
                        std::future::pending::<()>().await;
                    }
                    let (status, content_type, body) = match pages.get(path.as_str()) {
                        Some((content_type, body)) => (200, *content_type, body.clone()),
                        None => (404, "text/plain", "not found".to_string()),
                    };
                    // A `redirect` page sends its body as the Location; with
                    // an `!auth` entry every page needs that Authorization.
                    let mut extra = String::new();
                    let mut status = status;
                    if content_type == "redirect" {
                        status = 302;
                        extra = format!("Location: {body}\r\n");
                    }
                    if let Some((_, credentials)) =
                        pages.get("!auth").filter(|_| path != "/robots.txt")
                    {
                        if !head
                            .to_ascii_lowercase()
                            .contains(&format!("authorization: {credentials}").to_ascii_lowercase())
                        {
                            status = 401;
                            extra = "WWW-Authenticate: Basic realm=\"fixture\"\r\n".to_string();
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Site {
            base,
            hits,
            heads,
            asked,
        }
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
        .crawl(
            &request(&site, &json!({"maxDepth": 2})),
            &Identity::default(),
            None,
        )
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
        .crawl(
            &request(&site, &json!({"maxPages": 2})),
            &Identity::default(),
            None,
        )
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
            &Identity::default(),
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
            &Identity::default(),
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
        .crawl(
            &request(&site, &json!({"render": true})),
            &Identity::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(format!("{error:#}"), browser::NO_BROWSER);
}

/// The processes whose command line names this test's browser profiles,
/// once they are all gone or 10 s passed: a killed process leaves `/proc`
/// only when it has finished dying.
async fn browsers_left() -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let running = browsers_running();
        if running.is_empty() || Instant::now() > deadline {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

/// A rendered crawl sees the DOM after the page's scripts ran and after the
/// automation steps asked for (a click), sends the user's user agent and
/// cookies, and leaves no browser process behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_rendered_crawl_reads_the_scripted_page_and_leaves_no_browser() {
    const CHROMIUM: &str = "/opt/pw-browsers/chromium";
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM} to render with");
        return;
    }
    let site = Site::serving(
        HashMap::from([(
            "/",
            (
                "text/html",
                "<!doctype html><html><head><title>Scripted</title></head><body><main id=\"m\"></main><button id=\"more\" onclick=\"document.getElementById('m').insertAdjacentHTML('beforeend','<p>This paragraph appears only once the button was clicked.</p>')\">more</button><script>document.getElementById('m').innerHTML='<h1>Written by script</h1><p>This paragraph exists only after the page script ran in a browser.</p>';</script></body></html>".to_string(),
            ),
        )]),
        false,
    )
    .await;
    let base = site.base.clone();
    let scope = pa_types::session_scope::SessionScope::new(
        "render".to_string(),
        std::env::temp_dir(),
        BTreeMap::from([(browser::BROWSER_PATH_VAR.to_string(), CHROMIUM.to_string())]),
        BTreeSet::new(),
    );
    let crawl = {
        let _scope = pa_types::session_scope::enter(Some(scope));
        let request = parse_request(&json!({
            "url": format!("{base}/"),
            "render": true,
            "maxPages": 1,
            "actions": {"/": [{"Click": "#more"}, {"WaitFor": "#m p + p"}]},
        }))
        .unwrap();
        // The browser presents the user's settings too.
        let identity = Identity::from_settings(&crate::settings::WebCrawlSettings {
            user_agent: Some("ExampleBot/2.0".to_string()),
            cookies: Some("sid=abc".to_string()),
            ..Default::default()
        })
        .unwrap();
        WebCrawlEngine::direct()
            .crawl(&request, &identity, None)
            .await
    }
    .unwrap();
    let markdown = crawl.pages[0]["markdown"].as_str().unwrap();
    assert_eq!(
        (
            markdown.contains("This paragraph exists only after the page script ran"),
            markdown.contains("This paragraph appears only once the button was clicked"),
            heads_for(&site, "/")
                .iter()
                .any(|head| head.contains("user-agent: examplebot/2.0")
                    && head.contains("cookie: sid=abc")),
            browsers_left().await,
        ),
        (true, true, true, Vec::new())
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
        capture_observer: None,
        settings: None,
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

/// A path budget caps the pages crawled under that path, beside the rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_path_budget_caps_its_section() {
    let site = Site::start(false).await;
    let crawl = WebCrawlEngine::direct()
        .crawl(
            &request(&site, &json!({"maxDepth": 5, "budget": {"/a": 1, "*": 20}})),
            &Identity::default(),
            None,
        )
        .await
        .unwrap();
    // Only the budgeted section is pinned: which other pages a crawl reaches
    // before it settles varies with spider's scheduling once a budget is set.
    let kept = paths(&crawl);
    assert_eq!(
        (
            kept.iter()
                .filter(|path| path.starts_with("/a"))
                .collect::<Vec<_>>(),
            kept.contains(&"/".to_string()),
            site.hits().contains("/a/deep"),
        ),
        (vec![&"/a".to_string()], true, false)
    );
}

/// `fullResources` also follows the links a crawl skips by their extension
/// (stylesheets, scripts, media), each rendered as `web.read` renders it.
#[tokio::test(flavor = "multi_thread")]
async fn full_resources_keep_the_linked_documents() {
    let site = Site::serving(
        HashMap::from([
            ("/", ("text/html", page("Home", &["/style.css"]))),
            (
                "/style.css",
                ("text/css", "main { color: teal; }".to_string()),
            ),
        ]),
        false,
    )
    .await;
    let methods = |crawl: &Crawl| {
        crawl
            .pages
            .iter()
            .map(|page| page["method"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let engine = WebCrawlEngine::direct();
    let pages_only = engine
        .crawl(&request(&site, &json!({})), &Identity::default(), None)
        .await
        .unwrap();
    let everything = engine
        .crawl(
            &request(&site, &json!({"fullResources": true})),
            &Identity::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        (methods(&pages_only), methods(&everything)),
        (
            vec!["html".to_string()],
            vec!["html".to_string(), "text".to_string()]
        )
    );
}

/// A browser option on a crawl that does not render is refused before
/// anything is fetched.
#[test]
fn browser_options_need_render() {
    let refused = parse_request(&json!({"url": "https://example.org/", "waitSelector": "#main"}))
        .map(|_| ())
        .map_err(|error| error.to_string());
    assert_eq!(
        refused,
        Err("web.crawl waitSelector needs render: the browser does it".to_string())
    );
}

/// The request heads (lowercased) `site` received for `path`.
fn heads_for(site: &Site, path: &str) -> Vec<String> {
    site.heads
        .lock()
        .unwrap()
        .iter()
        .filter(|(asked, _)| asked == path)
        .map(|(_, head)| head.clone())
        .collect()
}

/// The user's `web.crawl` settings are read at each crawl: the same host
/// first crawls honestly (robots.txt obeyed, Prime Agent's user agent), and
/// after the settings change the next crawl ignores robots.txt and sends the
/// user's user agent, headers and cookies.
#[tokio::test(flavor = "multi_thread")]
async fn the_users_settings_change_what_the_next_crawl_presents() {
    let site = Site::start(false).await;
    let settings = Arc::new(Mutex::new(crate::settings::WebCrawlSettings::default()));
    let host = WebCrawlHost {
        engine: Arc::new(WebCrawlEngine::direct()),
        store: None,
        observer: None,
        capture_observer: None,
        settings: Some({
            let settings = Arc::clone(&settings);
            Arc::new(move || settings.lock().unwrap().clone()) as CrawlSettings
        }),
    };
    let payload = json!({"url": format!("{}/", site.base), "maxDepth": 2});
    host.handle(&payload).await.unwrap();
    let honest = (
        site.hits().contains("/private/secret"),
        heads_for(&site, "/"),
    );
    *settings.lock().unwrap() = crate::settings::WebCrawlSettings {
        respect_robots_txt: Some(false),
        user_agent: Some("ExampleBot/2.0".to_string()),
        headers: Some(BTreeMap::from([("X-Team".to_string(), "docs".to_string())])),
        cookies: Some("sid=abc".to_string()),
        ..Default::default()
    };
    site.heads.lock().unwrap().clear();
    host.handle(&payload).await.unwrap();
    let home = heads_for(&site, "/");
    assert_eq!(
        (
            honest.0,
            !honest.1.is_empty()
                && honest.1.iter().all(|head| {
                    head.contains("user-agent: mozilla/5.0 (compatible; prime-agent web.crawl")
                }),
            site.hits().contains("/private/secret"),
            !home.is_empty()
                && home.iter().all(|head| {
                    head.contains("user-agent: examplebot/2.0")
                        && head.contains("x-team: docs")
                        && head.contains("cookie: sid=abc")
                }),
        ),
        (false, true, true, true)
    );
}

/// Headers and cookies from the settings go to the crawled site only: a
/// redirect to another host is followed without them.
#[tokio::test(flavor = "multi_thread")]
async fn site_headers_never_follow_a_redirect_off_the_site() {
    let elsewhere = Site::serving(
        HashMap::from([("/landing", ("text/html", page("Landing", &[])))]),
        false,
    )
    .await;
    // The same server reached by another name is another host.
    let away = elsewhere.base.replace("127.0.0.1", "localhost");
    let site = Site::serving(
        HashMap::from([("/", ("redirect", format!("{away}/landing")))]),
        false,
    )
    .await;
    let identity = Identity::from_settings(&crate::settings::WebCrawlSettings {
        cookies: Some("sid=abc".to_string()),
        ..Default::default()
    })
    .unwrap();
    let _ = WebCrawlEngine::direct()
        .crawl(&request(&site, &json!({"maxDepth": 1})), &identity, None)
        .await;
    let (asked, followed) = (heads_for(&site, "/"), heads_for(&elsewhere, "/landing"));
    assert_eq!(
        (
            !asked.is_empty() && asked.iter().all(|head| head.contains("cookie: sid=abc")),
            !followed.is_empty() && followed.iter().all(|head| !head.contains("cookie:")),
        ),
        (true, true)
    );
}

/// With proxies in the settings every request goes through one of them, in
/// turn, asking for the site's absolute URL.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_goes_through_its_proxies_in_turn() {
    let (first, second) = (Site::start(false).await, Site::start(false).await);
    let identity = Identity::from_settings(&crate::settings::WebCrawlSettings {
        proxies: Some(vec![first.base.clone(), second.base.clone()]),
        ..Default::default()
    })
    .unwrap();
    let request = parse_request(&json!({"url": "http://docs.invalid/", "maxDepth": 2})).unwrap();
    let crawl = WebCrawlEngine::direct()
        .crawl(&request, &identity, None)
        .await
        .unwrap();
    let through = |site: &Site| {
        let heads = site.heads.lock().unwrap();
        !heads.is_empty()
            && heads
                .iter()
                .all(|(_, head)| head.starts_with("get http://docs.invalid/"))
    };
    assert_eq!(
        (crawl.pages.len(), through(&first), through(&second)),
        (6, true, true)
    );
}

/// A rendered crawl with `#:block`, `#:init-script`, `#:locale` and
/// `#:timezone`: the browser never asks for the blocked image, runs the init
/// script first, and shows the page the locale and time zone asked for.
#[tokio::test(flavor = "multi_thread")]
async fn a_rendered_crawl_blocks_injects_and_emulates() {
    const CHROMIUM: &str = "/opt/pw-browsers/chromium";
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM} to render with");
        return;
    }
    let site = Site::serving(
        HashMap::from([
            (
                "/",
                (
                    "text/html",
                    "<!doctype html><html><head><title>Seen</title></head><body><main id=\"m\"></main><img src=\"/pixel.png\"><script>document.getElementById('m').innerHTML='<h1>Seen</h1><p>The page saw the init marker '+window.__initMarker+', the locale '+Intl.DateTimeFormat().resolvedOptions().locale+' and the time zone '+Intl.DateTimeFormat().resolvedOptions().timeZone+'.</p>';</script></body></html>".to_string(),
                ),
            ),
            ("/pixel.png", ("image/png", "not really a png".to_string())),
        ]),
        false,
    )
    .await;
    let scope = pa_types::session_scope::SessionScope::new(
        "render-options".to_string(),
        std::env::temp_dir(),
        BTreeMap::from([(browser::BROWSER_PATH_VAR.to_string(), CHROMIUM.to_string())]),
        BTreeSet::new(),
    );
    let crawl = {
        let _scope = pa_types::session_scope::enter(Some(scope));
        let request = parse_request(&json!({
            "url": format!("{}/", site.base),
            "render": true,
            "maxPages": 1,
            "block": ["images"],
            "initScript": "window.__initMarker = 'early-bird';",
            "locale": "de-DE",
            "timezone": "Asia/Tokyo",
        }))
        .unwrap();
        WebCrawlEngine::direct()
            .crawl(&request, &Identity::default(), None)
            .await
    }
    .unwrap();
    let markdown = crawl.pages[0]["markdown"].as_str().unwrap_or_default();
    assert_eq!(
        (
            markdown.contains(
                "The page saw the init marker early-bird, the locale de-DE and the time zone Asia/Tokyo."
            ),
            site.hits().contains("/pixel.png"),
            browsers_left().await,
        ),
        (true, false, Vec::new())
    );
}

/// `web-screenshot` writes a PNG of the page and `web-pdf` a PDF, each
/// under the session's working directory, and neither leaves a browser.
#[tokio::test(flavor = "multi_thread")]
async fn captures_write_a_png_and_a_pdf_and_leave_no_browser() {
    const CHROMIUM: &str = "/opt/pw-browsers/chromium";
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM} to render with");
        return;
    }
    let site = Site::serving(
        HashMap::from([("/", ("text/html", page("Captured", &[])))]),
        false,
    )
    .await;
    let dir = std::env::temp_dir().join(format!("web-capture-{}", std::process::id()));
    let scope = pa_types::session_scope::SessionScope::new(
        "capture".to_string(),
        dir.clone(),
        BTreeMap::from([(browser::BROWSER_PATH_VAR.to_string(), CHROMIUM.to_string())]),
        BTreeSet::new(),
    );
    let (png, pdf) = {
        let _scope = pa_types::session_scope::enter(Some(scope));
        let png = capture::parse_request(&json!({
            "url": format!("{}/", site.base), "path": "shots/home.png", "kind": "png", "fullPage": true,
        }))
        .unwrap();
        let pdf = capture::parse_request(&json!({
            "url": format!("{}/", site.base), "path": "home.pdf", "kind": "pdf", "background": true,
        }))
        .unwrap();
        (
            capture::capture(&png, &Identity::default()).await.unwrap(),
            capture::capture(&pdf, &Identity::default()).await.unwrap(),
        )
    };
    let head = |path: &str| {
        std::fs::read(dir.join(path))
            .map(|bytes| bytes[..bytes.len().min(5)].to_vec())
            .unwrap_or_default()
    };
    let heads = (head("shots/home.png"), head("home.pdf"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        (
            heads,
            png["title"].clone(),
            pdf["path"].clone(),
            browsers_left().await,
        ),
        (
            (b"\x89PNG\r".to_vec(), b"%PDF-".to_vec()),
            json!("Captured"),
            json!(dir.join("home.pdf").display().to_string()),
            Vec::new(),
        )
    );
}

/// The user's `web.crawl.auth` opens a site behind HTTP Basic
/// authentication on a plain crawl.
#[tokio::test(flavor = "multi_thread")]
async fn basic_credentials_open_a_protected_site() {
    let mut protected = pages();
    // base64("ada:lovelace")
    protected.insert("!auth", ("", "Basic YWRhOmxvdmVsYWNl".to_string()));
    let site = Site::serving(protected, false).await;
    let identity = Identity::from_settings(
        &serde_json::from_value(json!({"auth": {"username": "ada", "password": "lovelace"}}))
            .unwrap(),
    )
    .unwrap();
    let crawl = WebCrawlEngine::direct()
        .crawl(&request(&site, &json!({"maxDepth": 2})), &identity, None)
        .await
        .unwrap();
    assert_eq!(
        (paths(&crawl), crawl.failed),
        (
            ["/", "/a", "/a/deep", "/b", "/docs/one", "/docs/two"]
                .map(str::to_string)
                .to_vec(),
            Vec::new()
        )
    );
}
