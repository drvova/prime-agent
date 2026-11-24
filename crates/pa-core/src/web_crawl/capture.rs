//! `web.capture`: one page loaded in a local headless Chrome and saved as a
//! PNG screenshot or a PDF. The kernel's `(web-screenshot url path ...)` and
//! `(web-pdf url path ...)` reach it.
//!
//! The browser is started as a rendered crawl's is ([`super::browser`]):
//! its own process group, in the orphan journal, killed when the capture
//! ends for any reason (a stopped cell drops the request's future, which
//! drops the browser). The page sees the user's `web.crawl` identity: its
//! user agent, headers, cookies (for the page's URL) and first proxy; never
//! the Basic credentials, which a browser would send to every host. A relative `path` is
//! taken under the session's working directory.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use futures::StreamExt;
use serde_json::{json, Value};
use spider::chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use spider::chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, Headers, SetExtraHttpHeadersParams,
};
use spider::chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, PrintToPdfParams,
};
use spider::chromiumoxide::page::ScreenshotParams;
use url::Url;

use super::browser;
use super::identity::Identity;

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
    })
}

/// Load `request.url` in a fresh headless browser with `identity`, and
/// write what `request.kind` asks for to `request.path`. Replies `{url,
/// finalUrl, title, path, bytes}`.
///
/// # Errors
///
/// No browser, one that would not start, a page that would not load within
/// the deadline, or a file that could not be written.
#[tracing::instrument(skip_all, fields(host = request.url.host_str().unwrap_or_default()))]
pub(crate) async fn capture(
    request: &CaptureRequest,
    identity: &Identity,
) -> anyhow::Result<Value> {
    let program = browser::find().ok_or_else(|| anyhow!(browser::NO_BROWSER))?;
    let proxy: Vec<String> = identity
        .proxies
        .first()
        .map(|proxy| format!("--proxy-server={proxy}"))
        .into_iter()
        .collect();
    // Kept until the capture's end: dropping it kills the browser.
    let started = browser::Browser::start(&program, &proxy).await?;
    let (chrome, mut handler) = spider::chromiumoxide::Browser::connect(started.endpoint.clone())
        .await
        .context("connecting to the browser")?;
    let work = async {
        let page = chrome
            .new_page("about:blank")
            .await
            .context("opening a page")?;
        page.set_user_agent(identity.user_agent())
            .await
            .context("setting the user agent")?;
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
        if let Some(cookies) = &identity.cookies {
            let cookies = cookies
                .split(';')
                .filter_map(|pair| pair.split_once('='))
                .filter_map(|(name, value)| {
                    CookieParam::builder()
                        .name(name.trim())
                        .value(value.trim())
                        .url(request.url.as_str())
                        .build()
                        .ok()
                })
                .collect();
            page.set_cookies(cookies)
                .await
                .context("setting the cookies")?;
        }
        if let Some((width, height)) = request.viewport {
            page.emulate_viewport(SetDeviceMetricsOverrideParams::new(
                i64::from(width),
                i64::from(height),
                1.0,
                /*mobile*/ false,
            ))
            .await
            .context("setting the viewport")?;
        }
        if let Some(script) = &request.init_script {
            page.add_script_to_evaluate_on_new_document(Some(script.clone()))
                .await
                .context("adding the init script")?;
        }
        page.goto(request.url.as_str())
            .await
            .with_context(|| format!("loading {}", request.url))?;
        page.wait_for_navigation()
            .await
            .with_context(|| format!("loading {}", request.url))?;
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
    };
    let (bytes, title, final_url) = tokio::time::timeout(request.timeout, async {
        tokio::select! {
            done = work => done,
            () = async { while handler.next().await.is_some() {} } => {
                bail!("the browser closed before the capture was made")
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow!(
            "capturing {} timed out after {}s",
            request.url,
            request.timeout.as_secs_f64()
        )
    })??;
    drop(started);
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
