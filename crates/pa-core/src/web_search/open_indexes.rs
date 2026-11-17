//! Open JSON search indexes: the keyless Hacker News Search API (Algolia) and
//! a self-hosted `SearXNG` instance's JSON API.

use serde_json::Value;

use super::dates;
use super::http::{
    array_field, html_text, http_url, str_field, ProviderCall, ProviderError, ProviderResult,
};
use super::query::{format_query, QueryTerm, GOOGLE_QUERY_SYNTAX, PHRASE_QUERY_SYNTAX};
use super::{SearchResponse, SearchSource};

const HN_SEARCH_URL: &str = "https://hn.algolia.com/api/v1/search";
const HN_ITEM_URL: &str = "https://news.ycombinator.com/item?id=";

/// Hacker News stories and comments by relevance. Stories and comments rank
/// on separate scales, so each kind is its own request; stories lead with
/// half the slots and either kind backfills the other's shortfall. Results
/// link the HN item page, so they satisfy a `site:news.ycombinator.com` scope.
pub(crate) async fn hackernews(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(10, 30);
    // `intitle:`/`intext:`/`inurl:` values join the keywords: Algolia matches
    // title, text, and link URL in one query.
    let mut keyword_query = parsed.clone();
    keyword_query.terms.extend(
        parsed
            .in_title
            .iter()
            .chain(&parsed.in_text)
            .chain(&parsed.in_url)
            .map(|text| QueryTerm {
                text: text.clone(),
                phrase: false,
                negated: false,
                group: None,
            }),
    );
    let keywords = if keyword_query.terms.is_empty() {
        String::new()
    } else {
        format_query(&keyword_query, PHRASE_QUERY_SYNTAX)
    };
    let mut params = vec![
        ("query", keywords.clone()),
        ("advancedSyntax", "true".to_string()),
        ("hitsPerPage", count.to_string()),
    ];
    // OR alternatives stop being required and rank by how many match.
    let optional: Vec<&str> = parsed
        .terms
        .iter()
        .filter(|term| term.group.is_some() && !term.negated)
        .map(|term| term.text.as_str())
        .collect();
    if !optional.is_empty() {
        params.push(("optionalWords", optional.join(" ")));
    }
    let start = parsed.after.clone().or_else(|| {
        call.recency
            .filter(|_| parsed.before.is_none())
            .map(|recency| dates::iso_days_ago(recency.days()))
    });
    let filters: Vec<String> = [
        start
            .as_deref()
            .and_then(dates::iso_to_epoch_seconds)
            .map(|at| format!("created_at_i>={at}")),
        parsed
            .before
            .as_deref()
            .and_then(dates::iso_to_epoch_seconds)
            .map(|at| format!("created_at_i<{at}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !filters.is_empty() {
        params.push(("numericFilters", filters.join(",")));
    }
    let fetch = |kind: &'static str| {
        let mut params = params.clone();
        params.push(("tags", kind.to_string()));
        let request = call
            .client
            .get(call.endpoint(HN_SEARCH_URL))
            .header("Accept", "application/json")
            .query(&params);
        async move { call.send_json(request).await }
    };
    // Comments carry no score: a keyword-less listing is stories only.
    let (stories, comments) = if keywords.is_empty() {
        (fetch("story").await?, Value::Null)
    } else {
        futures::try_join!(fetch("story"), fetch("comment"))?
    };
    Ok(merge_hackernews(&stories, &comments, count))
}

pub(crate) fn merge_hackernews(stories: &Value, comments: &Value, count: usize) -> SearchResponse {
    let to_source = |hit: &Value, story: bool| -> Option<SearchSource> {
        let id = hit
            .get("objectID")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?;
        let url = format!("{HN_ITEM_URL}{id}");
        let author = str_field(hit, "author");
        let body = hit
            .get(if story { "story_text" } else { "comment_text" })
            .and_then(Value::as_str)
            .map(html_text)
            .filter(|text| !text.is_empty());
        let (title, snippet) = if story {
            let mut stats = vec![
                format!(
                    "{} points",
                    hit.get("points").and_then(Value::as_u64).unwrap_or(0)
                ),
                format!(
                    "{} comments",
                    hit.get("num_comments").and_then(Value::as_u64).unwrap_or(0)
                ),
            ];
            stats.extend(author.as_ref().map(|author| format!("by {author}")));
            let parts: Vec<String> = std::iter::once(stats.join(", "))
                .chain(str_field(hit, "url"))
                .chain(body)
                .collect();
            (
                str_field(hit, "title").unwrap_or_else(|| url.clone()),
                Some(parts.join(" — ")),
            )
        } else {
            let by = author.as_ref().map_or_else(
                || "Comment".to_string(),
                |author| format!("Comment by {author}"),
            );
            let title = match str_field(hit, "story_title") {
                Some(story_title) => format!("{by} on: {story_title}"),
                None => by,
            };
            (title, body)
        };
        Some(SearchSource {
            title,
            url,
            snippet,
            published_date: str_field(hit, "created_at"),
            author,
        })
    };
    let stories: Vec<SearchSource> = array_field(stories, "hits")
        .iter()
        .filter_map(|hit| to_source(hit, true))
        .collect();
    let comments: Vec<SearchSource> = array_field(comments, "hits")
        .iter()
        .filter_map(|hit| to_source(hit, false))
        .collect();
    let story_slots = count.div_ceil(2).max(count.saturating_sub(comments.len()));
    let mut sources: Vec<SearchSource> = stories.into_iter().take(story_slots).collect();
    let rest = count.saturating_sub(sources.len());
    sources.extend(comments.into_iter().take(rest));
    SearchResponse::from_sources(sources)
}

/// A `SearXNG` instance's JSON API (`format=json` must be enabled on it).
pub(crate) async fn searxng(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let endpoint = call.access.ok_or_else(|| {
        ProviderError::new("SearXNG needs SEARXNG_ENDPOINT (its base URL)", Some(400))
    })?;
    let count = call.count(10, 20);
    let mut params = vec![
        ("q", call.query.for_syntax(GOOGLE_QUERY_SYNTAX)),
        ("format", "json".to_string()),
        ("pageno", "1".to_string()),
    ];
    if let Some(recency) = call.recency {
        params.push(("time_range", recency.id().to_string()));
    }
    if let Some(lang) = &call.query.lang {
        params.push(("language", lang.clone()));
    }
    let request = call
        .client
        .get(format!("{}/search", endpoint.trim_end_matches('/')))
        .header("Accept", "application/json")
        .query(&params);
    Ok(parse_searxng(&call.send_json(request).await?, count))
}

pub(crate) fn parse_searxng(data: &Value, count: usize) -> SearchResponse {
    let sources = array_field(data, "results")
        .iter()
        .filter_map(|row| {
            let url = http_url(row.get("url")?.as_str()?)?;
            Some(SearchSource {
                title: str_field(row, "title").unwrap_or_else(|| url.clone()),
                snippet: str_field(row, "content"),
                published_date: str_field(row, "publishedDate"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    let answers: Vec<String> = array_field(data, "answers")
        .iter()
        .filter_map(|answer| match answer {
            Value::String(text) => super::http::normalize_text(text),
            Value::Object(_) => str_field(answer, "answer"),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
        })
        .collect();
    SearchResponse {
        answer: (!answers.is_empty()).then(|| answers.join("\n")),
        ..SearchResponse::from_sources(sources)
    }
}
