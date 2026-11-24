//! `web.crawl`: a site read page by page, each page in `web.read`'s shape.
//! The kernel's `(web-crawl url ...)` reaches it through the `web.crawl`
//! host request.
//!
//! The spider crate keeps the frontier: robots.txt (honoured unless the
//! user's settings say otherwise, [`identity`]), the same-site rule, the depth and page limits, include and exclude patterns,
//! the sitemap, a polite delay and concurrency. The fetch is pa-core's own:
//! spider hands each URL to [`BoundedFetch`], which reads it with
//! `web.read`'s client and fetcher (bounded bodies, redirects followed by
//! hand, the crawl's identity, the session's proxies and CA bundle), and
//! every page is rendered by `web.read`'s renderer, so a crawled page and a
//! read page are the same object. With `render` a local Chrome loads each
//! page instead ([`browser`]); spider drives it over `DevTools`.
//!
//! The crawl runs on the host's runtime, inside the cell's host request:
//! a stopped cell drops the request's future, which drops the crawl (its
//! fetch tasks are aborted with their `JoinSet`) and the browser. The whole
//! result is bounded: at most [`MAX_PAGES`] pages, each cut at `maxChars`,
//! and past [`TOTAL_CHARS`] of markdown a page's text is paged out to the
//! session's spill store under an `out:` handle instead.

mod automate;
mod browser;
mod capture;
mod extract;
mod host;
mod identity;
mod options;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use spider::fetch_engine::{EngineError, EngineRequest, EngineResponse, HttpFetchEngine};
use url::Url;

pub(crate) use automate::AutomateOutcome;
pub(crate) use host::{
    AutomateObserver, CaptureObserver, CrawlObserver, CrawlSettings, ExtractObserver, WebCrawlHost,
};
pub(crate) use identity::{environment as crawl_environment, Identity};

use crate::kernel::spill::{SpillCopy, SpillStore};
use crate::web_read::{Accept, Fetched, Fetcher, HttpStatusError, ReadRequest, WebPage};

/// Pages a crawl returns unless asked for more.
const DEFAULT_MAX_PAGES: u32 = 20;
/// The most pages any crawl returns.
pub(crate) const MAX_PAGES: u32 = 200;
/// Link hops from the start page followed unless asked otherwise.
const DEFAULT_MAX_DEPTH: usize = 3;
/// The deepest a crawl may be asked to go.
const MAX_DEPTH: usize = 25;
/// Characters of markdown kept per page unless asked otherwise.
const DEFAULT_MAX_CHARS: usize = 8_000;
/// Characters of markdown all pages of one result hold together; a page past
/// it is paged out.
pub(crate) const TOTAL_CHARS: usize = 400_000;
/// The whole crawl's default deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest deadline a crawl may ask for.
const MAX_TIMEOUT: Duration = Duration::from_mins(10);
/// Requests in flight at once: polite to the site.
const CONCURRENCY: usize = 4;
/// Pause between requests, in milliseconds (robots.txt's Crawl-delay wins
/// when it asks for more).
const DELAY_MS: u64 = 250;
/// One page's deadline.
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest page body a rendered crawl keeps.
const RENDERED_PAGE_BYTES: f64 = 8.0 * 1024.0 * 1024.0;
/// Model rounds `#:automate` runs on each page: a crawl multiplies them.
const AUTOMATE_STEPS: usize = 3;
/// What a crawl announces itself as unless the user's settings say otherwise.
const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; prime-agent web.crawl; +https://github.com/PrimeIntellect-ai/prime-agent)";

/// One crawl to run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CrawlRequest {
    pub(crate) url: Url,
    pub(crate) max_pages: u32,
    pub(crate) max_depth: usize,
    /// Regexes a followed URL must match one of (none: every URL).
    pub(crate) include: Vec<String>,
    /// Regexes no followed URL may match.
    pub(crate) exclude: Vec<String>,
    pub(crate) max_chars: usize,
    /// Follow links into the start host's subdomains too.
    pub(crate) subdomains: bool,
    /// Seed the frontier from the site's sitemap.xml as well.
    pub(crate) sitemap: bool,
    pub(crate) render: Render,
    pub(crate) timeout: Duration,
    /// How far the frontier reaches beyond the defaults.
    pub(crate) reach: options::Reach,
    /// What the browser does on each page of a rendered crawl.
    pub(crate) browsing: options::Browsing,
    /// A WARC file every response the crawl received is appended to.
    pub(crate) warc: Option<std::path::PathBuf>,
    /// Each HTML page's format and the transform before it.
    pub(crate) shaping: crate::web_read::Shaping,
    /// What to extract from each HTML page (`extract` on its entry).
    pub(crate) extract: Option<extract::Selectors>,
    /// Which cookies the browser starts with and keeps.
    pub(crate) cookies: CookieScope,
    /// The goal the user's `web.crawl.ai` model works toward on each page
    /// before it is read (`#:automate`).
    pub(crate) automate: Option<String>,
}

/// The cookies a browser load starts with and what becomes of the ones it
/// ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CookieScope {
    /// The session's jar: the cookies earlier rendered crawls and captures
    /// of the site left, and this load's added to them (a browser's way
    /// within one session).
    #[default]
    Session,
    /// An empty jar, dropped when the load ends.
    None,
}

impl CookieScope {
    /// The `cookies` option of a `who` payload: `session` (the default) or
    /// `none`.
    ///
    /// # Errors
    ///
    /// Any other value.
    pub(crate) fn parse(data: &Value, who: &str) -> anyhow::Result<CookieScope> {
        match data.get("cookies").and_then(Value::as_str) {
            None | Some("session") => Ok(CookieScope::Session),
            Some("none") => Ok(CookieScope::None),
            Some(other) => bail!("{who} cookies must be session or none; got {other:?}"),
        }
    }
}

/// A session's cookie jar for its rendered crawls and captures: in memory
/// only (never written to disk, never in a result), one per session, gone
/// with it.
pub(crate) type CookieJar = Arc<reqwest::cookie::Jar>;

/// How pages are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Render {
    /// Plain HTTP: the HTML the server sends.
    Http,
    /// A local headless Chrome: the DOM after the page's scripts ran.
    Browser,
    /// Plain HTTP first; a page whose HTML says it needs JavaScript (an
    /// app shell, a framework's client bundle, a `<noscript>` asking for
    /// it) is loaded again in the browser (spider's `crawl_smart`).
    Smart,
}

/// What adoption telemetry learns of one crawl: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrawlOutcome {
    pub(crate) render: Render,
    /// Pages returned; `None` when the crawl failed outright.
    pub(crate) pages: Option<usize>,
    pub(crate) elapsed: Duration,
    /// The user's `web.crawl` settings changed what the crawl presents.
    pub(crate) identity: bool,
    /// The crawl wrote a WARC file.
    pub(crate) archived: bool,
    /// What the crawl did with each page beyond reading it.
    pub(crate) per_page: PageWork,
}

/// What a crawl asked done to each page: counts for adoption telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PageWork {
    /// A `#:format` or a `#:transform`.
    pub(crate) shaped: bool,
    /// An `#:extract`.
    pub(crate) extracted: bool,
    /// The browser started with cookies the session's jar held for the site.
    pub(crate) session_cookies: bool,
    /// Model requests `#:automate` made, over all pages; `None` without it.
    pub(crate) automation_calls: Option<u32>,
}

/// What adoption telemetry learns of one `web-extract`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExtractOutcome {
    pub(crate) ok: bool,
}

/// What adoption telemetry learns of one `web-screenshot` or `web-pdf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureOutcome {
    /// A PDF rather than a screenshot.
    pub(crate) pdf: bool,
    pub(crate) ok: bool,
    /// The browser started with cookies the session's jar held for the page.
    pub(crate) session_cookies: bool,
}

/// A finished crawl.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Crawl {
    pub(crate) url: String,
    pub(crate) render: Render,
    /// Each page in `web.read`'s reply shape, in the order they arrived; a
    /// page past the output budget also carries `handle`.
    pub(crate) pages: Vec<Value>,
    /// Pages the site answered with an error: `{url, status}`.
    pub(crate) failed: Vec<Value>,
    /// The crawl ended early (its deadline) or left pages unread (its
    /// limit, or pages it could not keep up with).
    pub(crate) truncated: bool,
    pub(crate) notes: Vec<String>,
    pub(crate) elapsed: Duration,
    /// The WARC file written and the records in it.
    pub(crate) warc: Option<(String, u64)>,
    /// What `#:automate` cost over all pages: model requests and tokens.
    pub(crate) automation: Option<(u32, u64)>,
}

/// The crawler: the client shared with `web.read`, and where test sites
/// are allowed.
pub(crate) struct WebCrawlEngine {
    reader: Arc<crate::web_read::WebReadEngine>,
}

impl WebCrawlEngine {
    /// The process-wide engine, over the shared reader's client.
    pub(crate) fn shared() -> Arc<WebCrawlEngine> {
        static SHARED: std::sync::OnceLock<Arc<WebCrawlEngine>> = std::sync::OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(WebCrawlEngine {
                reader: crate::web_read::WebReadEngine::shared(),
            })
        }))
    }

    /// An engine whose client goes straight to loopback test sites.
    #[cfg(test)]
    pub(crate) fn direct() -> Self {
        WebCrawlEngine {
            reader: Arc::new(crate::web_read::WebReadEngine::direct()),
        }
    }

    /// Crawl `request.url` within the request's deadline, paging pages past
    /// the output budget into `store` when the session has one. A rendered
    /// crawl's browser pages share `jar`: each starts with the jar's cookies
    /// for its URL that the browser lacks, and the cookies the page holds
    /// once loaded go back into it (spider's `chrome_cookie_jar`). With
    /// `request.automate`, `automation`'s model works toward that goal on
    /// each page before it is read (spider's `remote_multimodal`), its
    /// requests on pa-core's client.
    ///
    /// # Errors
    ///
    /// An invalid include or exclude pattern, a render with no browser (or
    /// one that would not start), a start page that could not be read, or
    /// `#:automate` without its model.
    #[tracing::instrument(skip_all, fields(host = request.url.host_str().unwrap_or_default()))]
    pub(crate) async fn crawl(
        &self,
        request: &CrawlRequest,
        identity: &Identity,
        store: Option<&SpillStore>,
        jar: &CookieJar,
        automation: Option<&automate::Model>,
    ) -> anyhow::Result<Crawl> {
        let started = Instant::now();
        for pattern in request.include.iter().chain(&request.exclude) {
            regex::Regex::new(pattern)
                .map_err(|error| anyhow!("web.crawl: invalid pattern {pattern:?}: {error}"))?;
        }
        let clients = identity.clients(self.reader.client())?;
        let mut website = spider::website::Website::new(request.url.as_str());
        website
            .with_subdomains(request.subdomains)
            .with_tld(false)
            .with_limit(request.max_pages)
            .with_depth(request.max_depth)
            .with_delay(DELAY_MS)
            .with_concurrency_limit(Some(CONCURRENCY))
            .with_request_timeout(Some(PAGE_TIMEOUT))
            .with_redirect_limit(8)
            .with_max_page_bytes(Some(RENDERED_PAGE_BYTES))
            .with_fetch_engine(BoundedFetch {
                clients: clients.clone(),
                next: std::sync::atomic::AtomicUsize::new(0),
                identity: identity.clone(),
                start: request.url.clone(),
                subdomains: request.subdomains,
                reach: request.reach.clone(),
            });
        identity.apply(&mut website, request.render);
        let compiled = |patterns: &[String]| {
            patterns
                .iter()
                .map(|pattern| spider::compact_str::CompactString::from(pattern.as_str()))
                .collect::<Vec<_>>()
        };
        if !request.include.is_empty() {
            website.with_whitelist_url(Some(compiled(&request.include)));
        }
        if !request.exclude.is_empty() {
            website.with_blacklist_url(Some(compiled(&request.exclude)));
        }
        // Upstream reads the site's /sitemap.xml after its links unless told not to.
        website.with_ignore_sitemap(!request.sitemap);
        request.reach.apply(&mut website, request.max_pages);
        // Smart mode's upgrade, whatever `SPIDER_CHROME_RENDER_UPGRADE` in
        // the daemon's environment says.
        website.with_enhancement(
            spider::configuration::CrawlEnhancement::RenderUpgrade,
            request.render == Render::Smart,
        );
        request.browsing.apply(&mut website);
        let mut notes = Vec::new();
        if request.render != Render::Http
            && identity
                .site_headers
                .contains_key(reqwest::header::AUTHORIZATION)
        {
            notes.push("web.crawl.auth is sent on plain HTTP reads only: a browser would send it to every host a page loads from".to_string());
        }
        // Kept until the crawl's end: dropping it kills the browser.
        let browser = match request.render {
            Render::Http => None,
            Render::Browser | Render::Smart => {
                website.cookie_jar = Arc::clone(jar);
                website.with_chrome_cookie_jar(true);
                let program = browser::find().ok_or_else(|| anyhow!(browser::NO_BROWSER))?;
                let started = browser::Browser::start(&program, &[]).await?;
                website.with_chrome_connection(Some(started.endpoint.clone()));
                Some(started)
            }
        };
        match (&request.automate, automation) {
            (Some(goal), Some(model)) => {
                let mut configs = spider_agent::RemoteMultimodalConfigs::new(
                    model.endpoint.as_str(),
                    model.name.as_str(),
                );
                configs.api_key = model.key.as_ref().map(|automate::ApiKey(key)| key.clone());
                configs.user_message_extra = Some(format!("The goal on this page: {goal}"));
                configs.cfg = spider_agent::RemoteMultimodalConfig {
                    max_rounds: AUTOMATE_STEPS,
                    screenshot: false,
                    automation_timeout_ms: u64::try_from(automate::REQUEST_TIMEOUT.as_millis())
                        .ok(),
                    ..spider_agent::RemoteMultimodalConfig::default()
                };
                configs.client = Some(automate::client()?);
                website.with_remote_multimodal(Some(configs));
            }
            (Some(_), None) => bail!("web.crawl #:automate needs the user's web.crawl.ai model"),
            (None, Some(_) | None) => {}
        }
        // spider reads robots.txt and the sitemap with this one.
        website.set_http_client(clients[0].clone());
        // Room for every page the limit allows plus its failures.
        let mut pages_rx = website.subscribe(request.max_pages as usize * 2 + 16);
        let mut collected = Collected::new(request, store);
        if let Some(path) = &request.warc {
            pa_types::session_scope::ensure_live()?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            collected.warc = Some((
                tokio::fs::File::create(path)
                    .await
                    .with_context(|| format!("creating the WARC file {}", path.display()))?,
                0,
            ));
        }
        let deadline_hit = {
            let run = async {
                let crawl = async {
                    match request.render {
                        Render::Http => website.crawl_raw().await,
                        Render::Browser => website.crawl().await,
                        Render::Smart => website.crawl_smart().await,
                    }
                };
                tokio::pin!(crawl);
                loop {
                    tokio::select! {
                        received = pages_rx.recv() => match received {
                            Ok(page) => collected.take(&page).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                                collected.missed += missed;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                (&mut crawl).await;
                                break;
                            }
                        },
                        () = &mut crawl => break,
                    }
                }
                // What was sent before the crawl returned.
                while let Ok(page) = pages_rx.try_recv() {
                    collected.take(&page).await;
                }
            };
            tokio::time::timeout(request.timeout, run).await.is_err()
        };
        drop(browser);
        if deadline_hit {
            notes.push(format!(
                "stopped at the {}s deadline; pass a longer #:timeout or a narrower #:include",
                request.timeout.as_secs()
            ));
        }
        if collected.missed > 0 {
            notes.push(format!(
                "{} pages arrived faster than they could be kept and were dropped",
                collected.missed
            ));
        }
        let reached_limit = collected.pages.len() >= request.max_pages as usize;
        if reached_limit {
            notes.push(format!(
                "stopped at #:max-pages {}; pass a larger one (at most {MAX_PAGES}) for more",
                request.max_pages
            ));
        }
        let warc = match collected.warc.take() {
            Some((mut file, records)) => {
                use tokio::io::AsyncWriteExt as _;
                if let Err(error) = file.flush().await {
                    notes.push(format!("the WARC file could not be finished: {error}"));
                }
                request
                    .warc
                    .as_ref()
                    .map(|path| (path.display().to_string(), records))
            }
            None => None,
        };
        notes.append(&mut collected.notes);
        if collected.pages.is_empty() && collected.failed.is_empty() && !deadline_hit {
            bail!(
                "web.crawl read no page of {}: the site refused or robots.txt disallows it",
                request.url
            );
        }
        Ok(Crawl {
            url: request.url.to_string(),
            render: request.render,
            truncated: deadline_hit || reached_limit || collected.missed > 0,
            pages: collected.pages,
            failed: collected.failed,
            notes,
            elapsed: started.elapsed(),
            warc,
            automation: request
                .automate
                .as_ref()
                .map(|_| (collected.model_calls, collected.model_tokens)),
        })
    }
}

/// The pages a crawl has kept so far.
struct Collected<'a> {
    request: &'a CrawlRequest,
    store: Option<&'a SpillStore>,
    pages: Vec<Value>,
    failed: Vec<Value>,
    notes: Vec<String>,
    /// Markdown characters the kept pages hold.
    chars: usize,
    /// Pages the broadcast dropped before they were taken.
    missed: u64,
    /// Model requests and tokens `#:automate` spent on the pages taken.
    model_calls: u32,
    model_tokens: u64,
    /// The WARC file and the records written to it; `None` once a write
    /// failed (with a note).
    warc: Option<(tokio::fs::File, u64)>,
}

impl<'a> Collected<'a> {
    fn new(request: &'a CrawlRequest, store: Option<&'a SpillStore>) -> Self {
        Collected {
            request,
            store,
            pages: Vec::new(),
            failed: Vec::new(),
            notes: Vec::new(),
            chars: 0,
            missed: 0,
            model_calls: 0,
            model_tokens: 0,
            warc: None,
        }
    }

    /// Keep one crawled page: render it as `web.read` would, or note why not.
    async fn take(&mut self, page: &spider::page::Page) {
        if self.pages.len() >= self.request.max_pages as usize {
            return;
        }
        if let Some((file, records)) = &mut self.warc {
            use tokio::io::AsyncWriteExt as _;
            let written = match (
                pa_types::session_scope::ensure_live(),
                spider::utils::warc::serialize_page(page),
            ) {
                (Ok(()), Some(record)) => file.write_all(&record).await,
                (Ok(()), None) => Ok(()),
                (Err(error), Some(_) | None) => Err(error),
            };
            match written {
                Ok(()) => *records += 1,
                Err(error) => {
                    self.notes.push(format!(
                        "the WARC file stopped at {records} records: {error}"
                    ));
                    self.warc = None;
                }
            }
        }
        // The model's work on the page, counted whatever becomes of it.
        let automation = page.remote_multimodal_usage.as_ref().map(|rounds| {
            let total = |of: fn(&spider_agent::AutomationUsage) -> u32| {
                rounds.iter().map(of).map(u64::from).sum::<u64>()
            };
            let calls = total(|usage| usage.llm_calls);
            self.model_calls = self
                .model_calls
                .saturating_add(u32::try_from(calls).unwrap_or(u32::MAX));
            self.model_tokens = self
                .model_tokens
                .saturating_add(total(|usage| usage.total_tokens));
            json!({
                "llmCalls": calls,
                "promptTokens": total(|usage| usage.prompt_tokens),
                "completionTokens": total(|usage| usage.completion_tokens),
                "totalTokens": total(|usage| usage.total_tokens),
            })
        });
        let status = page.status_code.as_u16();
        if !page.status_code.is_success() {
            self.failed
                .push(json!({"url": page.get_url(), "status": status}));
            return;
        }
        let (Ok(url), Ok(final_url)) =
            (Url::parse(page.get_url()), Url::parse(page.get_url_final()))
        else {
            return;
        };
        if !on_site(
            &self.request.url,
            self.request.subdomains,
            &self.request.reach,
            &final_url,
        ) {
            self.notes.push(format!(
                "{url} redirected off the site to {final_url}; not kept"
            ));
            return;
        }
        let media_type = page
            .headers
            .as_ref()
            .and_then(|headers| headers.get(reqwest::header::CONTENT_TYPE))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .or_else(|| (self.request.render != Render::Http).then(|| "text/html".to_string()));
        let html = matches!(
            media_type.as_deref(),
            Some("text/html" | "application/xhtml+xml")
        );
        let fetched = Fetched {
            final_url,
            media_type,
            body: page.get_html_bytes_u8().to_vec(),
        };
        let extracted = match (&self.request.extract, html) {
            (Some(selectors), true) => {
                let (selectors, body) = (selectors.clone(), fetched.body.clone());
                Some(
                    crate::web_read::on_own_thread("web-extract", move || {
                        selectors.extract(&String::from_utf8_lossy(&body))
                    })
                    .await,
                )
            }
            (Some(_), false) | (None, true | false) => None,
        };
        let read = ReadRequest {
            url,
            max_chars: self.request.max_chars,
            section: None,
            timeout: self.request.timeout,
            shaping: self.request.shaping.clone(),
        };
        let rendered = match crate::web_read::render(fetched, &self.request.shaping).await {
            Ok(document) => crate::web_read::finish(&read, document, Duration::ZERO),
            Err(error) => Err(error),
        };
        match rendered {
            Ok(rendered) => self.keep(&rendered, extracted, automation),
            Err(error) => self.notes.push(format!("{}: {error:#}", read.url)),
        }
    }

    /// Add `page` to the result, with what was extracted from it and what
    /// `#:automate`'s model spent on it, or page it out once the budget is
    /// spent (the extraction and the usage stay).
    fn keep(
        &mut self,
        page: &WebPage,
        extracted: Option<anyhow::Result<Value>>,
        automation: Option<Value>,
    ) {
        let mut entry = page.to_json();
        if let Some(usage) = automation {
            entry["automation"] = usage;
        }
        match extracted {
            None => {}
            Some(Ok(values)) => entry["extract"] = values,
            Some(Err(error)) => {
                if let Some(notes) = entry["notes"].as_array_mut() {
                    notes.push(json!(format!("nothing extracted: {error:#}")));
                }
            }
        }
        let chars = page.markdown.chars().count();
        if self.chars + chars <= TOTAL_CHARS {
            self.chars += chars;
            self.pages.push(entry);
            return;
        }
        let note = match self.store {
            Some(store) => {
                let handle = store.spill(SpillCopy::of_text(&page.text()));
                let marker = handle.marker(chars);
                entry["handle"] = json!(format!("{}{}", crate::kernel::spill::HANDLE_PREFIX, handle.id));
                format!("past the crawl's {TOTAL_CHARS}-char output budget: the page is paged out {marker}; read it with ctx-page")
            }
            None => format!(
                "past the crawl's {TOTAL_CHARS}-char output budget and this session keeps no paged-out output: read it with web-read"
            ),
        };
        if let Some(notes) = entry["notes"].as_array_mut() {
            notes.push(json!(note));
        }
        entry["markdown"] = json!("");
        entry["truncated"] = json!(true);
        entry["text"] = json!(note);
        self.pages.push(entry);
    }
}

/// Whether `candidate` is on the crawled site: `start`'s host, with
/// `subdomains` one under it, or one of the crawl's other hosts.
fn on_site(start: &Url, subdomains: bool, reach: &options::Reach, candidate: &Url) -> bool {
    match (start.host_str(), candidate.host_str()) {
        (Some(start), Some(candidate)) => {
            let start = start.trim_start_matches("www.");
            let candidate = candidate.trim_start_matches("www.");
            candidate == start
                || subdomains && candidate.ends_with(&format!(".{start}"))
                || reach.includes_host(candidate)
        }
        (Some(_) | None, None) | (None, Some(_)) => false,
    }
}

impl Crawl {
    /// The model-readable index: what was crawled, one line per page, the
    /// failures and the notes. The pages' text is in `pages`.
    pub(crate) fn text(&self) -> String {
        let mut lines = vec![format!(
            "Crawled {} page{} from {} ({}){}",
            self.pages.len(),
            if self.pages.len() == 1 { "" } else { "s" },
            self.url,
            match self.render {
                Render::Http => "http",
                Render::Browser => "rendered",
                Render::Smart => "smart",
            },
            if self.truncated { ", truncated" } else { "" }
        )];
        for (index, page) in self.pages.iter().enumerate() {
            let title = page["title"].as_str().unwrap_or("(untitled)");
            let url = page["finalUrl"].as_str().unwrap_or_default();
            let chars = page["totalChars"].as_u64().unwrap_or_default();
            let handle = page["handle"]
                .as_str()
                .map(|handle| format!(", paged out as {handle}"))
                .unwrap_or_default();
            lines.push(format!(
                "{}. {title} - {url} ({chars} chars{handle})",
                index + 1
            ));
        }
        for failure in &self.failed {
            lines.push(format!(
                "Failed: {} (HTTP {})",
                failure["url"].as_str().unwrap_or_default(),
                failure["status"]
            ));
        }
        if let Some((calls, tokens)) = self.automation {
            lines.push(format!(
                "#:automate: {calls} model request{} ({tokens} tokens), billed to the user's web.crawl.ai account",
                if calls == 1 { "" } else { "s" }
            ));
        }
        lines.extend(self.notes.iter().map(|note| format!("Note: {note}")));
        lines.join("\n")
    }

    /// The host-request reply.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "render": match self.render {
                Render::Http => false,
                Render::Browser | Render::Smart => true,
            },
            "mode": match self.render {
                Render::Http => "http",
                Render::Browser => "render",
                Render::Smart => "smart",
            },
            "pages": self.pages,
            "failed": self.failed,
            "truncated": self.truncated,
            "notes": self.notes,
            "warc": self.warc.as_ref().map(|(path, records)| json!({"path": path, "records": records})),
            "automation": self.automation.map(|(calls, tokens)| json!({"llmCalls": calls, "totalTokens": tokens})),
            "text": self.text(),
        })
    }
}

/// spider's per-URL fetch, done by `web.read`'s fetcher: the body is capped,
/// redirects are followed by hand, and no failure is reported as a TLS
/// handshake failure (which would make spider retry over plain http). It
/// sends the crawl's identity, its site headers to the site's hosts only.
struct BoundedFetch {
    /// One per proxy, taken in turn.
    clients: Vec<reqwest::Client>,
    next: std::sync::atomic::AtomicUsize,
    identity: Identity,
    start: Url,
    subdomains: bool,
    reach: options::Reach,
}

impl HttpFetchEngine for BoundedFetch {
    // spider declares the trait with `#[async_trait]`: this is that
    // signature written out.
    fn fetch<'life0, 'life1, 'async_trait>(
        &'life0 self,
        request: EngineRequest<'life1>,
    ) -> Pin<Box<dyn Future<Output = Result<EngineResponse, EngineError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let asked = Url::parse(request.url).map_err(|_| EngineError::Request)?;
            let turn = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let on_site = |url: &Url| on_site(&self.start, self.subdomains, &self.reach, url);
            let reader = Fetcher {
                client: &self.clients[turn % self.clients.len()],
                base_override: None,
                timeout: PAGE_TIMEOUT,
                identity: Some(crate::web_read::RequestIdentity {
                    user_agent: self.identity.user_agent(),
                    site_headers: &self.identity.site_headers,
                    on_site: &on_site,
                }),
            };
            match reader.get(&asked, Accept::Document).await {
                Ok(fetched) => {
                    let mut headers = reqwest::header::HeaderMap::new();
                    if let Some(value) = fetched
                        .media_type
                        .as_deref()
                        .and_then(|media| reqwest::header::HeaderValue::from_str(media).ok())
                    {
                        headers.insert(reqwest::header::CONTENT_TYPE, value);
                    }
                    Ok(EngineResponse {
                        status_code: reqwest::StatusCode::OK,
                        final_url: (fetched.final_url != asked)
                            .then(|| fetched.final_url.to_string()),
                        headers,
                        declared_content_length: Some(fetched.body.len() as u64),
                        body: fetched.body,
                        served: true,
                        ..EngineResponse::default()
                    })
                }
                Err(error) => Err(match error.downcast_ref::<HttpStatusError>() {
                    Some(status) => EngineError::Status(status.status),
                    None if format!("{error:#}").contains("timed out") => EngineError::Timeout,
                    None => EngineError::Other(format!("{error:#}")),
                }),
            }
        })
    }
}

/// Parse the `web.crawl` payload into a request.
///
/// # Errors
///
/// A missing or non-http(s) URL, or an out-of-range option.
pub(crate) fn parse_request(data: &Value) -> anyhow::Result<CrawlRequest> {
    let raw = data
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| anyhow!("web.crawl requires a url"))?;
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let url = Url::parse(&with_scheme)
        .map_err(|error| anyhow!("web.crawl: invalid url {raw:?}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web.crawl crawls http(s) URLs; got {}", url.scheme());
    }
    let count = |key: &str, default: u64, max: u64| -> anyhow::Result<u64> {
        match data.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_u64()
                .filter(|count| *count >= 1)
                .map(|count| count.min(max))
                .ok_or_else(|| anyhow!("web.crawl {key} must be a positive integer")),
        }
    };
    let patterns = |key: &str| -> anyhow::Result<Vec<String>> {
        match data.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(pattern)) => Ok(vec![pattern.clone()]),
            Some(Value::Array(patterns)) => patterns
                .iter()
                .map(|pattern| {
                    pattern
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("web.crawl {key} takes strings"))
                })
                .collect(),
            Some(_) => bail!("web.crawl {key} must be a regex string or a list of them"),
        }
    };
    let flag = |key: &str| data.get(key).and_then(Value::as_bool).unwrap_or(false);
    let timeout = match data.get("timeoutSeconds").and_then(Value::as_f64) {
        None => DEFAULT_TIMEOUT,
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Duration::from_secs_f64(seconds).min(MAX_TIMEOUT)
        }
        Some(_) => bail!("web.crawl timeoutSeconds must be positive"),
    };
    let render = match (data.get("mode").and_then(Value::as_str), flag("render")) {
        (None | Some("http"), false) => Render::Http,
        (None | Some("render"), true) | (Some("render"), false) => Render::Browser,
        (Some("smart"), false) => Render::Smart,
        (Some("http" | "smart"), true) => {
            bail!("web.crawl render #t is mode render; pass one of them")
        }
        (Some(other), _) => bail!("web.crawl mode must be http, render or smart; got {other:?}"),
    };
    let automate = match (data.get("automate"), render) {
        (None | Some(Value::Null), _) => None,
        (Some(Value::String(goal)), Render::Browser) if !goal.trim().is_empty() => {
            if goal.chars().count() > automate::MAX_PROMPT_CHARS {
                bail!(
                    "web.crawl automate is longer than {} chars",
                    automate::MAX_PROMPT_CHARS
                );
            }
            Some(goal.trim().to_string())
        }
        (Some(Value::String(_)), Render::Http | Render::Smart) => {
            bail!(
                "web.crawl automate drives every page in the browser: pass render #t (mode render)"
            )
        }
        (Some(_), _) => bail!("web.crawl automate is the goal, a non-empty string"),
    };
    Ok(CrawlRequest {
        url,
        max_pages: u32::try_from(count(
            "maxPages",
            u64::from(DEFAULT_MAX_PAGES),
            u64::from(MAX_PAGES),
        )?)
        .unwrap_or(MAX_PAGES),
        max_depth: usize::try_from(count(
            "maxDepth",
            DEFAULT_MAX_DEPTH as u64,
            MAX_DEPTH as u64,
        )?)
        .unwrap_or(MAX_DEPTH),
        include: patterns("include")?,
        exclude: patterns("exclude")?,
        max_chars: usize::try_from(count(
            "maxChars",
            DEFAULT_MAX_CHARS as u64,
            crate::web_read::MAX_MAX_CHARS as u64,
        )?)
        .unwrap_or(DEFAULT_MAX_CHARS),
        subdomains: flag("subdomains"),
        sitemap: flag("sitemap"),
        render,
        timeout,
        reach: options::Reach::parse(data)?,
        browsing: options::Browsing::parse(data, render != Render::Http)?,
        warc: data
            .get("warc")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| {
                let path = std::path::PathBuf::from(path);
                match (path.is_absolute(), pa_types::session_scope::current_dir()) {
                    (false, Ok(cwd)) => cwd.join(path),
                    (true, _) | (false, Err(_)) => path,
                }
            }),
        shaping: crate::web_read::Shaping::parse(data, "web.crawl")?,
        extract: extract::Selectors::parse(data, "extract", "web.crawl")?,
        cookies: CookieScope::parse(data, "web.crawl")?,
        automate,
    })
}
