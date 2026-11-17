//! Structured web-search query parsing.
//!
//! Agents embed Google-style directives in queries (`site:`, `before:`/`after:`,
//! `inurl:`, `filetype:`, quoted phrases, `OR` groups, `-exclusions`) whatever
//! engine answers. The parsed [`StructuredQuery`] lets each provider map the
//! constraints onto native parameters or re-emit only the syntax its engine
//! understands ([`format_query`]), and the pipeline post-filters every
//! response leniently ([`apply_query_constraints`]): a constraint that would
//! eliminate every result is relaxed and reported instead of returning nothing.

use super::dates;
use super::SearchSource;

/// One free-text token of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryTerm {
    pub(crate) text: String,
    /// Quoted exact phrase (`"like this"`) or verbatim-required (`+term`).
    pub(crate) phrase: bool,
    /// Excluded via `-term` or `NOT term`.
    pub(crate) negated: bool,
    /// OR-group id: terms sharing one are alternatives; groups are contiguous.
    pub(crate) group: Option<u32>,
}

impl QueryTerm {
    fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            phrase: false,
            negated: false,
            group: None,
        }
    }
}

/// A raw query decomposed into free text plus every recognized constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StructuredQuery {
    pub(crate) raw: String,
    pub(crate) terms: Vec<QueryTerm>,
    /// `site:` includes (any-of): lowercased, scheme stripped, may carry a path.
    pub(crate) sites: Vec<String>,
    pub(crate) excluded_sites: Vec<String>,
    pub(crate) in_url: Vec<String>,
    pub(crate) excluded_in_url: Vec<String>,
    pub(crate) in_title: Vec<String>,
    pub(crate) excluded_in_title: Vec<String>,
    /// Body substrings: query-building only (snippets are partial).
    pub(crate) in_text: Vec<String>,
    pub(crate) excluded_in_text: Vec<String>,
    /// `filetype:`/`ext:` (any-of), lowercased, no leading dot.
    pub(crate) filetypes: Vec<String>,
    pub(crate) excluded_filetypes: Vec<String>,
    /// Inclusive lower publish-date bound, ISO `YYYY-MM-DD`.
    pub(crate) after: Option<String>,
    /// Exclusive upper publish-date bound, ISO `YYYY-MM-DD`.
    pub(crate) before: Option<String>,
    pub(crate) lang: Option<String>,
    /// Any directive or boolean operator was recognized.
    pub(crate) has_directives: bool,
}

/// Which parsed features an engine's query syntax re-emits ([`format_query`]).
/// The zero value renders plain keywords for natural-language APIs.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per independent syntax feature
pub(crate) struct QuerySyntax {
    pub(crate) phrases: bool,
    pub(crate) negation: bool,
    pub(crate) or: bool,
    pub(crate) site: bool,
    pub(crate) in_url: bool,
    pub(crate) in_title: bool,
    pub(crate) in_text: bool,
    pub(crate) filetype: bool,
    pub(crate) date_range: bool,
}

/// The classic operator set (Google, Startpage, Ecosia, Brave, Kagi, `SearXNG`).
pub(crate) const GOOGLE_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: true,
    site: true,
    in_url: true,
    in_title: true,
    in_text: true,
    filetype: true,
    date_range: true,
};

/// Quoted phrases and exclusions only (natural-language engines).
pub(crate) const PHRASE_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: false,
    site: false,
    in_url: false,
    in_title: false,
    in_text: false,
    filetype: false,
    date_range: false,
};

impl StructuredQuery {
    /// Any post-filterable constraint (sites, url/title terms, filetypes, dates).
    pub(crate) fn has_constraints(&self) -> bool {
        !(self.sites.is_empty()
            && self.excluded_sites.is_empty()
            && self.in_url.is_empty()
            && self.excluded_in_url.is_empty()
            && self.in_title.is_empty()
            && self.excluded_in_title.is_empty()
            && self.filetypes.is_empty()
            && self.excluded_filetypes.is_empty()
            && self.after.is_none()
            && self.before.is_none())
    }

    /// Bare, deduplicated hosts of the `site:` values (paths are enforced by
    /// the post-filter).
    pub(crate) fn site_hosts(sites: &[String]) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for site in sites {
            let host = site.split('/').next().unwrap_or_default();
            if !host.is_empty() && !hosts.iter().any(|known| known == host) {
                hosts.push(host.to_string());
            }
        }
        hosts
    }

    /// The query for an engine with `syntax`: directive-free queries pass
    /// through verbatim.
    pub(crate) fn for_syntax(&self, syntax: QuerySyntax) -> String {
        if self.has_directives {
            format_query(self, syntax)
        } else {
            self.raw.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Site,
    InUrl,
    InTitle,
    InText,
    Filetype,
    Before,
    After,
    Lang,
}

fn directive_field(name: &str) -> Option<Field> {
    Some(match name {
        "site" | "domain" | "host" => Field::Site,
        "inurl" | "url" => Field::InUrl,
        "intitle" | "title" => Field::InTitle,
        "intext" | "inbody" | "inanchor" => Field::InText,
        "filetype" | "ext" => Field::Filetype,
        "before" | "until" => Field::Before,
        "after" | "since" => Field::After,
        "lang" | "language" => Field::Lang,
        _ => return None,
    })
}

fn all_mode(name: &str) -> Option<Field> {
    match name {
        "allintitle" => Some(Field::InTitle),
        "allinurl" => Some(Field::InUrl),
        "allintext" => Some(Field::InText),
        _ => None,
    }
}

struct RawToken {
    text: String,
    quoted: bool,
    quoted_value: bool,
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\u{201c}' | '\u{201d}')
}

fn tokenize(raw: &str) -> Vec<RawToken> {
    let chars: Vec<char> = raw.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if is_quote(ch) {
            let mut j = i + 1;
            let mut buf = String::new();
            while j < chars.len() && !is_quote(chars[j]) {
                buf.push(chars[j]);
                j += 1;
            }
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                tokens.push(RawToken {
                    text: trimmed.to_string(),
                    quoted: true,
                    quoted_value: false,
                });
            }
            i = j + 1;
            continue;
        }
        let mut buf = String::new();
        let mut quoted_value = false;
        while i < chars.len() && !chars[i].is_whitespace() {
            let c = chars[i];
            if is_quote(c) && buf.ends_with(':') {
                let mut j = i + 1;
                while j < chars.len() && !is_quote(chars[j]) {
                    buf.push(chars[j]);
                    j += 1;
                }
                quoted_value = true;
                i = j + 1;
                continue;
            }
            if is_quote(c) {
                break;
            }
            buf.push(c);
            i += 1;
        }
        if !buf.is_empty() {
            tokens.push(RawToken {
                text: buf,
                quoted: false,
                quoted_value,
            });
        }
    }
    split_parens(tokens)
}

/// Split leading `(` and unbalanced trailing `)` into standalone tokens so
/// `(react OR vue)` parses while `site:wikipedia.org/Foo_(bar)` stays whole.
fn split_parens(tokens: Vec<RawToken>) -> Vec<RawToken> {
    let paren = |text: &str| RawToken {
        text: text.to_string(),
        quoted: false,
        quoted_value: false,
    };
    let mut out = Vec::new();
    for token in tokens {
        if token.quoted || token.quoted_value {
            out.push(token);
            continue;
        }
        let mut text = token.text.as_str();
        while let Some(rest) = text.strip_prefix('(') {
            out.push(paren("("));
            text = rest;
        }
        let mut trailing = 0;
        while let Some(body) = text.strip_suffix(')') {
            let depth = body.chars().fold(0i32, |depth, c| match c {
                '(' => depth + 1,
                ')' => depth - 1,
                _ => depth,
            });
            if depth > 0 {
                break;
            }
            text = body;
            trailing += 1;
        }
        if !text.is_empty() {
            out.push(paren(text));
        }
        for _ in 0..trailing {
            out.push(paren(")"));
        }
    }
    out
}

/// Split `name:value` (optionally `+`/`-` prefixed) into (sign, name, value).
fn split_directive(text: &str) -> Option<(char, String, String)> {
    let (sign, body) = match text.chars().next() {
        Some(sign @ ('+' | '-')) => (sign, &text[1..]),
        _ => (' ', text),
    };
    let colon = body.find(':')?;
    let name = &body[..colon];
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphabetic() || c == '-') {
        return None;
    }
    Some((
        sign,
        name.to_ascii_lowercase(),
        body[colon + 1..].to_string(),
    ))
}

fn is_reserved_token(text: &str) -> bool {
    if matches!(
        text,
        "(" | ")" | "OR" | "AND" | "NOT" | "|" | "||" | "&&" | "!"
    ) {
        return true;
    }
    split_directive(text)
        .is_some_and(|(_, name, _)| directive_field(&name).is_some() || all_mode(&name).is_some())
}

/// Parse a `before:`/`after:` value into ISO `YYYY-MM-DD` (`YYYY`, `YYYY-MM`,
/// `YYYY-MM-DD` with `-`/`/`/`.` separators, or `MM/DD/YYYY`).
pub(crate) fn parse_date_value(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.trim().split(['-', '/', '.']).collect();
    let numbers: Option<Vec<u32>> = parts
        .iter()
        .map(|part| {
            (!part.is_empty() && part.len() <= 4 && part.chars().all(|c| c.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect();
    let numbers = numbers?;
    let (year, month, day) = match (parts.first().map(|part| part.len()), numbers.as_slice()) {
        (Some(4), [year]) => (*year, 1, 1),
        (Some(4), [year, month]) => (*year, *month, 1),
        (Some(4), [year, month, day]) => (*year, *month, *day),
        (Some(1 | 2), [first, second, year]) if parts[2].len() == 4 => {
            if *first > 12 && *second <= 12 {
                (*year, *second, *first)
            } else {
                (*year, *first, *second)
            }
        }
        _ => return None,
    };
    if !(1000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year}-{month:02}-{day:02}"))
}

fn normalize_site(value: &str) -> String {
    let mut site = value.trim().to_lowercase();
    if let Some(at) = site.find("://") {
        let scheme = &site[..at];
        if scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        {
            site = site[at + 3..].to_string();
        }
    }
    if let Some(rest) = site.strip_prefix("*.") {
        site = rest.to_string();
    }
    site.trim_end_matches(['/', '.']).to_string()
}

#[derive(Default)]
struct Parser {
    query: StructuredQuery,
    negate_next: bool,
    or_pending: bool,
    last_was_term: bool,
    group_seq: u32,
    all_mode: Option<Field>,
}

impl Parser {
    fn push_constraint(&mut self, field: Field, value: &str, negated: bool) {
        self.query.has_directives = true;
        self.or_pending = false;
        self.last_was_term = false;
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let q = &mut self.query;
        match (field, negated) {
            (Field::Site, _) => {
                let site = normalize_site(value);
                if !site.is_empty() {
                    if negated {
                        q.excluded_sites.push(site);
                    } else {
                        q.sites.push(site);
                    }
                }
            }
            (Field::InUrl, false) => q.in_url.push(value.to_string()),
            (Field::InUrl, true) => q.excluded_in_url.push(value.to_string()),
            (Field::InTitle, false) => q.in_title.push(value.to_string()),
            (Field::InTitle, true) => q.excluded_in_title.push(value.to_string()),
            (Field::InText, false) => q.in_text.push(value.to_string()),
            (Field::InText, true) => q.excluded_in_text.push(value.to_string()),
            (Field::Filetype, _) => {
                let ext = value.to_lowercase().trim_start_matches('.').to_string();
                if !ext.is_empty() {
                    if negated {
                        q.excluded_filetypes.push(ext);
                    } else {
                        q.filetypes.push(ext);
                    }
                }
            }
            (Field::Before | Field::After | Field::Lang, _) => {}
        }
    }

    fn push_term(&mut self, text: &str, phrase: bool) {
        let negated = std::mem::take(&mut self.negate_next);
        if let Some(mode) = self.all_mode {
            self.push_constraint(mode, text, negated);
            return;
        }
        let mut term = QueryTerm {
            text: text.to_string(),
            phrase,
            negated,
            group: None,
        };
        if self.or_pending && self.last_was_term {
            if let Some(prev) = self.query.terms.last_mut() {
                let group = *prev.group.get_or_insert_with(|| {
                    self.group_seq += 1;
                    self.group_seq
                });
                term.group = Some(group);
            }
        }
        self.or_pending = false;
        self.last_was_term = true;
        self.query.terms.push(term);
    }

    fn end_directive(&mut self) {
        self.query.has_directives = true;
        self.or_pending = false;
        self.last_was_term = false;
    }
}

/// Parse a raw query. Lenient: unknown `name:value` tokens stay free text,
/// and a directive with an unparseable value degrades to a plain term.
pub(crate) fn parse_search_query(raw: &str) -> StructuredQuery {
    let mut p = Parser {
        query: StructuredQuery {
            raw: raw.to_string(),
            ..StructuredQuery::default()
        },
        ..Parser::default()
    };
    let tokens = tokenize(raw);
    let mut idx = 0;
    while idx < tokens.len() {
        let token = &tokens[idx];
        idx += 1;
        if token.quoted {
            p.push_term(&token.text, true);
            continue;
        }
        match token.text.as_str() {
            "(" | ")" => continue,
            "OR" | "|" | "||" => {
                p.or_pending = true;
                p.query.has_directives = true;
                continue;
            }
            "AND" | "&&" => {
                p.query.has_directives = true;
                continue;
            }
            "NOT" | "!" => {
                p.negate_next = true;
                p.query.has_directives = true;
                continue;
            }
            "-" | "+" => {
                if token.text == "-" && tokens.get(idx).is_some_and(|next| next.quoted) {
                    p.negate_next = true;
                }
                continue;
            }
            _ => {}
        }
        let directive = split_directive(&token.text);
        if let Some((sign, name, value)) = &directive {
            if let Some(mode) = all_mode(name) {
                p.all_mode = Some(mode);
                p.query.has_directives = true;
                if !value.trim().is_empty() {
                    p.push_constraint(mode, value, *sign == '-');
                }
                p.or_pending = false;
                p.last_was_term = false;
                continue;
            }
            if let Some(field) = directive_field(name) {
                let mut value = value.trim().to_string();
                if value.is_empty() {
                    if let Some(next) = tokens.get(idx) {
                        if next.quoted || !is_reserved_token(&next.text) {
                            value = next.text.trim().to_string();
                            idx += 1;
                        }
                    }
                }
                if value.is_empty() {
                    p.query.has_directives = true;
                    continue;
                }
                let negated = *sign == '-' || std::mem::take(&mut p.negate_next);
                match field {
                    Field::Before | Field::After => {
                        let Some(iso) = parse_date_value(&value) else {
                            p.push_term(&token.text, false);
                            continue;
                        };
                        if field == Field::Before {
                            p.query.before = Some(iso);
                        } else {
                            p.query.after = Some(iso);
                        }
                        p.end_directive();
                    }
                    Field::Lang => {
                        p.query.lang = Some(value.to_lowercase());
                        p.end_directive();
                    }
                    Field::Site
                    | Field::InUrl
                    | Field::InTitle
                    | Field::InText
                    | Field::Filetype => p.push_constraint(field, &value, negated),
                }
                continue;
            }
        }
        let text = token.text.as_str();
        if text.len() > 1 && text.starts_with('-') {
            p.negate_next = true;
            p.query.has_directives = true;
            let stripped = text.trim_start_matches('-');
            if stripped.is_empty() {
                continue;
            }
            if let Some((_, name, value)) = split_directive(stripped) {
                if let Some(
                    field @ (Field::Site
                    | Field::InUrl
                    | Field::InTitle
                    | Field::InText
                    | Field::Filetype),
                ) = directive_field(&name)
                {
                    p.negate_next = false;
                    p.push_constraint(field, &value, true);
                    continue;
                }
            }
            p.push_term(stripped, false);
            continue;
        }
        if let Some(stripped) = text.strip_prefix('+').filter(|rest| !rest.is_empty()) {
            p.push_term(stripped, true);
            p.query.has_directives = true;
            continue;
        }
        p.push_term(text, false);
    }
    p.query
}

fn quote_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn render_term(term: &QueryTerm, syntax: QuerySyntax) -> Option<String> {
    if term.negated && !syntax.negation {
        return None;
    }
    let body = if term.phrase && syntax.phrases {
        format!("\"{}\"", term.text)
    } else {
        term.text.clone()
    };
    Some(if term.negated {
        format!("-{body}")
    } else {
        body
    })
}

fn render_terms(terms: &[QueryTerm], syntax: QuerySyntax) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < terms.len() {
        let term = &terms[i];
        match term.group {
            Some(group) if syntax.or => {
                let mut members = Vec::new();
                let mut j = i;
                while j < terms.len() && terms[j].group == Some(group) {
                    members.extend(render_term(&terms[j], syntax));
                    j += 1;
                }
                i = j;
                match members.len() {
                    0 => {}
                    1 => parts.push(members.remove(0)),
                    _ => parts.push(format!("({})", members.join(" OR "))),
                }
            }
            Some(_) | None => {
                parts.extend(render_term(term, syntax));
                i += 1;
            }
        }
    }
    parts.join(" ")
}

fn push_any_of(parts: &mut Vec<String>, prefix: &str, values: &[String], syntax: QuerySyntax) {
    if values.len() > 1 && syntax.or {
        let joined: Vec<String> = values.iter().map(|v| format!("{prefix}:{v}")).collect();
        parts.push(format!("({})", joined.join(" OR ")));
    } else {
        parts.extend(values.iter().map(|v| format!("{prefix}:{v}")));
    }
}

/// Rebuild a query for an engine with `syntax`. Never empty for a non-empty
/// input: a directives-only query falls back to the constraint values.
pub(crate) fn format_query(q: &StructuredQuery, syntax: QuerySyntax) -> String {
    let mut parts = Vec::new();
    let text = render_terms(&q.terms, syntax);
    if !text.is_empty() {
        parts.push(text);
    }
    if syntax.site {
        push_any_of(&mut parts, "site", &q.sites, syntax);
        parts.extend(q.excluded_sites.iter().map(|s| format!("-site:{s}")));
    }
    let pairs = [
        (syntax.in_url, "inurl", &q.in_url, &q.excluded_in_url),
        (
            syntax.in_title,
            "intitle",
            &q.in_title,
            &q.excluded_in_title,
        ),
        (syntax.in_text, "intext", &q.in_text, &q.excluded_in_text),
    ];
    for (enabled, name, included, excluded) in pairs {
        if enabled {
            parts.extend(
                included
                    .iter()
                    .map(|v| format!("{name}:{}", quote_value(v))),
            );
            parts.extend(
                excluded
                    .iter()
                    .map(|v| format!("-{name}:{}", quote_value(v))),
            );
        }
    }
    if syntax.filetype {
        push_any_of(&mut parts, "filetype", &q.filetypes, syntax);
        parts.extend(
            q.excluded_filetypes
                .iter()
                .map(|f| format!("-filetype:{f}")),
        );
    }
    if syntax.date_range {
        if let Some(after) = &q.after {
            parts.push(format!("after:{after}"));
        }
        if let Some(before) = &q.before {
            parts.push(format!("before:{before}"));
        }
    }
    let result = parts.join(" ").trim().to_string();
    if !result.is_empty() {
        return result;
    }
    let fallback: Vec<&str> = q
        .sites
        .iter()
        .chain(&q.in_title)
        .chain(&q.in_url)
        .chain(&q.in_text)
        .chain(&q.filetypes)
        .map(String::as_str)
        .collect();
    let fallback = fallback.join(" ").trim().to_string();
    if fallback.is_empty() {
        q.raw.trim().to_string()
    } else {
        fallback
    }
}

/// The query for a credential-free HTML engine: path-carrying `site:` and
/// every `inurl:` value zero-match across the scraper set, so they become
/// plain keywords (the post-filter enforces them); negated forms pass through.
pub(crate) fn format_scraper_query(parsed: &StructuredQuery, syntax: QuerySyntax) -> String {
    if !parsed.has_directives {
        return parsed.raw.clone();
    }
    let mut downgraded = parsed.clone();
    let demoted: Vec<String> = parsed
        .sites
        .iter()
        .filter(|site| site.contains('/'))
        .chain(&parsed.in_url)
        .cloned()
        .collect();
    downgraded.sites.retain(|site| !site.contains('/'));
    downgraded.in_url.clear();
    downgraded
        .terms
        .extend(demoted.iter().map(|text| QueryTerm::plain(text)));
    format_query(&downgraded, syntax)
}

fn host_and_path(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    Some((parsed.host_str()?.to_lowercase(), parsed.path().to_string()))
}

/// `site:` matcher: exact host or subdomain; a path must prefix the URL path.
pub(crate) fn matches_site(url: &str, site: &str) -> bool {
    let Some((host, path)) = host_and_path(url) else {
        return false;
    };
    let (site_host, site_path) = site.find('/').map_or((site, ""), |at| site.split_at(at));
    if host != site_host && !host.ends_with(&format!(".{site_host}")) {
        return false;
    }
    site_path.is_empty() || path.to_lowercase().starts_with(&site_path.to_lowercase())
}

fn matches_filetype(url: &str, ext: &str) -> bool {
    host_and_path(url).is_some_and(|(_, path)| path.to_lowercase().ends_with(&format!(".{ext}")))
}

type Predicate<'a> = Box<dyn Fn(&SearchSource) -> bool + 'a>;

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn constraint_dimensions(q: &StructuredQuery) -> Vec<(String, Predicate<'_>)> {
    let mut dims: Vec<(String, Predicate<'_>)> = Vec::new();
    let label = |prefix: &str, values: &[String], sep: &str| {
        values
            .iter()
            .map(|v| format!("{prefix}:{v}"))
            .collect::<Vec<_>>()
            .join(sep)
    };
    if !q.sites.is_empty() {
        dims.push((
            label("site", &q.sites, " OR "),
            Box::new(|src| q.sites.iter().any(|site| matches_site(&src.url, site))),
        ));
    }
    if !q.excluded_sites.is_empty() {
        dims.push((
            label("-site", &q.excluded_sites, " "),
            Box::new(|src| {
                !q.excluded_sites
                    .iter()
                    .any(|site| matches_site(&src.url, site))
            }),
        ));
    }
    if !q.in_url.is_empty() {
        dims.push((
            label("inurl", &q.in_url, " "),
            Box::new(|src| q.in_url.iter().all(|v| contains_ci(&src.url, v))),
        ));
    }
    if !q.excluded_in_url.is_empty() {
        dims.push((
            label("-inurl", &q.excluded_in_url, " "),
            Box::new(|src| !q.excluded_in_url.iter().any(|v| contains_ci(&src.url, v))),
        ));
    }
    if !q.in_title.is_empty() {
        dims.push((
            label("intitle", &q.in_title, " "),
            Box::new(|src| q.in_title.iter().all(|v| contains_ci(&src.title, v))),
        ));
    }
    if !q.excluded_in_title.is_empty() {
        dims.push((
            label("-intitle", &q.excluded_in_title, " "),
            Box::new(|src| {
                !q.excluded_in_title
                    .iter()
                    .any(|v| contains_ci(&src.title, v))
            }),
        ));
    }
    if !q.filetypes.is_empty() {
        dims.push((
            label("filetype", &q.filetypes, " OR "),
            Box::new(|src| {
                q.filetypes
                    .iter()
                    .any(|ext| matches_filetype(&src.url, ext))
            }),
        ));
    }
    if !q.excluded_filetypes.is_empty() {
        dims.push((
            label("-filetype", &q.excluded_filetypes, " "),
            Box::new(|src| {
                !q.excluded_filetypes
                    .iter()
                    .any(|ext| matches_filetype(&src.url, ext))
            }),
        ));
    }
    if q.after.is_some() || q.before.is_some() {
        let after = q.after.as_deref().and_then(dates::iso_to_epoch_seconds);
        let before = q.before.as_deref().and_then(dates::iso_to_epoch_seconds);
        let text = [
            q.after.as_ref().map(|a| format!("after:{a}")),
            q.before.as_ref().map(|b| format!("before:{b}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        dims.push((
            text,
            Box::new(move |src| {
                let Some(time) = src
                    .published_date
                    .as_deref()
                    .and_then(dates::published_epoch_seconds)
                else {
                    // Undated: cannot prove a violation.
                    return true;
                };
                after.is_none_or(|after| time >= after) && before.is_none_or(|before| time < before)
            }),
        ));
    }
    dims
}

/// Lenient post-filter: each constraint dimension applies in turn, and one
/// that would eliminate every remaining source is skipped and reported (its
/// directive rendering lands in the second element).
pub(crate) fn apply_query_constraints(
    sources: Vec<SearchSource>,
    q: &StructuredQuery,
) -> (Vec<SearchSource>, Vec<String>) {
    let mut current = sources;
    let mut dropped = Vec::new();
    if current.is_empty() {
        return (current, dropped);
    }
    for (label, predicate) in constraint_dimensions(q) {
        if current.iter().any(&predicate) {
            current.retain(|source| predicate(source));
        } else {
            dropped.push(label);
        }
    }
    (current, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_parse_into_constraints_and_render_per_syntax() {
        let parsed = parse_search_query(
            "rust \"async traits\" -nightly site:https://GitHub.com/rust-lang/ OR site:docs.rs after:2024 filetype:.PDF",
        );
        assert_eq!(parsed.sites, vec!["github.com/rust-lang", "docs.rs"]);
        assert_eq!(parsed.after.as_deref(), Some("2024-01-01"));
        assert_eq!(parsed.filetypes, vec!["pdf"]);
        assert!(parsed.has_directives && parsed.has_constraints());
        assert_eq!(
            format_query(&parsed, GOOGLE_QUERY_SYNTAX),
            "rust \"async traits\" -nightly (site:github.com/rust-lang OR site:docs.rs) filetype:pdf after:2024-01-01"
        );
        assert_eq!(
            format_query(&parsed, QuerySyntax::default()),
            "rust async traits"
        );
        assert_eq!(
            format_scraper_query(&parsed, GOOGLE_QUERY_SYNTAX),
            "rust \"async traits\" -nightly github.com/rust-lang site:docs.rs filetype:pdf after:2024-01-01"
        );
    }

    #[test]
    fn the_post_filter_relaxes_a_dimension_that_would_empty_the_results() {
        let source = |url: &str| SearchSource {
            title: "t".to_string(),
            url: url.to_string(),
            snippet: None,
            published_date: None,
            author: None,
        };
        let parsed = parse_search_query("q site:docs.rs intitle:nomatch");
        let (kept, dropped) = apply_query_constraints(
            vec![source("https://docs.rs/a"), source("https://example.com/b")],
            &parsed,
        );
        assert_eq!(kept, vec![source("https://docs.rs/a")]);
        assert_eq!(dropped, vec!["intitle:nomatch".to_string()]);
    }
}
