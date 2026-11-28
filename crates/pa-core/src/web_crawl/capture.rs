//! `web.capture`: one page loaded in a local headless Chrome and saved as a
//! PNG screenshot or a PDF. The kernel's `(web-capture url path #:format
//! "png"|"pdf" ...)` reaches it.
//!
//! The browser is started as a rendered crawl's is ([`super::browser`]):
//! its own process group, in the orphan journal, killed when the capture
//! ends for any reason (a stopped cell drops the request's future, which
//! drops the browser). The page sees the user's `web.crawl` identity: its
//! user agent, headers, cookies (for the page's URL) and first proxy; never
//! the Basic credentials, which a browser would send to every host. A relative `path` is
//! taken under the session's working directory. The page starts with the
//! session jar's cookies for its URL and leaves the cookies it then holds
//! in the jar, as a rendered crawl's pages do ([`super::CookieJar`]).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use futures::StreamExt;
use serde_json::{json, Value};
use spider::chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use spider::chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, Headers, SetCookiesParams, SetExtraHttpHeadersParams,
};
use spider::chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, PrintToPdfParams,
};
use spider::chromiumoxide::page::ScreenshotParams;
use url::Url;

use super::browser;
use super::identity::Identity;
use super::{CookieJar, CookieScope};

/// A capture's default deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest deadline a capture may ask for.
const MAX_TIMEOUT: Duration = Duration::from_mins(2);
/// The longest wait after the page loaded.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// The largest viewport side, in CSS pixels.
const MAX_VIEWPORT: u64 = 8_192;

/// What a capture saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureKind {
    Screenshot {
        /// The whole page, not only the viewport.
        full_page: bool,
    },
    Pdf {
        landscape: bool,
        /// Print background colours and images.
        background: bool,
    },
}

/// One capture to make.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CaptureRequest {
    pub(crate) url: Url,
    pub(crate) path: PathBuf,
    pub(crate) kind: CaptureKind,
    pub(crate) viewport: Option<(u32, u32)>,
    /// A fixed wait after the page loaded.
    pub(crate) wait: Option<Duration>,
    /// JavaScript run in the document before the page's own scripts.
    pub(crate) init_script: Option<String>,
    pub(crate) timeout: Duration,
    /// Which cookies the page starts with and keeps.
    pub(crate) cookies: CookieScope,
}

/// Parse a `web.capture` payload.
///
/// # Errors
///
/// A missing or non-http(s) URL, a missing path, or an option of the wrong
/// shape.
pub(crate) fn parse_request(data: &Value) -> anyhow::Result<CaptureRequest> {
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let raw = text("url").ok_or_else(|| anyhow!("web.capture requires a url"))?;
    let url =
        Url::parse(raw).map_err(|error| anyhow!("web.capture: invalid url {raw:?}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web.capture captures http(s) URLs; got {}", url.scheme());
    }
    let path = PathBuf::from(text("path").ok_or_else(|| anyhow!("web.capture requires a path"))?);
    let path = match (path.is_absolute(), pa_types::session_scope::current_dir()) {
        (false, Ok(cwd)) => cwd.join(path),
        (true, _) | (false, Err(_)) => path,
    };
    let flag = |key: &str| data.get(key).and_then(Value::as_bool).unwrap_or(false);
    let kind = match data.get("kind").and_then(Value::as_str) {
        Some("png") => CaptureKind::Screenshot {
            full_page: flag("fullPage"),
        },
        Some("pdf") => CaptureKind::Pdf {
            landscape: flag("landscape"),
            background: flag("background"),
        },
        Some(_) | None => bail!("web.capture kind must be png or pdf"),
    };
    let viewport = match data.get("viewport") {
        None | Some(Value::Null) => None,
        Some(Value::Array(sides)) if sides.len() == 2 => {
            let side = |value: &Value| {
                value
                    .as_u64()
                    .filter(|side| (1..=MAX_VIEWPORT).contains(side))
                    .and_then(|side| u32::try_from(side).ok())
            };
            match (side(&sides[0]), side(&sides[1])) {
                (Some(width), Some(height)) => Some((width, height)),
                (Some(_) | None, None) | (None, Some(_)) => {
                    bail!("web.capture viewport sides must be 1 to {MAX_VIEWPORT} pixels")
                }
            }
        }
        Some(_) => bail!("web.capture viewport must be (width height)"),
    };
    let seconds = |key: &str, max: Duration| -> anyhow::Result<Option<Duration>> {
        match data.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .map(|seconds| Some(Duration::from_secs_f64(seconds).min(max)))
                .ok_or_else(|| anyhow!("web.capture {key} must be a positive number of seconds")),
        }
    };
    Ok(CaptureRequest {
        url,
        path,
        kind,
        viewport,
        wait: seconds("waitSeconds", MAX_WAIT)?,
        init_script: text("initScript").map(str::to_string),
        timeout: seconds("timeoutSeconds", MAX_TIMEOUT)?.unwrap_or(DEFAULT_TIMEOUT),
        cookies: CookieScope::parse(data, "web.capture")?,
    })
}

/// How one page is opened in a fresh browser ([`with_page`]).
pub(crate) struct PageSetup<'a> {
    pub(crate) url: &'a Url,
    pub(crate) identity: &'a Identity,
    pub(crate) jar: &'a CookieJar,
    pub(crate) viewport: Option<(u32, u32)>,
    pub(crate) init_script: Option<&'a str>,
    pub(crate) timeout: Duration,
    /// What the deadline's error says was being done (`capturing`).
    pub(crate) doing: &'static str,
}

/// Open `setup.url` in a fresh headless browser with the user's identity
/// and the cookies `setup.jar` holds for it, run `work` on the loaded page,
/// and put the cookies the page then holds back into the jar (domain, path,
/// expiry and flags kept), all within `setup.timeout`. The browser is
/// killed when this returns or its future is dropped.
///
/// # Errors
///
/// No browser, one that would not start, a page that would not load or
/// set up, `work`'s error, or the deadline.
pub(crate) async fn with_page<T>(
    setup: PageSetup<'_>,
    work: impl AsyncFnOnce(&spider::chromiumoxide::Page) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let PageSetup {
        url,
        identity,
        jar,
        viewport,
        init_script,
        timeout,
        doing,
    } = setup;
    let program = browser::find().ok_or_else(|| anyhow!(browser::NO_BROWSER))?;
    let proxy: Vec<String> = identity
        .proxies
        .first()
        .map(|proxy| format!("--proxy-server={proxy}"))
        .into_iter()
        .collect();
    // Kept until the end: dropping it kills the browser.
    let started = browser::Browser::start(&program, &proxy).await?;
    let (chrome, mut handler) = spider::chromiumoxide::Browser::connect(started.endpoint.clone())
        .await
        .context("connecting to the browser")?;
    let loaded = async {
        let page = chrome
            .new_page("about:blank")
            .await
            .context("opening a page")?;
        page.set_user_agent(identity.user_agent())
            .await
            .context("setting the user agent")?;
        // The session's cookies for the URL first, so the user's own
        // settings win, each set for the page's URL.
        let mut cookies = spider::features::chrome::cookie_params_from_jar(jar, url)
            .map_err(|error| anyhow!("reading the session's cookies: {error}"))?;
        cookies.extend(
            identity
                .cookies
                .iter()
                .flat_map(|cookies| cookies.split(';'))
                .filter_map(|pair| pair.split_once('='))
                .filter_map(|(name, value)| {
                    CookieParam::builder()
                        .name(name.trim())
                        .value(value.trim())
                        .url(url.as_str())
                        .path("/")
                        .build()
                        .ok()
                }),
        );
        // `Network.setCookies` itself: chromey's `set_cookies` refuses every
        // cookie while the page is still `about:blank`, even one that names
        // its URL.
        if !cookies.is_empty() {
            page.execute(SetCookiesParams::new(cookies))
                .await
                .context("setting the cookies")?;
        }
        // The browser sends the headers on the page's own requests, to
        // whichever host each goes; the cookies are the page's URL's.
        if !identity.headers.is_empty() {
            let headers: serde_json::Map<String, Value> = identity
                .headers
                .iter()
                .filter_map(|(name, value)| Some((name.to_string(), json!(value.to_str().ok()?))))
                .collect();
            page.set_extra_headers(SetExtraHttpHeadersParams::new(Headers::new(Value::Object(
                headers,
            ))))
            .await
            .context("setting the request headers")?;
        }
        if let Some((width, height)) = viewport {
            page.emulate_viewport(SetDeviceMetricsOverrideParams::new(
                i64::from(width),
                i64::from(height),
                1.0,
                /*mobile*/ false,
            ))
            .await
            .context("setting the viewport")?;
        }
        if let Some(script) = init_script {
            page.add_script_to_evaluate_on_new_document(Some(script.to_string()))
                .await
                .context("adding the init script")?;
        }
        page.goto(url.as_str())
            .await
            .with_context(|| format!("loading {url}"))?;
        page.wait_for_navigation()
            .await
            .with_context(|| format!("loading {url}"))?;
        let done = work(&page).await?;
        // What the page holds now, for the next load.
        for cookie in page
            .get_cookies()
            .await
            .context("reading the page's cookies")?
        {
            if let Some((line, url)) = spider::utils::browser_cookie_to_set_cookie(&cookie) {
                jar.add_cookie_str(&line, &url);
            }
        }
        anyhow::Ok(done)
    };
    let done = tokio::time::timeout(timeout, async {
        tokio::select! {
            done = loaded => done,
            () = async { while handler.next().await.is_some() {} } => {
                bail!("the browser closed before {doing} {url} was done")
            }
        }
    })
    .await
    .map_err(|_| anyhow!("{doing} {url} timed out after {}s", timeout.as_secs_f64()))??;
    drop(started);
    Ok(done)
}

/// Load `request.url` in a fresh headless browser with `identity` and the
/// cookies `jar` holds for it, and write what `request.kind` asks for to
/// `request.path`; the page's cookies then go back into `jar`. Replies
/// `{url, finalUrl, title, path, bytes}`.
///
/// # Errors
///
/// No browser, one that would not start, a page that would not load within
/// the deadline, or a file that could not be written.
#[tracing::instrument(skip_all, fields(host = request.url.host_str().unwrap_or_default()))]
pub(crate) async fn capture(
    request: &CaptureRequest,
    identity: &Identity,
    jar: &CookieJar,
) -> anyhow::Result<Value> {
    let setup = PageSetup {
        url: &request.url,
        identity,
        jar,
        viewport: request.viewport,
        init_script: request.init_script.as_deref(),
        timeout: request.timeout,
        doing: "capturing",
    };
    let (bytes, title, final_url) = with_page(setup, async |page| {
        if let Some(wait) = request.wait {
            tokio::time::sleep(wait).await;
        }
        let bytes = match request.kind {
            CaptureKind::Screenshot { full_page } => page
                .screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(full_page)
                        .build(),
                )
                .await
                .context("taking the screenshot")?,
            CaptureKind::Pdf {
                landscape,
                background,
            } => page
                .pdf(PrintToPdfParams {
                    landscape: Some(landscape),
                    print_background: Some(background),
                    ..PrintToPdfParams::default()
                })
                .await
                .context("printing to PDF")?,
        };
        let title = page.get_title().await.ok().flatten();
        let final_url = page.url().await.ok().flatten();
        anyhow::Ok((bytes, title, final_url))
    })
    .await?;
    pa_types::session_scope::ensure_live()?;
    if let Some(parent) = request.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&request.path, &bytes)
        .with_context(|| format!("writing {}", request.path.display()))?;
    Ok(json!({
        "url": request.url.as_str(),
        "finalUrl": final_url.unwrap_or_else(|| request.url.to_string()),
        "title": title,
        "path": request.path.display().to_string(),
        "bytes": bytes.len(),
    }))
}
