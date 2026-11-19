//! Site routes for `web.read` (after oh-my-pi's special handlers, MIT):
//! sources whose page is a poor read but whose API, raw file, or alternate
//! rendering is a good one. A route that fails falls back to the page itself.

use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context};
use serde_json::Value;
use url::Url;

use super::fetch::{failed_status, Accept, Fetcher};
use super::html::{self, Element};
use super::markdown::{demote_headings, resolve_links, AnchorStyle};
use super::{pdf, Body, Document, PageLink, ReadMethod};
use crate::web_search::dates::epoch_seconds_to_iso;

/// Stack Exchange sites outside `*.stackexchange.com`, with their API names.
const STANDALONE_STACK_SITES: &[(&str, &str)] = &[
    ("stackoverflow.com", "stackoverflow"),
    ("superuser.com", "superuser"),
    ("serverfault.com", "serverfault"),
    ("askubuntu.com", "askubuntu"),
    ("mathoverflow.net", "mathoverflow"),
    ("stackapps.com", "stackapps"),
];
/// Answers a Stack Exchange read shows, by votes.
const STACK_ANSWERS: usize = 8;

/// A URL a site route reads better than its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// An arXiv paper: its HTML rendering when it has one (`abs`/`html`),
    /// else (or for a `pdf` link) the PDF.
    Arxiv { id: String, wants_pdf: bool },
    /// A file on GitHub (`blob/<ref>/<path>`): the raw file.
    GithubFile {
        owner: String,
        repo: String,
        ref_and_path: String,
    },
    /// A repository's front page: its README.
    GithubRepo { owner: String, repo: String },
    /// An issue or pull request: the REST API's thread.
    GithubIssue {
        owner: String,
        repo: String,
        number: u64,
    },
    /// A Stack Exchange question: the API's question and voted answers.
    StackExchange { site: String, question: u64 },
}

impl Route {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Route::Arxiv { .. } => "arXiv",
            Route::GithubFile { .. } => "GitHub file",
            Route::GithubRepo { .. } => "GitHub README",
            Route::GithubIssue { .. } => "GitHub issue",
            Route::StackExchange { .. } => "Stack Exchange",
        }
    }
}

/// The route for `url`, when a site has one.
pub(crate) fn route(url: &Url) -> Option<Route> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match host {
        "arxiv.org" | "export.arxiv.org" => match segments.as_slice() {
            [kind @ ("abs" | "pdf" | "html"), id @ ..] if !id.is_empty() => {
                let id = id.join("/");
                let id = id.strip_suffix(".pdf").unwrap_or(&id).to_string();
                Some(Route::Arxiv {
                    id,
                    wants_pdf: *kind == "pdf",
                })
            }
            _ => None,
        },
        "github.com" => match segments.as_slice() {
            [owner, repo] => Some(Route::GithubRepo {
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
            }),
            [owner, repo, "blob" | "raw", rest @ ..] if rest.len() >= 2 => {
                Some(Route::GithubFile {
                    owner: (*owner).to_string(),
                    repo: (*repo).to_string(),
                    ref_and_path: rest.join("/"),
                })
            }
            [owner, repo, "issues" | "pull", number, ..] => Some(Route::GithubIssue {
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
                number: number.parse().ok()?,
            }),
            _ => None,
        },
        _ => {
            let site = STANDALONE_STACK_SITES
                .iter()
                .find(|(known, _)| *known == host)
                .map(|(_, site)| (*site).to_string())
                .or_else(|| {
                    host.strip_suffix(".stackexchange.com")
                        .filter(|site| !site.is_empty() && !site.contains('.'))
                        .map(str::to_string)
                })?;
            match segments.as_slice() {
                ["questions" | "q", id, ..] => Some(Route::StackExchange {
                    site,
                    question: id.parse().ok()?,
                }),
                _ => None,
            }
        }
    }
}

/// Read `route`.
///
/// # Errors
///
/// The route's fetches failed; the caller falls back to the page itself.
pub(crate) async fn read(fetcher: &Fetcher<'_>, route: &Route) -> anyhow::Result<Document> {
    match route {
        Route::Arxiv { id, wants_pdf } => arxiv(fetcher, id, *wants_pdf).await,
        Route::GithubFile {
            owner,
            repo,
            ref_and_path,
        } => github_file(fetcher, owner, repo, ref_and_path).await,
        Route::GithubRepo { owner, repo } => github_readme(fetcher, owner, repo).await,
        Route::GithubIssue {
            owner,
            repo,
            number,
        } => github_issue(fetcher, owner, repo, *number).await,
        Route::StackExchange { site, question } => stack_exchange(fetcher, site, *question).await,
    }
}

fn parse_url(raw: &str) -> anyhow::Result<Url> {
    Url::parse(raw).map_err(|error| anyhow!("invalid URL {raw}: {error}"))
}

async fn arxiv(fetcher: &Fetcher<'_>, id: &str, wants_pdf: bool) -> anyhow::Result<Document> {
    let mut notes = Vec::new();
    if !wants_pdf {
        let html_url = parse_url(&format!("https://arxiv.org/html/{id}"))?;
        match fetcher.get(&html_url, Accept::Document).await {
            Ok(reply) => {
                let page_url = reply.final_url.clone();
                let rendered = tokio::task::spawn_blocking(move || {
                    html::render_page(&String::from_utf8_lossy(&reply.body), &page_url)
                })
                .await
                .context("HTML rendering")?;
                let mut final_url = html_url;
                final_url.set_fragment(None);
                return Ok(Document {
                    final_url,
                    title: rendered.title,
                    content_type: "text/html".to_string(),
                    method: ReadMethod::ArxivHtml,
                    body: Body::Markdown {
                        text: rendered.markdown,
                        anchors: AnchorStyle::Explicit,
                    },
                    links: rendered.links,
                    notes,
                });
            }
            Err(error) if failed_status(&error) == Some(404) => {
                notes.push(format!(
                    "arXiv has no HTML rendering of {id}; this is the PDF"
                ));
            }
            Err(error) => notes.push(format!(
                "the arXiv HTML rendering failed ({error:#}); this is the PDF"
            )),
        }
    }
    let pdf_url = parse_url(&format!("https://arxiv.org/pdf/{id}"))?;
    let reply = fetcher.get(&pdf_url, Accept::Pdf).await?;
    let final_url = reply.final_url.clone();
    let bytes = reply.body;
    let text = pdf::extract(bytes).await?;
    // The paper's title, authors and abstract head the pages; a metadata
    // failure only loses the header.
    let meta_url = parse_url(&format!("https://export.arxiv.org/api/query?id_list={id}"))?;
    let (title, header) = match fetcher.get(&meta_url, Accept::Document).await {
        Ok(feed) => arxiv_header(&String::from_utf8_lossy(&feed.body), id),
        Err(error) => {
            notes.push(format!("the arXiv metadata failed ({error:#})"));
            (None, String::new())
        }
    };
    if text.empty_pages > 0 {
        notes.push(format!(
            "{} of {} pages had no extractable text",
            text.empty_pages, text.pages
        ));
    }
    Ok(Document {
        final_url,
        title: title.or_else(|| Some(format!("arXiv:{id}"))),
        content_type: "application/pdf".to_string(),
        method: ReadMethod::ArxivPdf,
        body: Body::Markdown {
            text: format!("{header}{}", text.markdown),
            anchors: AnchorStyle::Explicit,
        },
        links: vec![PageLink {
            text: "arXiv abstract page".to_string(),
            url: format!("https://arxiv.org/abs/{id}"),
        }],
        notes,
    })
}

/// The Atom entry's title and a markdown header (authors, date, abstract).
pub(crate) fn arxiv_header(feed: &str, id: &str) -> (Option<String>, String) {
    let tree = html::parse(feed);
    let Some(entry) = tree.find(&|element| element.name == "entry") else {
        return (None, String::new());
    };
    let field = |name: &str| {
        entry
            .find(&|element| element.name == name)
            .map(Element::raw_text)
            .and_then(|text| crate::web_search::http::normalize_text(&text))
    };
    let Some(title) = field("title") else {
        return (None, String::new());
    };
    let mut authors = Vec::new();
    entry.find_all("author", &mut authors);
    let authors: Vec<String> = authors
        .iter()
        .filter_map(|author| {
            author
                .find(&|element| element.name == "name")
                .map(Element::raw_text)
                .and_then(|name| crate::web_search::http::normalize_text(&name))
        })
        .collect();
    let mut header = format!("# {title}\n\n");
    if !authors.is_empty() {
        let _ = writeln!(header, "**Authors:** {}", authors.join(", "));
    }
    if let Some(published) = field("published") {
        let _ = writeln!(
            header,
            "**Published:** {}",
            published.get(..10).unwrap_or(&published)
        );
    }
    let _ = write!(header, "**arXiv:** {id}\n\n");
    if let Some(summary) = field("summary") {
        let _ = write!(header, "## Abstract {{#abstract}}\n\n{summary}\n\n");
    }
    (Some(title), header)
}

async fn github_file(
    fetcher: &Fetcher<'_>,
    owner: &str,
    repo: &str,
    ref_and_path: &str,
) -> anyhow::Result<Document> {
    let raw = parse_url(&format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/{ref_and_path}"
    ))?;
    let reply = fetcher.get(&raw, Accept::Document).await?;
    if reply.body.contains(&0) {
        bail!("{owner}/{repo}/{ref_and_path} is a binary file");
    }
    let text = String::from_utf8_lossy(&reply.body).into_owned();
    let path = ref_and_path.rsplit('/').next().unwrap_or(ref_and_path);
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    let final_url = parse_url(&format!(
        "https://github.com/{owner}/{repo}/blob/{ref_and_path}"
    ))?;
    let mut links = vec![PageLink {
        text: "raw file".to_string(),
        url: raw.to_string(),
    }];
    let body = match extension.as_deref() {
        Some("md" | "markdown" | "mdx") => {
            let (text, mut found) = resolve_links(&text, &final_url);
            links.append(&mut found);
            Body::Markdown {
                text,
                anchors: AnchorStyle::GithubSlugs,
            }
        }
        other => Body::Code {
            language: other.map_or_else(|| "text".to_string(), code_language),
            text,
        },
    };
    Ok(Document {
        final_url,
        title: Some(format!("{owner}/{repo}: {ref_and_path}")),
        content_type: reply.media_type.unwrap_or_else(|| "text/plain".to_string()),
        method: ReadMethod::GithubFile,
        body,
        links,
        notes: Vec::new(),
    })
}

/// A fence language for a file extension.
fn code_language(extension: &str) -> String {
    match extension {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" => "powershell",
        "yml" | "yaml" => "yaml",
        "h" | "c" => "c",
        "hpp" | "cc" | "cpp" | "cxx" => "cpp",
        "rb" => "ruby",
        "kt" | "kts" => "kotlin",
        "cs" => "csharp",
        other => other,
    }
    .to_string()
}

async fn github_readme(fetcher: &Fetcher<'_>, owner: &str, repo: &str) -> anyhow::Result<Document> {
    let raw = parse_url(&format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/HEAD/README.md"
    ))?;
    let reply = fetcher.get(&raw, Accept::Document).await?;
    let base = parse_url(&format!("https://github.com/{owner}/{repo}/blob/HEAD/"))?;
    let (text, found) = resolve_links(&String::from_utf8_lossy(&reply.body), &base);
    let mut links = vec![PageLink {
        text: "README (raw)".to_string(),
        url: raw.to_string(),
    }];
    links.extend(found);
    Ok(Document {
        final_url: parse_url(&format!("https://github.com/{owner}/{repo}"))?,
        title: Some(format!("{owner}/{repo}")),
        content_type: "text/markdown".to_string(),
        method: ReadMethod::GithubReadme,
        body: Body::Markdown {
            text,
            anchors: AnchorStyle::GithubSlugs,
        },
        links,
        notes: Vec::new(),
    })
}

async fn fetch_json(fetcher: &Fetcher<'_>, url: &str) -> anyhow::Result<Value> {
    let reply = fetcher.get(&parse_url(url)?, Accept::Json).await?;
    serde_json::from_slice(&reply.body).with_context(|| format!("{url} returned invalid JSON"))
}

fn str_at<'v>(value: &'v Value, pointer: &str) -> &'v str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// `YYYY-MM-DD` of an ISO timestamp.
fn day(iso: &str) -> &str {
    iso.get(..10).unwrap_or(iso)
}

async fn github_issue(
    fetcher: &Fetcher<'_>,
    owner: &str,
    repo: &str,
    number: u64,
) -> anyhow::Result<Document> {
    let api = format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}");
    let issue = fetch_json(fetcher, &api).await?;
    let comments = fetch_json(fetcher, &format!("{api}/comments?per_page=100")).await?;
    let pull = issue.get("pull_request").is_some();
    let html_url = issue.get("html_url").and_then(Value::as_str).map_or_else(
        || format!("https://github.com/{owner}/{repo}/issues/{number}"),
        str::to_string,
    );
    let title = format!("{} (#{number})", str_at(&issue, "/title"));
    let labels: Vec<&str> = issue
        .get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let mut text = format!(
        "# {title}\n\n**{}** {} · opened by {} on {} · {} comments",
        str_at(&issue, "/state"),
        if pull { "pull request" } else { "issue" },
        str_at(&issue, "/user/login"),
        day(str_at(&issue, "/created_at")),
        issue
            .get("comments")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    );
    if !labels.is_empty() {
        let _ = write!(text, " · labels: {}", labels.join(", "));
    }
    text.push_str("\n\n");
    text.push_str(&demote_headings(str_at(&issue, "/body").trim(), 2));
    for comment in comments.as_array().map_or(&[][..], Vec::as_slice) {
        let _ = write!(
            text,
            "\n\n## Comment by {} on {} {{#issuecomment-{}}}\n\n{}",
            str_at(comment, "/user/login"),
            day(str_at(comment, "/created_at")),
            comment
                .get("id")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            demote_headings(str_at(comment, "/body").trim(), 2)
        );
    }
    Ok(Document {
        final_url: parse_url(&html_url)?,
        title: Some(title),
        content_type: "application/json".to_string(),
        method: ReadMethod::GithubIssue,
        body: Body::Markdown {
            text,
            anchors: AnchorStyle::Explicit,
        },
        links: Vec::new(),
        notes: Vec::new(),
    })
}

async fn stack_exchange(
    fetcher: &Fetcher<'_>,
    site: &str,
    question: u64,
) -> anyhow::Result<Document> {
    let api = format!("https://api.stackexchange.com/2.3/questions/{question}");
    let questions = fetch_json(fetcher, &format!("{api}?site={site}&filter=withbody")).await?;
    let item = questions
        .pointer("/items/0")
        .ok_or_else(|| anyhow!("Stack Exchange has no question {question} on {site}"))?;
    let answers = fetch_json(
        fetcher,
        &format!("{api}/answers?site={site}&filter=withbody&sort=votes&order=desc&pagesize={STACK_ANSWERS}"),
    )
    .await?;
    let page = parse_url(item.get("link").and_then(Value::as_str).unwrap_or_default())
        .or_else(|_| parse_url(&format!("https://{site}.com/questions/{question}")))?;
    let decode = crate::web_search::http::decode_entities;
    let title = decode(str_at(item, "/title"));
    let date = |value: &Value| {
        value
            .get("creation_date")
            .and_then(Value::as_i64)
            .map(epoch_seconds_to_iso)
            .unwrap_or_default()
    };
    let tags: Vec<String> = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(|tag| format!("`{tag}`"))
                .collect()
        })
        .unwrap_or_default();
    let (question_body, mut links) = html::render_fragment(str_at(item, "/body"), &page);
    let question_body = demote_headings(&question_body, 2);
    let mut text = format!(
        "# {title}\n\n**Score {}** · {} answers · asked by {} on {} · tags: {}\n\n## Question {{#question}}\n\n{question_body}",
        item.get("score").and_then(Value::as_i64).unwrap_or_default(),
        item.get("answer_count").and_then(Value::as_u64).unwrap_or_default(),
        decode(str_at(item, "/owner/display_name")),
        date(item),
        tags.join(", "),
    );
    for answer in answers
        .get("items")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let (body, mut answer_links) = html::render_fragment(str_at(answer, "/body"), &page);
        let body = demote_headings(&body, 2);
        let accepted = if answer.get("is_accepted").and_then(Value::as_bool) == Some(true) {
            " · accepted"
        } else {
            ""
        };
        let _ = write!(
            text,
            "\n\n## Answer by {} · score {}{accepted} · {} {{#answer-{}}}\n\n{body}",
            decode(str_at(answer, "/owner/display_name")),
            answer
                .get("score")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            date(answer),
            answer
                .get("answer_id")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        );
        for link in answer_links.drain(..) {
            if !links.iter().any(|known| known.url == link.url) {
                links.push(link);
            }
        }
    }
    Ok(Document {
        final_url: page,
        title: Some(title),
        content_type: "application/json".to_string(),
        method: ReadMethod::StackExchange,
        body: Body::Markdown {
            text,
            anchors: AnchorStyle::Explicit,
        },
        links,
        notes: Vec::new(),
    })
}
