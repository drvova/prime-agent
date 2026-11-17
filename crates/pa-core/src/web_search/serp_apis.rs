//! Keyed SERP APIs: Serper (Google results), You.com Eco, Brave, and Kagi.
//! Each returns ranked organic results; Serper and Kagi add an answer box and
//! related questions.

use serde_json::{json, Value};

use super::dates;
use super::http::{array_field, http_url, str_field, truncate_chars, ProviderCall, ProviderResult};
use super::query::{QuerySyntax, GOOGLE_QUERY_SYNTAX};
use super::{Recency, SearchResponse, SearchSource};

const SERPER_URL: &str = "https://google.serper.dev/search";
const ECO_URL: &str = "https://ydc-index.io/v1/eco_search";
const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const KAGI_URL: &str = "https://kagi.com/api/v1/search";

/// Serper: `organic` results, the knowledge graph as the answer, and the
/// "People Also Ask" pairs as related questions.
pub(crate) async fn serper(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let mut body = json!({ "q": call.query.for_syntax(GOOGLE_QUERY_SYNTAX) });
    if let Some(count) = call.num_results {
        body["num"] = json!(count.clamp(1, 100));
    }
    if let Some(recency) = call.recency {
        body["tbs"] = json!(recency.google_tbs());
    }
    let request = call
        .client
        .post(call.endpoint(SERPER_URL))
        .header("X-API-KEY", call.key()?)
        .json(&body);
    let data = call.send_json(request).await?;
    Ok(parse_serper(&data, call.count(10, 100)))
}

pub(crate) fn parse_serper(data: &Value, count: usize) -> SearchResponse {
    let sources = organic_sources(array_field(data, "organic"), "link", "snippet", count);
    let answer = data.get("knowledgeGraph").and_then(|graph| {
        let mut lines = Vec::new();
        if let Some(title) = str_field(graph, "title") {
            lines.push(format!("Knowledge Graph: {title}"));
        }
        lines.extend(str_field(graph, "description"));
        if let Some(attributes) = graph.get("attributes").and_then(Value::as_object) {
            for (key, value) in attributes {
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string);
                if !text.trim().is_empty() {
                    lines.push(format!("{key}: {}", text.trim()));
                }
            }
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    });
    let related_questions = array_field(data, "peopleAlsoAsk")
        .iter()
        .take(3)
        .filter_map(|item| {
            let question = str_field(item, "question")?;
            Some(match str_field(item, "snippet") {
                Some(answer) => format!("{question} — {answer}"),
                None => question,
            })
        })
        .collect();
    SearchResponse {
        answer,
        related_questions,
        ..SearchResponse::from_sources(sources)
    }
}

/// You.com Eco Search (at most 10 results per page).
pub(crate) async fn eco(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let request = call
        .client
        .get(call.endpoint(ECO_URL))
        .query(&[("query", call.query.for_syntax(GOOGLE_QUERY_SYNTAX))])
        .header("X-API-Key", call.key()?);
    let data = call.send_json(request).await?;
    let web = data
        .get("results")
        .map_or(&[][..], |results| array_field(results, "web"));
    Ok(SearchResponse::from_sources(organic_sources(
        web,
        "url",
        "description",
        call.count(10, 10),
    )))
}

/// `title`/`<url_key>`/`<snippet_key>` rows into sources, http(s) only.
fn organic_sources(
    rows: &[Value],
    url_key: &str,
    snippet_key: &str,
    count: usize,
) -> Vec<SearchSource> {
    rows.iter()
        .filter_map(|row| {
            let url = http_url(row.get(url_key)?.as_str()?)?;
            Some(SearchSource {
                title: str_field(row, "title").unwrap_or_else(|| url.clone()),
                snippet: str_field(row, snippet_key),
                published_date: str_field(row, "date"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect()
}

/// Brave parses the operator set inline; date bounds ride `freshness`.
const BRAVE_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    date_range: false,
    ..GOOGLE_QUERY_SYNTAX
};

/// Brave Web Search: `description` plus `extra_snippets` per result.
pub(crate) async fn brave(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 20);
    let query = call.query.for_syntax(BRAVE_QUERY_SYNTAX);
    if query.chars().count() > 500 {
        return Err(super::http::ProviderError::new(
            "Brave search queries cannot exceed 500 characters",
            Some(400),
        ));
    }
    let mut params = vec![
        ("q", query),
        ("count", count.to_string()),
        ("extra_snippets", "true".to_string()),
        ("text_decorations", "false".to_string()),
        ("safesearch", "moderate".to_string()),
    ];
    let freshness = match (&call.query.after, &call.query.before) {
        (None, None) => call.recency.map(|recency| {
            match recency {
                Recency::Day => "pd",
                Recency::Week => "pw",
                Recency::Month => "pm",
                Recency::Year => "py",
            }
            .to_string()
        }),
        (after, before) => Some(format!(
            "{}to{}",
            after.as_deref().unwrap_or("1970-01-01"),
            before
                .clone()
                .unwrap_or_else(|| dates::epoch_seconds_to_iso(dates::now_epoch_seconds()))
        )),
    };
    params.extend(freshness.map(|freshness| ("freshness", freshness)));
    let request = call
        .client
        .get(call.endpoint(BRAVE_URL))
        .query(&params)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", call.key()?);
    let data = call.send_json(request).await?;
    Ok(parse_brave(&data, count))
}

pub(crate) fn parse_brave(data: &Value, count: usize) -> SearchResponse {
    let results = data
        .get("web")
        .map_or(&[][..], |web| array_field(web, "results"));
    let sources = results
        .iter()
        .filter_map(|result| {
            let url = http_url(result.get("url")?.as_str()?)?;
            let mut snippets: Vec<String> = Vec::new();
            for text in std::iter::once(result.get("description"))
                .chain(array_field(result, "extra_snippets").iter().map(Some))
                .flatten()
                .filter_map(Value::as_str)
            {
                let text = super::http::html_text(text);
                if !text.is_empty() && !snippets.contains(&text) {
                    snippets.push(text);
                }
            }
            Some(SearchSource {
                title: str_field(result, "title")
                    .map_or_else(|| url.clone(), |t| truncate_chars(&t, 300)),
                snippet: (!snippets.is_empty()).then(|| truncate_chars(&snippets.join("\n"), 8000)),
                published_date: str_field(result, "age"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    SearchResponse::from_sources(sources)
}

/// Kagi Search API v1: result buckets, related/adjacent questions, and a
/// direct answer.
pub(crate) async fn kagi(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let count = call.count(10, 40);
    let mut body = json!({
        "query": call.query.for_syntax(GOOGLE_QUERY_SYNTAX),
        "workflow": "search",
        "limit": count,
    });
    if let Some(recency) = call.recency {
        body["filters"] = json!({ "after": dates::iso_days_ago(recency.days()) });
    }
    let request = call
        .client
        .post(call.endpoint(KAGI_URL))
        .bearer_auth(call.key()?)
        .header("Accept", "application/json")
        .json(&body);
    let data = call.send_json(request).await?;
    // A 200 envelope can still carry the error list.
    let error = data.get("error").and_then(|error| {
        error.as_str().map(str::to_string).or_else(|| {
            error.as_array()?.iter().find_map(|entry| {
                str_field(entry, "message")
                    .or_else(|| str_field(entry, "msg"))
                    .or_else(|| entry.get("code").map(Value::to_string))
            })
        })
    });
    if let Some(error) = error {
        return Err(super::http::ProviderError::new(
            format!("Kagi API error: {error}"),
            Some(502),
        ));
    }
    Ok(parse_kagi(&data, count))
}

pub(crate) fn parse_kagi(data: &Value, count: usize) -> SearchResponse {
    let Some(buckets) = data.get("data") else {
        return SearchResponse::default();
    };
    let mut sources = Vec::new();
    for (bucket, tag) in [
        ("search", ""),
        ("video", "[Video] "),
        ("news", "[News] "),
        ("infobox", "[Info] "),
    ] {
        for item in array_field(buckets, bucket) {
            let Some(url) = ["url", "href", "link"]
                .iter()
                .find_map(|key| str_field(item, key))
            else {
                continue;
            };
            let title = str_field(item, "title")
                .or_else(|| str_field(item, "name"))
                .unwrap_or_else(|| url.clone());
            sources.push(SearchSource {
                title: format!("{tag}{title}"),
                snippet: ["snippet", "description", "summary"]
                    .iter()
                    .find_map(|key| str_field(item, key)),
                published_date: str_field(item, "time"),
                author: None,
                url,
            });
        }
    }
    sources.truncate(count);
    let related_questions = ["adjacent_question", "related_search"]
        .iter()
        .flat_map(|bucket| array_field(buckets, bucket))
        .filter_map(|item| {
            let props = item.get("props");
            props
                .and_then(|props| {
                    str_field(props, "question").or_else(|| str_field(props, "query"))
                })
                .or_else(|| str_field(item, "title"))
        })
        .collect();
    let answer = array_field(buckets, "direct_answer")
        .first()
        .and_then(|answer| str_field(answer, "snippet").or_else(|| str_field(answer, "title")));
    SearchResponse {
        answer,
        related_questions,
        ..SearchResponse::from_sources(sources)
    }
}

impl Recency {
    /// Google's `tbs` time window (Serper, Firecrawl).
    pub(crate) fn google_tbs(self) -> &'static str {
        match self {
            Recency::Day => "qdr:d",
            Recency::Week => "qdr:w",
            Recency::Month => "qdr:m",
            Recency::Year => "qdr:y",
        }
    }
}
