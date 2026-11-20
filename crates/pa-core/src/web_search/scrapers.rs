//! Keyless HTML engines: `DuckDuckGo`'s no-JS frontend, Startpage, Mojeek, and
//! Ecosia, plus the Public Web aggregate that fans out to all four. Each
//! fetch carries a desktop browser fingerprint; a bot-challenge page is a
//! provider failure (429) so the chain moves on. The headless-browser
//! escalation some agents layer on top is not ported: these run as plain HTTP.

use std::collections::HashMap;

use futures::stream::{FuturesUnordered, StreamExt};

use super::catalog::ProviderId;
use super::http::{html_text, http_url, ProviderCall, ProviderError, ProviderResult};
use super::query::{format_scraper_query, QuerySyntax, GOOGLE_QUERY_SYNTAX};
use super::{Recency, SearchResponse, SearchSource};

const DUCKDUCKGO_URL: &str = "https://html.duckduckgo.com/html/";
const STARTPAGE_HOME_URL: &str = "https://www.startpage.com/";
const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";
const MOJEEK_URL: &str = "https://www.mojeek.com/search";
const ECOSIA_HOME_URL: &str = "https://www.ecosia.org/";
const ECOSIA_SEARCH_URL: &str = "https://www.ecosia.org/search";

/// One opening tag: its byte span and raw text (`<a class="x" href="...">`).
struct Tag<'h> {
    start: usize,
    end: usize,
    raw: &'h str,
}

impl Tag<'_> {
    /// An attribute value (double or single quoted), entities decoded.
    fn attr(&self, name: &str) -> Option<String> {
        let lowered = self.raw.to_ascii_lowercase();
        let mut from = 0;
        while let Some(found) = lowered[from..].find(name) {
            let at = from + found;
            from = at + name.len();
            let before_ok = lowered[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let rest = lowered[from..].trim_start();
            if !before_ok || !rest.starts_with('=') {
                continue;
            }
            let value_start = self.raw.len() - rest.len() + 1;
            let value = self.raw[value_start..].trim_start();
            let quote = value.chars().next()?;
            let body = if quote == '"' || quote == '\'' {
                let inner = &value[1..];
                &inner[..inner.find(quote)?]
            } else {
                value
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or_default()
            };
            return Some(super::http::decode_entities(body));
        }
        None
    }

    fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|classes| classes.split_whitespace().any(|token| token == class))
    }
}

/// Every `<name ...>` opening tag in `html`, in document order.
fn tags<'h>(html: &'h str, name: &str) -> Vec<Tag<'h>> {
    let lowered = html.to_ascii_lowercase();
    let needle = format!("<{name}");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = lowered[from..].find(&needle) {
        let start = from + offset;
        from = start + needle.len();
        let next = lowered[from..].chars().next();
        if !next.is_some_and(|c| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let Some(close) = lowered[start..].find('>') else {
            break;
        };
        let end = start + close + 1;
        found.push(Tag {
            start,
            end,
            raw: &html[start..end],
        });
        from = end;
    }
    found
}

/// The markup after `tag` up to the next `</name` (the element's inner HTML
/// when it does not nest itself).
fn inner<'h>(html: &'h str, tag: &Tag<'_>, name: &str) -> &'h str {
    let rest = &html[tag.end..];
    let close = rest
        .to_ascii_lowercase()
        .find(&format!("</{name}"))
        .unwrap_or(rest.len());
    &rest[..close]
}

/// `html` split at each `<name>` tag carrying `class`: one block per tag,
/// running to the next block (or the end).
fn blocks_by_class<'h>(html: &'h str, name: &str, class: &str) -> Vec<&'h str> {
    let starts: Vec<usize> = tags(html, name)
        .into_iter()
        .filter(|tag| tag.has_class(class))
        .map(|tag| tag.start)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, start)| &html[*start..starts.get(i + 1).copied().unwrap_or(html.len())])
        .collect()
}

/// First `<name>` tag in `block` carrying `class`, with its text.
fn classed_text(block: &str, names: &[&str], class: &str) -> Option<String> {
    names.iter().find_map(|name| {
        tags(block, name)
            .into_iter()
            .find(|tag| tag.has_class(class))
            .map(|tag| html_text(inner(block, &tag, name)))
            .filter(|text| !text.is_empty())
    })
}

/// Deduplicate by URL and cap at `count`.
fn dedupe(results: Vec<SearchSource>, count: usize) -> Vec<SearchSource> {
    let mut seen = std::collections::HashSet::new();
    results
        .into_iter()
        .filter(|source| seen.insert(source.url.clone()))
        .take(count)
        .collect()
}

/// An http(s) result URL resolved against `base`, rejecting the engine's own
/// hosts (internal navigation rows share the result markup).
fn external_url(href: &str, base: &str, own_hosts: &[&str]) -> Option<String> {
    let url = url::Url::parse(base).ok()?.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if own_hosts
        .iter()
        .any(|own| host == *own || host.ends_with(&format!(".{own}")))
    {
        return None;
    }
    Some(url.to_string())
}

fn challenge(label: &str, wall: &str) -> ProviderError {
    ProviderError::new(
        format!("{label} blocked the request with {wall}; it throttles automated searches from shared IPs. Retry later or configure a keyed provider via /mcp."),
        Some(429),
    )
}

/// The `DuckDuckGo` HTML frontend parses quotes, `-`, OR, `site:`, `filetype:`,
/// and the `in*:` operators; date bounds are left to the post-filter.
const DDG_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    date_range: false,
    ..GOOGLE_QUERY_SYNTAX
};

/// `DuckDuckGo` via `html.duckduckgo.com` (POST `q=...`).
pub(crate) async fn duckduckgo(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 20);
    let mut form = vec![
        ("q", format_scraper_query(call.query, DDG_QUERY_SYNTAX)),
        ("kl", "us-en".to_string()),
        ("b", String::new()),
    ];
    if let Some(recency) = call.recency {
        form.push(("df", recency.letter().to_string()));
    }
    let request = call
        .browser_request(
            reqwest::Method::POST,
            &call.endpoint(DUCKDUCKGO_URL),
            Some("https://html.duckduckgo.com/"),
        )
        .form(&form);
    let (status, html) = call.send_raw(request).await?;
    if !(200..300).contains(&status) {
        return Err(call.http_error(status, &html));
    }
    if html.contains("anomaly-modal") || html.contains("anomaly.js") {
        return Err(challenge("DuckDuckGo", "a bot-detection challenge"));
    }
    Ok(SearchResponse::from_sources(dedupe(
        parse_duckduckgo(&html),
        count,
    )))
}

pub(crate) fn parse_duckduckgo(html: &str) -> Vec<SearchSource> {
    blocks_by_class(html, "div", "result")
        .into_iter()
        .filter_map(|block| {
            let anchor = tags(block, "a")
                .into_iter()
                .find(|tag| tag.has_class("result__a"))?;
            // Resolve DDG's `//duckduckgo.com/l/?uddg=<target>` click
            // wrapper; ad redirects (`/y.js`) yield nothing.
            let href = anchor.attr("href")?;
            if href.to_ascii_lowercase().contains("duckduckgo.com/y.js") {
                return None;
            }
            let wrapped = href.split_once('?').and_then(|(_, query)| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "uddg")
                    .map(|(_, target)| target.into_owned())
            });
            let url = match (wrapped, href.strip_prefix("//")) {
                (Some(target), _) => http_url(&target),
                (None, Some(rest)) => http_url(&format!("https://{rest}")),
                (None, None) => http_url(&href),
            }?;
            let title = html_text(inner(block, &anchor, "a"));
            (!title.is_empty()).then(|| SearchSource {
                title,
                url,
                snippet: classed_text(block, &["a", "div", "span"], "result__snippet"),
                published_date: None,
                author: None,
            })
        })
        .collect()
}

/// Startpage (Google's index behind a privacy frontend): GET the homepage,
/// lift the search form's hidden inputs (the `sc` session token), and POST
/// them back with the query; without a token, a plain GET.
pub(crate) async fn startpage(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 20);
    let query = format_scraper_query(call.query, GOOGLE_QUERY_SYNTAX);
    let home = call.browser_request(
        reqwest::Method::GET,
        &call.endpoint(STARTPAGE_HOME_URL),
        None,
    );
    let form_inputs = match call.send_raw(home).await {
        Ok((status, html)) if (200..300).contains(&status) && !startpage_challenge(&html) => {
            // The `/sp/search` form's hidden inputs, when they carry `sc`.
            tags(&html, "form")
                .into_iter()
                .find(|tag| tag.attr("action").as_deref() == Some("/sp/search"))
                .map(|form| {
                    let body = inner(&html, &form, "form");
                    tags(body, "input")
                        .into_iter()
                        .filter(|input| input.attr("type").as_deref() == Some("hidden"))
                        .filter_map(|input| {
                            Some((input.attr("name")?, input.attr("value").unwrap_or_default()))
                        })
                        .collect::<Vec<(String, String)>>()
                })
                .filter(|inputs| {
                    inputs
                        .iter()
                        .any(|(name, value)| name == "sc" && !value.is_empty())
                })
        }
        Ok(_) | Err(_) => None,
    };
    let search_url = call.endpoint(STARTPAGE_SEARCH_URL);
    let mut fields = form_inputs.unwrap_or_default();
    let tokened = !fields.is_empty();
    fields.push(("query".to_string(), query));
    if let Some(recency) = call.recency {
        fields.push(("with_date".to_string(), recency.letter().to_string()));
    }
    let request = if tokened {
        call.browser_request(reqwest::Method::POST, &search_url, Some(STARTPAGE_HOME_URL))
            .form(&fields)
    } else {
        call.browser_request(reqwest::Method::GET, &search_url, Some(STARTPAGE_HOME_URL))
            .query(&fields)
    };
    let (status, html) = call.send_raw(request).await?;
    if startpage_challenge(&html) {
        return Err(challenge("Startpage", "a CAPTCHA challenge"));
    }
    if !(200..300).contains(&status) {
        return Err(call.http_error(status, &html));
    }
    Ok(SearchResponse::from_sources(dedupe(
        parse_startpage(&html),
        count,
    )))
}

fn startpage_challenge(html: &str) -> bool {
    html.contains("component---src-pages-captcha") || html.contains("/sp/captcha")
}

pub(crate) fn parse_startpage(html: &str) -> Vec<SearchSource> {
    blocks_by_class(html, "div", "result")
        .into_iter()
        .filter_map(|block| {
            let anchor = tags(block, "a")
                .into_iter()
                .find(|tag| tag.has_class("result-link"))?;
            let url = external_url(
                &anchor.attr("href")?,
                STARTPAGE_HOME_URL,
                &["startpage.com"],
            )?;
            let anchor_html = inner(block, &anchor, "a");
            let heading = ["h2", "h3"].iter().find_map(|name| {
                tags(anchor_html, name)
                    .first()
                    .map(|tag| html_text(inner(anchor_html, tag, name)))
            });
            let title = heading
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| html_text(anchor_html));
            (!title.is_empty()).then(|| SearchSource {
                title,
                url,
                snippet: classed_text(block, &["p"], "description"),
                published_date: None,
                author: None,
            })
        })
        .collect()
}

/// Mojeek's syntax: quoted phrases, `-` exclusions, and `site:`.
const MOJEEK_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: false,
    site: true,
    in_url: false,
    in_title: false,
    in_text: false,
    filetype: false,
    date_range: false,
};

/// Mojeek's independent index (server-rendered results page).
pub(crate) async fn mojeek(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 20);
    let mut params = vec![
        ("q", format_scraper_query(call.query, MOJEEK_QUERY_SYNTAX)),
        ("t", count.to_string()),
        ("arc", "none".to_string()),
        ("lang", "en".to_string()),
        ("lb", "en".to_string()),
    ];
    if let Some(recency) = call.recency {
        params.push(("since", recency.id().to_string()));
    }
    let request = call
        .browser_request(
            reqwest::Method::GET,
            &call.endpoint(MOJEEK_URL),
            Some("https://www.mojeek.com/"),
        )
        .query(&params);
    let (status, html) = call.send_raw(request).await?;
    let robot_wall = (html.contains("altcha-widget")
        || html.contains("captcha-wrap")
        || html
            .to_ascii_lowercase()
            .contains("sending automated queries"))
        && !html.contains("results-standard");
    if robot_wall {
        return Err(challenge("Mojeek", "its automated-queries wall"));
    }
    if !(200..300).contains(&status) {
        return Err(call.http_error(status, &html));
    }
    Ok(SearchResponse::from_sources(dedupe(
        parse_mojeek(&html),
        count,
    )))
}

pub(crate) fn parse_mojeek(html: &str) -> Vec<SearchSource> {
    let own = ["mojeek.com", "mojeek.co.uk", "mojeek.fr", "mojeek.de"];
    tags(html, "ul")
        .into_iter()
        .filter(|list| list.has_class("results-standard"))
        .flat_map(|list| {
            let region = inner(html, &list, "ul");
            let starts: Vec<usize> = tags(region, "li").iter().map(|tag| tag.start).collect();
            starts
                .iter()
                .enumerate()
                .map(|(i, start)| {
                    &region[*start..starts.get(i + 1).copied().unwrap_or(region.len())]
                })
                .filter_map(|item| {
                    let anchor = tags(item, "a")
                        .into_iter()
                        .find(|tag| tag.has_class("title"))?;
                    let url = external_url(&anchor.attr("href")?, "https://www.mojeek.com/", &own)?;
                    let title = html_text(inner(item, &anchor, "a"));
                    (!title.is_empty()).then(|| SearchSource {
                        title,
                        url,
                        snippet: classed_text(item, &["p"], "s"),
                        published_date: None,
                        author: None,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Ecosia (Google-backed results behind Cloudflare); no date filter exists,
/// so recency is ignored rather than approximated.
pub(crate) async fn ecosia(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 20);
    let request = call
        .browser_request(
            reqwest::Method::GET,
            &call.endpoint(ECOSIA_SEARCH_URL),
            Some(ECOSIA_HOME_URL),
        )
        .query(&[("q", format_scraper_query(call.query, GOOGLE_QUERY_SYNTAX))]);
    let (status, html) = call.send_raw(request).await?;
    let blocked = status == 403
        || status == 429
        || html.contains("Ecosia Firewall")
        || html.contains("_cf_chl_opt")
        || html.contains("/cdn-cgi/challenge-platform/");
    if blocked {
        return Err(challenge("Ecosia", "a Cloudflare bot challenge"));
    }
    if !(200..300).contains(&status) {
        return Err(call.http_error(status, &html));
    }
    Ok(SearchResponse::from_sources(dedupe(
        parse_ecosia(&html),
        count,
    )))
}

/// The first tag in `html` whose `data-test-id` is `id`, with its tag name.
fn test_id_tag<'h>(
    html: &'h str,
    names: &[&'static str],
    id: &str,
) -> Option<(Tag<'h>, &'static str)> {
    names.iter().find_map(|name| {
        tags(html, name)
            .into_iter()
            .find(|tag| tag.attr("data-test-id").as_deref() == Some(id))
            .map(|tag| (tag, *name))
    })
}

pub(crate) fn parse_ecosia(html: &str) -> Vec<SearchSource> {
    tags(html, "article")
        .into_iter()
        .filter(|tag| tag.attr("data-test-id").as_deref() == Some("organic-result"))
        .filter_map(|article| {
            let body = inner(html, &article, "article");
            let (heading, heading_name) =
                test_id_tag(body, &["h2", "h3", "div", "span"], "result-title")?;
            // The title heading sits inside its target link.
            let anchor = tags(body, "a")
                .into_iter()
                .rfind(|tag| tag.start < heading.start)?;
            let url = external_url(&anchor.attr("href")?, ECOSIA_HOME_URL, &["ecosia.org"])?;
            let title = html_text(inner(body, &heading, heading_name));
            let snippet = ["web-result-description", "result-description"]
                .iter()
                .find_map(|id| {
                    let (tag, name) = test_id_tag(body, &["p", "div", "span"], id)?;
                    Some(html_text(inner(body, &tag, name))).filter(|text| !text.is_empty())
                });
            (!title.is_empty()).then_some(SearchSource {
                title,
                url,
                snippet,
                published_date: None,
                author: None,
            })
        })
        .collect()
}

/// Engines the Public Web aggregate fans out to; the order breaks ranking
/// ties (Google-index engines first, Mojeek's independent index last).
const PUBLIC_ENGINES: [ProviderId; 4] = [
    ProviderId::Startpage,
    ProviderId::Duckduckgo,
    ProviderId::Ecosia,
    ProviderId::Mojeek,
];
/// Every keyless engine in parallel, merged by cross-engine consensus (how
/// many engines returned a URL), then best per-engine rank. Fails only when
/// every engine fails. Each engine has its own soft deadline (from its observed latency,
/// `engine_limits`): once one engine answered, the aggregate stops waiting for an engine past
/// its own deadline.
pub(crate) async fn public(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(15, 30);
    let started = tokio::time::Instant::now();
    let deadlines: Vec<tokio::time::Instant> = PUBLIC_ENGINES
        .iter()
        .map(|engine| started + call.limits.soft_deadline(*engine))
        .collect();
    let mut pending: FuturesUnordered<_> = PUBLIC_ENGINES
        .iter()
        .enumerate()
        .map(|(index, engine)| {
            let engine_call = ProviderCall {
                provider: *engine,
                ..*call
            };
            async move { (index, super::dispatch(&engine_call).await) }
        })
        .collect();
    let mut responses: Vec<Option<SearchResponse>> = vec![None; PUBLIC_ENGINES.len()];
    let mut settled = [false; PUBLIC_ENGINES.len()];
    let mut failures = Vec::new();
    let hard = tokio::time::sleep(call.timeout);
    tokio::pin!(hard);
    loop {
        // The latest soft deadline among the engines still out.
        let still_due = deadlines
            .iter()
            .zip(settled)
            .filter(|(_, settled)| !settled)
            .map(|(deadline, _)| *deadline)
            .filter(|deadline| *deadline > tokio::time::Instant::now())
            .max();
        if still_due.is_none() && responses.iter().any(Option::is_some) {
            break;
        }
        tokio::select! {
            next = pending.next() => match next {
                Some((index, Ok(response))) => {
                    settled[index] = true;
                    responses[index] = Some(response);
                }
                Some((index, Err(error))) => {
                    settled[index] = true;
                    failures.push(format!("{}: {}", PUBLIC_ENGINES[index].id(), error.message));
                }
                None => break,
            },
            () = tokio::time::sleep_until(still_due.unwrap_or(started)), if still_due.is_some() => {}
            () = &mut hard => break,
        }
    }
    let mut merged: Vec<(SearchSource, usize, usize)> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    for response in responses.into_iter().flatten() {
        for (rank, source) in response.sources.into_iter().enumerate() {
            // Host without `www.`, path without a trailing slash, query
            // kept, fragment dropped: what engines disagree on for one page.
            let key = url::Url::parse(&source.url).map_or_else(
                |_| source.url.clone(),
                |url| {
                    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
                    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
                    let path = url.path();
                    let path = if path.len() > 1 {
                        path.trim_end_matches('/')
                    } else {
                        path
                    };
                    let query = url
                        .query()
                        .map(|query| format!("?{query}"))
                        .unwrap_or_default();
                    format!("{host}{path}{query}")
                },
            );
            if let Some(&at) = index_of.get(&key) {
                let (existing, engines, best_rank) = &mut merged[at];
                *engines += 1;
                if rank < *best_rank {
                    *best_rank = rank;
                    existing.title = source.title;
                    existing.url = source.url;
                }
                if source.snippet.as_ref().map_or(0, String::len)
                    > existing.snippet.as_ref().map_or(0, String::len)
                {
                    existing.snippet = source.snippet;
                }
            } else {
                index_of.insert(key, merged.len());
                merged.push((source, 1, rank));
            }
        }
    }
    if merged.is_empty() && failures.len() == PUBLIC_ENGINES.len() {
        return Err(ProviderError::new(
            format!("All public engines failed: {}", failures.join("; ")),
            Some(503),
        ));
    }
    // Stable sort: insertion order breaks the remaining ties.
    merged.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    Ok(SearchResponse::from_sources(
        merged
            .into_iter()
            .take(count)
            .map(|(source, _, _)| source)
            .collect(),
    ))
}

impl Recency {
    /// The single-letter window `DuckDuckGo` (`df`) and Startpage (`with_date`) take.
    fn letter(self) -> &'static str {
        match self {
            Recency::Day => "d",
            Recency::Week => "w",
            Recency::Month => "m",
            Recency::Year => "y",
        }
    }
}
