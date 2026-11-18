//! Keyed agent-oriented search APIs: Tavily, Exa, Perplexity (Sonar), Jina
//! Reader, Firecrawl, and Parallel. Several synthesize an answer next to the
//! sources (Tavily's answer, Exa's per-result summaries, Perplexity's reply).

use serde_json::{json, Value};

use super::dates;
use super::http::{
    array_field, http_url, normalize_text, str_field, truncate_chars, ProviderCall, ProviderError,
    ProviderResult,
};
use super::query::{QuerySyntax, StructuredQuery, GOOGLE_QUERY_SYNTAX, PHRASE_QUERY_SYNTAX};
use super::{SearchCitation, SearchResponse, SearchSource};

const TAVILY_URL: &str = "https://api.tavily.com/search";
const EXA_URL: &str = "https://api.exa.ai/search";
const PERPLEXITY_URL: &str = "https://api.perplexity.ai/chat/completions";
const JINA_URL: &str = "https://s.jina.ai/";
const FIRECRAWL_URL: &str = "https://api.firecrawl.dev/v2/search";
const PARALLEL_URL: &str = "https://api.parallel.ai/v1beta/search";
const PARALLEL_BETA_HEADER: &str = "search-extract-2025-10-10";

/// Tavily: `site:`/`-site:` map to domain lists and `after:`/`before:` to the
/// date range; a time-filtered search that comes back empty retries once
/// without its time filter.
pub(crate) async fn tavily(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(5, 20);
    let mut body = json!({
        "query": parsed.for_syntax(PHRASE_QUERY_SYNTAX),
        "search_depth": "basic",
        "max_results": count,
        "include_answer": "advanced",
        "include_raw_content": false,
    });
    let include = StructuredQuery::site_hosts(&parsed.sites);
    let exclude = StructuredQuery::site_hosts(&parsed.excluded_sites);
    if !include.is_empty() {
        body["include_domains"] = json!(include);
    }
    if !exclude.is_empty() {
        body["exclude_domains"] = json!(exclude);
    }
    let mut timed = false;
    if let Some(after) = &parsed.after {
        body["start_date"] = json!(after);
        timed = true;
    }
    if let Some(before) = &parsed.before {
        body["end_date"] = json!(before);
        timed = true;
    }
    if let (false, Some(recency)) = (timed, call.recency) {
        body["time_range"] = json!(recency.id());
        timed = true;
    }
    let key = call.key()?;
    let post = |body: &Value| {
        call.client
            .post(call.endpoint(TAVILY_URL))
            .bearer_auth(key)
            .json(body)
    };
    let response = parse_tavily(&call.send_json(post(&body)).await?, count);
    if !timed || response.answer.is_some() || !response.sources.is_empty() {
        return Ok(response);
    }
    if let Some(fields) = body.as_object_mut() {
        for field in ["start_date", "end_date", "time_range"] {
            fields.remove(field);
        }
    }
    Ok(parse_tavily(&call.send_json(post(&body)).await?, count))
}

pub(crate) fn parse_tavily(data: &Value, count: usize) -> SearchResponse {
    let sources = array_field(data, "results")
        .iter()
        .filter_map(|result| {
            let url = str_field(result, "url")?;
            Some(SearchSource {
                title: str_field(result, "title").unwrap_or_else(|| url.clone()),
                snippet: str_field(result, "content"),
                published_date: str_field(result, "published_date"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    SearchResponse {
        answer: str_field(data, "answer"),
        ..SearchResponse::from_sources(sources)
    }
}

/// Exa neural search: per-result summaries (the first three, titled, form the
/// answer); `site:` and date bounds map to native filters.
pub(crate) async fn exa(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(10, 100);
    let query = parsed.for_syntax(PHRASE_QUERY_SYNTAX);
    let mut body = json!({
        "query": query,
        "numResults": count,
        "type": "auto",
        "contents": { "summary": { "query": query } },
    });
    let include = StructuredQuery::site_hosts(&parsed.sites);
    let exclude = StructuredQuery::site_hosts(&parsed.excluded_sites);
    if !include.is_empty() {
        body["includeDomains"] = json!(include);
    }
    if !exclude.is_empty() {
        body["excludeDomains"] = json!(exclude);
    }
    let after = parsed.after.clone().or_else(|| {
        call.recency
            .map(|recency| dates::iso_days_ago(recency.days()))
    });
    if let Some(after) = after {
        body["startPublishedDate"] = json!(after);
    }
    if let Some(before) = &parsed.before {
        body["endPublishedDate"] = json!(before);
    }
    let request = call
        .client
        .post(call.endpoint(EXA_URL))
        .header("x-api-key", call.key()?)
        .json(&body);
    Ok(parse_exa(&call.send_json(request).await?, count))
}

pub(crate) fn parse_exa(data: &Value, count: usize) -> SearchResponse {
    let results: Vec<&Value> = array_field(data, "results")
        .iter()
        .filter(|result| str_field(result, "url").is_some())
        .collect();
    let summaries: Vec<String> = results
        .iter()
        .filter_map(|result| {
            let summary = str_field(result, "summary")?;
            let title = str_field(result, "title")
                .or_else(|| str_field(result, "url"))
                .unwrap_or_default();
            Some(format!("**{title}**: {summary}"))
        })
        .take(3)
        .collect();
    let sources = results
        .iter()
        .filter_map(|result| {
            let url = str_field(result, "url")?;
            let highlights: Vec<&str> = array_field(result, "highlights")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let snippet = str_field(result, "summary")
                .or_else(|| str_field(result, "text"))
                .or_else(|| normalize_text(&highlights.join(" ")))
                .map(|snippet| truncate_chars(&snippet, 500));
            Some(SearchSource {
                title: str_field(result, "title").unwrap_or_else(|| url.clone()),
                snippet,
                published_date: str_field(result, "publishedDate"),
                author: str_field(result, "author"),
                url,
            })
        })
        .take(count)
        .collect();
    SearchResponse {
        answer: (!summaries.is_empty()).then(|| summaries.join("\n\n")),
        ..SearchResponse::from_sources(sources)
    }
}

/// Perplexity's operator support beyond the native filters.
const PERPLEXITY_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    or: true,
    in_url: true,
    in_title: true,
    filetype: true,
    ..PHRASE_QUERY_SYNTAX
};

/// Perplexity Sonar: the synthesized reply is the answer, `search_results`
/// the sources, and `citations` the cited URLs. `site:`, date bounds, `lang:`
/// and recency map to the native search filters.
pub(crate) async fn perplexity(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let mut body = json!({
        "model": "sonar",
        "messages": [{ "role": "user", "content": parsed.for_syntax(PERPLEXITY_QUERY_SYNTAX) }],
        "return_related_questions": true,
    });
    if parsed.has_directives {
        let mut domains: Vec<String> = StructuredQuery::site_hosts(&parsed.sites);
        domains.extend(
            StructuredQuery::site_hosts(&parsed.excluded_sites)
                .into_iter()
                .map(|host| format!("-{host}")),
        );
        domains.truncate(20);
        if !domains.is_empty() {
            body["search_domain_filter"] = json!(domains);
        }
        // `%m/%d/%Y` without zero padding (`3/1/2025`).
        let us_date = |iso: &str| {
            let mut parts = iso.split('-').map(|part| part.trim_start_matches('0'));
            let (year, month, day) = (parts.next(), parts.next(), parts.next());
            format!(
                "{}/{}/{}",
                month.unwrap_or_default(),
                day.unwrap_or_default(),
                year.unwrap_or_default()
            )
        };
        if let Some(after) = &parsed.after {
            body["search_after_date_filter"] = json!(us_date(after));
        }
        if let Some(before) = &parsed.before {
            body["search_before_date_filter"] = json!(us_date(before));
        }
        if let Some(lang) = parsed.lang.as_deref().and_then(|lang| lang.get(..2)) {
            body["search_language_filter"] = json!([lang]);
        }
    }
    if let Some(recency) = call.recency {
        body["search_recency_filter"] = json!(recency.id());
    }
    let request = call
        .client
        .post(call.endpoint(PERPLEXITY_URL))
        .bearer_auth(call.key()?)
        .json(&body);
    Ok(parse_perplexity(
        &call.send_json(request).await?,
        call.count(10, 50),
    ))
}

pub(crate) fn parse_perplexity(data: &Value, count: usize) -> SearchResponse {
    let answer = array_field(data, "choices")
        .first()
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let results = array_field(data, "search_results");
    let sources: Vec<SearchSource> = results
        .iter()
        .filter_map(|result| {
            let url = http_url(result.get("url")?.as_str()?)?;
            Some(SearchSource {
                title: str_field(result, "title").unwrap_or_else(|| url.clone()),
                snippet: str_field(result, "snippet"),
                published_date: str_field(result, "date"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    let citations = array_field(data, "citations")
        .iter()
        .filter_map(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(|url| SearchCitation {
            title: results
                .iter()
                .find(|result| result.get("url").and_then(Value::as_str) == Some(url))
                .and_then(|result| str_field(result, "title"))
                .unwrap_or_else(|| url.to_string()),
            url: url.to_string(),
            cited_text: None,
        })
        .collect();
    let related_questions = array_field(data, "related_questions")
        .iter()
        .filter_map(Value::as_str)
        .filter_map(normalize_text)
        .collect();
    SearchResponse {
        answer,
        citations,
        related_questions,
        ..SearchResponse::from_sources(sources)
    }
}

/// Jina Reader search: one include `site:` rides the `X-Site` header (the
/// rest stays inline, Bing-backed).
pub(crate) async fn jina(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(5, 20);
    let site = (parsed.sites.len() == 1)
        .then(|| StructuredQuery::site_hosts(&parsed.sites).pop())
        .flatten();
    let query = parsed.for_syntax(QuerySyntax {
        site: site.is_none(),
        in_title: true,
        in_url: true,
        filetype: true,
        ..PHRASE_QUERY_SYNTAX
    });
    let mut url = url::Url::parse(&call.endpoint(JINA_URL))
        .map_err(|error| ProviderError::new(format!("Jina URL: {error}"), Some(500)))?;
    url.path_segments_mut()
        .map_err(|()| ProviderError::new("Jina URL cannot carry a path", Some(500)))?
        .pop_if_empty()
        .push(&query);
    url.query_pairs_mut()
        .append_pair("count", &count.to_string());
    let mut request = call
        .client
        .get(url.as_str())
        .bearer_auth(call.key()?)
        .header("Accept", "application/json")
        .header("X-Respond-With", "no-content")
        .header("X-Retain-Images", "none");
    if let Some(site) = &site {
        request = request.header("X-Site", site);
    }
    parse_jina(&call.send_json(request).await?, count)
}

pub(crate) fn parse_jina(data: &Value, count: usize) -> ProviderResult<SearchResponse> {
    let rows = match data {
        Value::Array(rows) => rows.as_slice(),
        Value::Object(envelope) => {
            if let Some(code) = envelope.get("code").and_then(Value::as_u64) {
                if code != 200 {
                    return Err(ProviderError::new(
                        format!("Jina API response reported failure ({code})"),
                        u16::try_from(code).ok(),
                    ));
                }
            }
            envelope
                .get("data")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .ok_or_else(|| ProviderError::new("Jina API returned no data array", Some(502)))?
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            return Err(ProviderError::new(
                "Jina API returned an invalid response",
                Some(502),
            ))
        }
    };
    let sources = rows
        .iter()
        .filter_map(|row| {
            let url = str_field(row, "url")?;
            Some(SearchSource {
                title: str_field(row, "title").unwrap_or_else(|| url.clone()),
                snippet: str_field(row, "description").or_else(|| str_field(row, "content")),
                published_date: None,
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    Ok(SearchResponse::from_sources(sources))
}

/// Firecrawl (SERP-backed): Google operators inline; absolute date bounds
/// move to a `tbs` custom range.
pub(crate) async fn firecrawl(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(10, 100);
    // ISO `YYYY-MM-DD` to Google's `MM/DD/YYYY`.
    let google_date = |iso: &str| {
        let parts: Vec<&str> = iso.split('-').collect();
        format!(
            "{}/{}/{}",
            parts.get(1).unwrap_or(&""),
            parts.get(2).unwrap_or(&""),
            parts.first().unwrap_or(&"")
        )
    };
    let date_tbs = (parsed.after.is_some() || parsed.before.is_some()).then(|| {
        let mut parts = vec!["cdr:1".to_string()];
        parts.extend(
            parsed
                .after
                .as_deref()
                .map(|after| format!("cd_min:{}", google_date(after))),
        );
        parts.extend(
            parsed
                .before
                .as_deref()
                .map(|before| format!("cd_max:{}", google_date(before))),
        );
        parts.join(",")
    });
    let syntax = QuerySyntax {
        date_range: date_tbs.is_none(),
        ..GOOGLE_QUERY_SYNTAX
    };
    let mut body = json!({
        "query": parsed.for_syntax(syntax),
        "limit": count,
        "sources": [{ "type": "web" }],
    });
    let tbs = date_tbs.or_else(|| call.recency.map(|recency| recency.google_tbs().to_string()));
    if let Some(tbs) = tbs {
        body["tbs"] = json!(tbs);
    }
    let request = call
        .client
        .post(call.endpoint(FIRECRAWL_URL))
        .bearer_auth(call.key()?)
        .json(&body);
    parse_firecrawl(&call.send_json(request).await?, count)
}

pub(crate) fn parse_firecrawl(data: &Value, count: usize) -> ProviderResult<SearchResponse> {
    if data.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(ProviderError::new(
            str_field(data, "error").unwrap_or_else(|| "Firecrawl request failed".to_string()),
            Some(502),
        ));
    }
    let rows = match data.get("data") {
        Some(Value::Array(rows)) => rows.as_slice(),
        Some(nested @ Value::Object(_)) => array_field(nested, "web"),
        Some(Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) | None => {
            array_field(data, "results")
        }
    };
    let sources = rows
        .iter()
        .filter_map(|row| {
            let url = str_field(row, "url")?;
            Some(SearchSource {
                title: str_field(row, "title").unwrap_or_else(|| url.clone()),
                snippet: ["description", "snippet", "markdown"]
                    .iter()
                    .find_map(|key| str_field(row, key)),
                published_date: None,
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    Ok(SearchResponse::from_sources(sources))
}

/// Parallel's beta Search API: an objective plus one search query;
/// `site:`/`-site:`/`after:` (or recency) map to the source policy
/// (exclusions only without an allow list, per Parallel's docs).
pub(crate) async fn parallel(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    let parsed = call.query;
    let count = call.count(10, 40);
    let query = parsed.for_syntax(QuerySyntax {
        or: true,
        ..PHRASE_QUERY_SYNTAX
    });
    let mut policy = serde_json::Map::new();
    let include = StructuredQuery::site_hosts(&parsed.sites);
    let exclude = StructuredQuery::site_hosts(&parsed.excluded_sites);
    if !include.is_empty() {
        policy.insert("include_domains".to_string(), json!(include));
    } else if !exclude.is_empty() {
        policy.insert("exclude_domains".to_string(), json!(exclude));
    }
    let after = parsed.after.clone().or_else(|| {
        call.recency
            .map(|recency| dates::iso_days_ago(recency.days()))
    });
    if let Some(after) = after {
        policy.insert("after_date".to_string(), json!(after));
    }
    let mut body = json!({
        "objective": query,
        "search_queries": [query],
        "mode": "fast",
        "excerpts": { "max_chars_per_result": 10_000 },
    });
    if !policy.is_empty() {
        body["source_policy"] = Value::Object(policy);
    }
    let request = call
        .client
        .post(call.endpoint(PARALLEL_URL))
        .header("Accept", "application/json")
        .header("x-api-key", call.key()?)
        .header("parallel-beta", PARALLEL_BETA_HEADER)
        .json(&body);
    Ok(parse_parallel(&call.send_json(request).await?, count))
}

pub(crate) fn parse_parallel(data: &Value, count: usize) -> SearchResponse {
    let sources = array_field(data, "results")
        .iter()
        .filter_map(|row| {
            let url = str_field(row, "url")?;
            let excerpts: Vec<&str> = array_field(row, "excerpts")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some(SearchSource {
                title: str_field(row, "title").unwrap_or_else(|| url.clone()),
                snippet: (!excerpts.is_empty()).then(|| excerpts.join("\n\n")),
                published_date: str_field(row, "publish_date"),
                author: None,
                url,
            })
        })
        .take(count)
        .collect();
    SearchResponse::from_sources(sources)
}
