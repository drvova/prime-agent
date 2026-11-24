//! `web.read`: one URL to structured markdown the model can cite, follow and
//! quote. The kernel's `rlm.web.read(...)` reaches it through the `web.read`
//! host request, beside `web.search`, whose sources it reads directly.
//!
//! The pipeline (after oh-my-pi's URL read, MIT): a site route first —
//! arXiv papers to their HTML rendering, else their PDF with the paper's
//! metadata; GitHub blobs to the raw file, repositories to the README,
//! issues and pull requests to the REST API; Stack Exchange questions to the
//! API's question and voted answers — falling back to a generic fetch whose
//! content type picks the renderer (readable HTML, PDF pages, markdown,
//! JSON, plain text). Every result carries the outline of headings with
//! their anchors and the links found; `section` (or the URL's own fragment)
//! narrows it to one heading's span, or a code view's `L10-L20` lines.
//!
//! The network work runs on the host, off any paint path: bounded bodies,
//! redirects followed by hand, one overall deadline. Nothing here runs at
//! startup: the client is built on the first read.

mod fetch;
mod host;
mod html;
mod markdown;
mod pdf;
mod sites;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

pub(crate) use fetch::{Accept, Fetched, Fetcher, HttpStatusError, RequestIdentity};
pub(crate) use host::{ReadObserver, WebReadHost};

use markdown::{AnchorStyle, Heading};

/// Default whole-read deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on a caller-supplied deadline.
const MAX_TIMEOUT: Duration = Duration::from_mins(2);
/// Default characters of markdown returned.
const DEFAULT_MAX_CHARS: usize = 20_000;
/// Upper bound on a caller-supplied `max_chars`.
pub(crate) const MAX_MAX_CHARS: usize = 200_000;
/// Links and outline entries returned at most.
const MAX_LINKS: usize = 200;
const MAX_OUTLINE: usize = 200;

/// Which renderer produced the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadMethod {
    Html,
    Pdf,
    Markdown,
    Json,
    Text,
    ArxivHtml,
    ArxivPdf,
    GithubFile,
    GithubReadme,
    GithubIssue,
    StackExchange,
}

impl ReadMethod {
    pub(crate) fn id(self) -> &'static str {
        match self {
            ReadMethod::Html => "html",
            ReadMethod::Pdf => "pdf",
            ReadMethod::Markdown => "markdown",
            ReadMethod::Json => "json",
            ReadMethod::Text => "text",
            ReadMethod::ArxivHtml => "arxiv-html",
            ReadMethod::ArxivPdf => "arxiv-pdf",
            ReadMethod::GithubFile => "github-file",
            ReadMethod::GithubReadme => "github-readme",
            ReadMethod::GithubIssue => "github-issue",
            ReadMethod::StackExchange => "stackexchange",
        }
    }

    /// The telemetry bucket: a site route, a PDF, or a generic page.
    pub(crate) fn kind(self) -> ReadKind {
        match self {
            ReadMethod::ArxivHtml
            | ReadMethod::ArxivPdf
            | ReadMethod::GithubFile
            | ReadMethod::GithubReadme
            | ReadMethod::GithubIssue
            | ReadMethod::StackExchange => ReadKind::Site,
            ReadMethod::Pdf => ReadKind::Pdf,
            ReadMethod::Html | ReadMethod::Markdown | ReadMethod::Json | ReadMethod::Text => {
                ReadKind::Page
            }
        }
    }
}

/// [`ReadMethod::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadKind {
    Site,
    Pdf,
    Page,
}

/// What adoption telemetry learns of one read: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    Read { kind: ReadKind, truncated: bool },
    Failed,
}

impl ReadOutcome {
    /// The outcome of `result` and how long it took.
    pub(crate) fn of(result: &Result<WebPage, ReadError>) -> (Self, Duration) {
        match result {
            Ok(page) => (
                ReadOutcome::Read {
                    kind: page.method.kind(),
                    truncated: page.truncated,
                },
                page.elapsed,
            ),
            Err(error) => (ReadOutcome::Failed, error.elapsed),
        }
    }
}

/// A link found on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PageLink {
    pub(crate) text: String,
    pub(crate) url: String,
}

/// What a renderer produced, before sectioning and truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Body {
    /// Markdown, and where its heading anchors come from.
    Markdown { text: String, anchors: AnchorStyle },
    /// A source file or plain text, shown fenced; `L10-L20` selects lines.
    Code { language: String, text: String },
}

/// A rendered document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) final_url: Url,
    pub(crate) title: Option<String>,
    pub(crate) content_type: String,
    pub(crate) method: ReadMethod,
    pub(crate) body: Body,
    pub(crate) links: Vec<PageLink>,
    pub(crate) notes: Vec<String>,
}

/// One read to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadRequest {
    pub(crate) url: Url,
    pub(crate) max_chars: usize,
    pub(crate) section: Option<String>,
    pub(crate) timeout: Duration,
}

/// A finished read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebPage {
    pub(crate) url: String,
    pub(crate) final_url: String,
    pub(crate) title: Option<String>,
    pub(crate) content_type: String,
    pub(crate) method: ReadMethod,
    /// The returned markdown: the section asked for, truncated to `max_chars`.
    pub(crate) markdown: String,
    /// The whole document's headings (not just the section's).
    pub(crate) outline: Vec<Heading>,
    pub(crate) links: Vec<PageLink>,
    /// The section shown (an anchor, or an `L10-L20` line range).
    pub(crate) section: Option<String>,
    pub(crate) truncated: bool,
    /// Characters of the selected span before truncation.
    pub(crate) total_chars: usize,
    pub(crate) notes: Vec<String>,
    pub(crate) elapsed: Duration,
}

/// A read that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadError {
    pub(crate) message: String,
    pub(crate) elapsed: Duration,
}

/// The reader: a lazily built HTTP client shared by every session.
pub(crate) struct WebReadEngine {
    client: OnceLock<reqwest::Client>,
    /// Tests route `https://host/path` to `{base}/host/path`.
    base_override: Option<String>,
}

impl WebReadEngine {
    /// The process-wide engine every session's `web.read` handler shares.
    pub(crate) fn shared() -> Arc<WebReadEngine> {
        static SHARED: OnceLock<Arc<WebReadEngine>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(WebReadEngine {
                client: OnceLock::new(),
                base_override: None,
            })
        }))
    }

    /// An engine whose every request answers from `base` (a loopback test
    /// server), bypassing any proxy.
    #[cfg(test)]
    pub(crate) fn for_test_server(base: &str) -> Self {
        WebReadEngine {
            base_override: Some(base.to_string()),
            ..Self::direct()
        }
    }

    /// An engine that reads URLs as given, bypassing any proxy (tests read
    /// loopback sites with it).
    #[cfg(test)]
    pub(crate) fn direct() -> Self {
        let engine = WebReadEngine {
            client: OnceLock::new(),
            base_override: None,
        };
        let _ = engine.client.set(
            pa_ai::utils::tls::http_client_builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("test client"),
        );
        engine
    }

    /// The shared client, built on first use (`web-crawl` fetches with it too).
    pub(crate) fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            pa_ai::utils::tls::http_client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default()
        })
    }

    /// Read one URL within its deadline: the site route, else the generic
    /// fetch, then the section and the truncation.
    ///
    /// # Errors
    ///
    /// [`ReadError`]: the fetch failed, the content type has no renderer,
    /// the deadline passed, or the named section does not exist.
    #[tracing::instrument(skip_all, fields(host = request.url.host_str().unwrap_or_default()))]
    pub(crate) async fn read(&self, request: &ReadRequest) -> Result<WebPage, ReadError> {
        let started = Instant::now();
        let fetcher = Fetcher {
            client: self.client(),
            base_override: self.base_override.as_deref(),
            timeout: request.timeout,
            identity: None,
        };
        let document =
            match tokio::time::timeout(request.timeout, document(&fetcher, &request.url)).await {
                Ok(Ok(document)) => document,
                Ok(Err(error)) => {
                    return Err(ReadError {
                        message: format!("{error:#}"),
                        elapsed: started.elapsed(),
                    })
                }
                Err(_) => {
                    return Err(ReadError {
                        message: format!(
                            "reading {} timed out after {}s",
                            request.url,
                            request.timeout.as_secs_f64()
                        ),
                        elapsed: started.elapsed(),
                    })
                }
            };
        finish(request, document, started.elapsed()).map_err(|error| ReadError {
            message: format!("{error:#}"),
            elapsed: started.elapsed(),
        })
    }
}

/// The site route when one matches and succeeds, else the generic read.
async fn document(fetcher: &Fetcher<'_>, url: &Url) -> anyhow::Result<Document> {
    let mut notes = Vec::new();
    if let Some(route) = sites::route(url) {
        match sites::read(fetcher, &route).await {
            Ok(document) => return Ok(document),
            Err(error) => notes.push(format!(
                "the {} read failed ({error:#}); this is the page itself",
                route.label()
            )),
        }
    }
    let reply = match fetcher.get(url, Accept::Document).await {
        Ok(reply) => reply,
        Err(error) if notes.is_empty() => return Err(error),
        // Both failed: the route's failure says more than the page's.
        Err(error) => return Err(error.context(notes.join("; "))),
    };
    let mut document = render(reply).await?;
    notes.append(&mut document.notes);
    document.notes = notes;
    Ok(document)
}

/// What a fetched body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sniffed {
    Pdf,
    Html,
    Markdown,
    Json,
    Text,
    Binary,
}

/// What a fetched body is, from its media type, its URL, and a sniff.
fn sniff(fetched: &Fetched) -> Sniffed {
    let media = fetched.media_type.as_deref().unwrap_or_default();
    let path = fetched.final_url.path();
    let head = &fetched.body[..fetched.body.len().min(512)];
    let head_text = String::from_utf8_lossy(head)
        .trim_start()
        .to_ascii_lowercase();
    if media == "application/pdf" || head.starts_with(b"%PDF-") {
        Sniffed::Pdf
    } else if matches!(media, "text/html" | "application/xhtml+xml")
        || head_text.starts_with("<!doctype html")
        || head_text.starts_with("<html")
    {
        Sniffed::Html
    } else if media == "text/markdown"
        || std::path::Path::new(path)
            .extension()
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
            })
    {
        Sniffed::Markdown
    } else if media == "application/json" || media.ends_with("+json") {
        Sniffed::Json
    } else if media.starts_with("text/")
        || media.ends_with("+xml")
        || media == "application/xml"
        || media.is_empty() && std::str::from_utf8(&fetched.body).is_ok()
    {
        Sniffed::Text
    } else {
        Sniffed::Binary
    }
}

/// Render a fetched body by its content type (CPU-bound work runs on a
/// blocking thread).
pub(crate) async fn render(fetched: Fetched) -> anyhow::Result<Document> {
    let kind = sniff(&fetched);
    let content_type = fetched
        .media_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let final_url = fetched.final_url.clone();
    match kind {
        Sniffed::Pdf => {
            let bytes = fetched.body;
            let text = pdf::extract(bytes).await?;
            let mut notes = Vec::new();
            if text.empty_pages > 0 {
                notes.push(format!(
                    "{} of {} pages had no extractable text",
                    text.empty_pages, text.pages
                ));
            }
            Ok(Document {
                title: final_url
                    .path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                final_url,
                content_type,
                method: ReadMethod::Pdf,
                body: Body::Markdown {
                    text: text.markdown,
                    anchors: AnchorStyle::Explicit,
                },
                links: Vec::new(),
                notes,
            })
        }
        Sniffed::Html => {
            let page_url = final_url.clone();
            let rendered = tokio::task::spawn_blocking(move || {
                html::render_page(&String::from_utf8_lossy(&fetched.body), &page_url)
            })
            .await
            .context("HTML rendering")?;
            Ok(Document {
                final_url,
                title: rendered.title,
                content_type,
                method: ReadMethod::Html,
                body: Body::Markdown {
                    text: rendered.markdown,
                    anchors: AnchorStyle::Explicit,
                },
                links: rendered.links,
                notes: Vec::new(),
            })
        }
        Sniffed::Markdown => {
            let (text, links) =
                markdown::resolve_links(&String::from_utf8_lossy(&fetched.body), &final_url);
            Ok(Document {
                final_url,
                title: None,
                content_type,
                method: ReadMethod::Markdown,
                body: Body::Markdown {
                    text,
                    anchors: AnchorStyle::GithubSlugs,
                },
                links,
                notes: Vec::new(),
            })
        }
        Sniffed::Json => {
            let raw = String::from_utf8_lossy(&fetched.body).into_owned();
            let text = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .unwrap_or(raw);
            Ok(Document {
                final_url,
                title: None,
                content_type,
                method: ReadMethod::Json,
                body: Body::Code {
                    language: "json".to_string(),
                    text,
                },
                links: Vec::new(),
                notes: Vec::new(),
            })
        }
        Sniffed::Text => Ok(Document {
            final_url,
            title: None,
            content_type,
            method: ReadMethod::Text,
            body: Body::Code {
                language: "text".to_string(),
                text: String::from_utf8_lossy(&fetched.body).into_owned(),
            },
            links: Vec::new(),
            notes: Vec::new(),
        }),
        Sniffed::Binary => bail!(
            "{final_url} is {content_type} ({} bytes), which web.read cannot render",
            fetched.body.len()
        ),
    }
}

/// Apply the section (or the URL's own fragment) and the truncation.
pub(crate) fn finish(
    request: &ReadRequest,
    document: Document,
    elapsed: Duration,
) -> anyhow::Result<WebPage> {
    let mut notes = document.notes;
    let fragment = request
        .url
        .fragment()
        .filter(|fragment| !fragment.is_empty())
        .map(str::to_string);
    let wanted = request.section.clone().or(fragment);
    let explicit = request.section.is_some();
    let (full, outline, selected, section) = match document.body {
        Body::Markdown { text, anchors } => {
            let outline = markdown::outline(&text, anchors, &document.final_url);
            let selected = match wanted.as_deref() {
                None => None,
                Some(wanted) => match markdown::select_section(&text, &outline, wanted) {
                    Some((span, heading)) => Some((span, heading.anchor.clone())),
                    None if explicit => {
                        let known: Vec<&str> = outline
                            .iter()
                            .take(30)
                            .map(|heading| heading.anchor.as_str())
                            .collect();
                        bail!(
                            "no section \"{wanted}\" in {}; its anchors: {}",
                            document.final_url,
                            if known.is_empty() {
                                "(none)".to_string()
                            } else {
                                known.join(", ")
                            }
                        );
                    }
                    None => {
                        notes.push(format!(
                            "the page has no #{wanted} anchor; showing the whole page"
                        ));
                        None
                    }
                },
            };
            let (body, section) =
                selected.map_or((None, None), |(span, anchor)| (Some(span), Some(anchor)));
            (text, outline, body, section)
        }
        Body::Code { language, text } => {
            let lines: Vec<&str> = text.lines().collect();
            let range = wanted.as_deref().and_then(markdown::line_range);
            if explicit && range.is_none() {
                bail!(
                    "{} is a code view; its section takes a line range such as L10-L20",
                    document.final_url
                );
            }
            let (start, end) = range.unwrap_or((1, lines.len().max(1)));
            if start > lines.len().max(1) {
                bail!(
                    "{} has {} lines; L{start} is past the end",
                    document.final_url,
                    lines.len()
                );
            }
            let end = end.min(lines.len());
            let shown = lines[start - 1..end].join("\n");
            let fence = html::fence_for(&shown);
            let heading = if range.is_some() {
                format!("Lines {start}-{end} of {}\n\n", lines.len())
            } else {
                String::new()
            };
            let fenced = format!("{heading}{fence}{language}\n{shown}\n{fence}");
            let section = range.map(|_| format!("L{start}-L{end}"));
            (fenced, Vec::new(), None, section)
        }
    };
    let body = selected.unwrap_or(full);
    let total_chars = body.chars().count();
    let (text, truncated) = markdown::truncate(&body, request.max_chars);
    if truncated {
        notes.push(format!(
            "truncated to {} of {total_chars} chars; pass section= one of the outline's anchors, or a larger max_chars",
            request.max_chars
        ));
    }
    let mut links = document.links;
    links.truncate(MAX_LINKS);
    let mut outline = outline;
    outline.truncate(MAX_OUTLINE);
    Ok(WebPage {
        url: request.url.to_string(),
        final_url: document.final_url.to_string(),
        title: document.title,
        content_type: document.content_type,
        method: document.method,
        markdown: text,
        outline,
        links,
        section,
        truncated,
        total_chars,
        notes,
        elapsed,
    })
}

impl WebPage {
    /// The model-readable rendering: the title, the source line, notes, then
    /// the markdown.
    pub(crate) fn text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(title) = &self.title {
            if !self.markdown.starts_with("# ") {
                parts.push(format!("# {title}"));
            }
        }
        let section = self
            .section
            .as_ref()
            .map(|section| format!(", section {section}"))
            .unwrap_or_default();
        parts.push(format!(
            "Source: {} ({}{section})",
            self.final_url,
            self.method.id()
        ));
        parts.extend(self.notes.iter().map(|note| format!("Note: {note}")));
        parts.push(String::new());
        parts.push(self.markdown.clone());
        parts.join("\n")
    }

    /// The host-request reply: the structured page plus its rendering.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "finalUrl": self.final_url,
            "title": self.title,
            "contentType": self.content_type,
            "method": self.method.id(),
            "markdown": self.markdown,
            "outline": self.outline,
            "links": self.links,
            "section": self.section,
            "truncated": self.truncated,
            "totalChars": self.total_chars,
            "notes": self.notes,
            "text": self.text(),
        })
    }
}

/// Parse the `web.read` payload into a request.
///
/// # Errors
///
/// A missing or non-http(s) URL, or an out-of-range option.
pub(crate) fn parse_request(data: &Value) -> anyhow::Result<ReadRequest> {
    let raw = data
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| anyhow!("web.read requires a url"))?;
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let url = Url::parse(&with_scheme)
        .map_err(|error| anyhow!("web.read: invalid url {raw:?}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web.read reads http(s) URLs; got {}", url.scheme());
    }
    let max_chars = match data.get("maxChars") {
        None | Some(Value::Null) => DEFAULT_MAX_CHARS,
        Some(value) => value
            .as_u64()
            .filter(|count| *count >= 1)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| anyhow!("web.read maxChars must be a positive integer"))?
            .min(MAX_MAX_CHARS),
    };
    let section = data
        .get("section")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(str::to_string);
    let timeout = match data.get("timeoutSeconds").and_then(Value::as_f64) {
        None => DEFAULT_TIMEOUT,
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Duration::from_secs_f64(seconds).min(MAX_TIMEOUT)
        }
        Some(_) => bail!("web.read timeoutSeconds must be positive"),
    };
    Ok(ReadRequest {
        url,
        max_chars,
        section,
        timeout,
    })
}
