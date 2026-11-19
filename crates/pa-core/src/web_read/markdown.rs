//! The markdown side of `web.read`: the outline of headings and their
//! anchors, one heading's span (`section=`), a line range of a code view
//! (`L10-L20`, GitHub's line fragment), and the line-boundary truncation.

use serde::Serialize;
use url::Url;

use super::PageLink;

/// Where a heading's anchor comes from when it carries no explicit `{#id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorStyle {
    /// Only explicit `{#id}` anchors are real (rendered HTML, PDF pages);
    /// other headings still get a slug to select them by, but no URL.
    Explicit,
    /// The document is markdown its host renders with GitHub-style heading
    /// slugs (a README, a `.md` file, an issue body): every slug is real.
    GithubSlugs,
}

/// One heading of the outline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Heading {
    pub(crate) level: u8,
    pub(crate) text: String,
    /// What `section=` takes (the page's own id, else a GitHub-style slug).
    pub(crate) anchor: String,
    /// `final_url#anchor`, when the anchor exists on the page itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// The heading's 0-based line in the full markdown.
    #[serde(skip)]
    pub(crate) line: usize,
}

/// A heading line: its level, its text, and an explicit `{#id}` suffix.
fn heading_line(line: &str) -> Option<(u8, &str, Option<&str>)> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some(open) = rest.rfind(" {#") {
        if let Some(id) = rest[open + 3..].strip_suffix('}') {
            if !id.is_empty() && !id.contains(char::is_whitespace) {
                return Some((
                    u8::try_from(hashes).ok()?,
                    rest[..open].trim_end(),
                    Some(id),
                ));
            }
        }
    }
    Some((u8::try_from(hashes).ok()?, rest, None))
}

/// Each line's index paired with whether it sits inside a code fence (the
/// fence lines included), and the fence still open after the last line.
fn fenced_lines(markdown: &str) -> (Vec<(usize, &str, bool)>, Option<String>) {
    let mut fence: Option<String> = None;
    let lines = markdown
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let marker = line.trim_start();
            let run = marker.len() - marker.trim_start_matches(['`', '~']).len();
            let inside = match &fence {
                Some(open) => {
                    if marker.starts_with(open.as_str()) && marker[run..].trim().is_empty() {
                        fence = None;
                    }
                    true
                }
                None if run >= 3 && (marker.starts_with("```") || marker.starts_with("~~~")) => {
                    fence = Some(marker[..run].to_string());
                    true
                }
                None => false,
            };
            (index, line, inside)
        })
        .collect();
    (lines, fence)
}

/// `markdown` with every heading outside code fences pushed `by` levels
/// down (capped at 6): a post body nested under its own section heading.
pub(crate) fn demote_headings(markdown: &str, by: usize) -> String {
    let (lines, _) = fenced_lines(markdown);
    lines
        .into_iter()
        .map(|(_, line, fenced)| match heading_line(line) {
            Some((level, _, _)) if !fenced => {
                let level = usize::from(level);
                format!("{}{}", "#".repeat((level + by).min(6)), &line[level..])
            }
            Some(_) | None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Relative `[label](target)` links outside code fences resolved against
/// `base` (a README's `GUIDE.md` becomes a URL `web.read` can follow), and
/// every http(s) link collected (images excluded).
pub(crate) fn resolve_links(markdown: &str, base: &Url) -> (String, Vec<PageLink>) {
    let (lines, _) = fenced_lines(markdown);
    let mut found: Vec<PageLink> = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    for (_, line, fenced) in lines {
        if fenced || !line.contains("](") {
            out.push(line.to_string());
            continue;
        }
        let mut rendered = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(at) = rest.find("](") {
            let (before, after) = rest.split_at(at);
            let target_end = after[2..].find(')').map_or(after.len(), |end| end + 2);
            let target = &after[2..target_end];
            let (href, title) = target.split_once(' ').unwrap_or((target, ""));
            let label_start = before.rfind('[');
            let image = label_start.is_some_and(|start| before[..start].ends_with('!'));
            let resolved = match Url::parse(href) {
                Ok(absolute) => Some(absolute),
                Err(_) if href.is_empty() || href.starts_with('#') => None,
                Err(_) => base.join(href).ok(),
            };
            rendered.push_str(before);
            rendered.push_str("](");
            match &resolved {
                Some(url) if !href.contains("://") => {
                    rendered.push_str(url.as_str());
                    if !title.is_empty() {
                        rendered.push(' ');
                        rendered.push_str(title);
                    }
                }
                Some(_) | None => rendered.push_str(target),
            }
            if let (Some(url), Some(start), false) = (&resolved, label_start, image) {
                let label = before[start + 1..].trim();
                if matches!(url.scheme(), "http" | "https")
                    && !label.is_empty()
                    && !found.iter().any(|link| link.url == url.as_str())
                {
                    found.push(PageLink {
                        text: strip_inline_markup(label),
                        url: url.to_string(),
                    });
                }
            }
            rest = &after[target_end.min(after.len())..];
        }
        rendered.push_str(rest);
        out.push(rendered);
    }
    (out.join("\n"), found)
}

/// GitHub's heading slug: lowercased, punctuation dropped, spaces to `-`.
pub(crate) fn github_slug(text: &str) -> String {
    let plain = strip_inline_markup(text);
    plain
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|ch| match ch {
            ' ' => Some('-'),
            '-' | '_' => Some(ch),
            _ if ch.is_alphanumeric() => Some(ch),
            _ => None,
        })
        .collect()
}

/// `[label](url)` to `label`, and emphasis/code markers dropped.
fn strip_inline_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some((close, end)) = after.find("](").and_then(|close| {
            after[close + 2..]
                .find(')')
                .map(|end| (close, close + 2 + end + 1))
        }) {
            out.push_str(&after[..close]);
            rest = &after[end..];
        } else {
            out.push('[');
            rest = after;
        }
    }
    out.push_str(rest);
    out.replace(['*', '`'], "")
}

/// The headings of `markdown`, outside code fences, with their anchors.
pub(crate) fn outline(markdown: &str, style: AnchorStyle, page_url: &Url) -> Vec<Heading> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    fenced_lines(markdown)
        .0
        .into_iter()
        .filter(|(_, _, fenced)| !fenced)
        .filter_map(|(line, text, _)| {
            let (level, title, explicit) = heading_line(text)?;
            let (anchor, real) = if let Some(id) = explicit {
                (id.to_string(), true)
            } else {
                let slug = github_slug(title);
                let count = seen.entry(slug.clone()).or_insert(0);
                let anchor = if *count == 0 {
                    slug
                } else {
                    format!("{slug}-{count}")
                };
                *count += 1;
                (anchor, style == AnchorStyle::GithubSlugs)
            };
            let url = real.then(|| {
                let mut url = page_url.clone();
                url.set_fragment(Some(&anchor));
                url.to_string()
            });
            Some(Heading {
                level,
                text: strip_inline_markup(title),
                anchor,
                url,
                line,
            })
        })
        .collect()
}

/// The span of the heading `section` names (its anchor, `#anchor`, or its
/// text, case-insensitively): from the heading to the next heading of the
/// same or a higher level.
pub(crate) fn select_section<'h>(
    markdown: &str,
    outline: &'h [Heading],
    section: &str,
) -> Option<(String, &'h Heading)> {
    let wanted = section.trim().trim_start_matches('#').to_lowercase();
    let index = outline
        .iter()
        .position(|heading| heading.anchor.to_lowercase() == wanted)
        .or_else(|| {
            outline
                .iter()
                .position(|heading| heading.text.to_lowercase() == wanted)
        })?;
    let heading = &outline[index];
    let end = outline[index + 1..]
        .iter()
        .find(|next| next.level <= heading.level)
        .map_or(usize::MAX, |next| next.line);
    let span: Vec<&str> = markdown
        .lines()
        .skip(heading.line)
        .take(end.saturating_sub(heading.line))
        .collect();
    Some((span.join("\n").trim_end().to_string(), heading))
}

/// `L10`, `L10-L20`, or `L10-20`: a 1-based inclusive line range.
pub(crate) fn line_range(section: &str) -> Option<(usize, usize)> {
    let spec = section.trim().trim_start_matches('#');
    let spec = spec.strip_prefix('L').or_else(|| spec.strip_prefix('l'))?;
    let (start, end) = match spec.split_once('-') {
        Some((start, end)) => (start, end.trim_start_matches(['L', 'l'])),
        None => (spec, spec),
    };
    let start: usize = start.parse().ok()?;
    let end: usize = end.parse().ok()?;
    (start >= 1 && end >= start).then_some((start, end))
}

/// At most `max_chars` chars, cut at a line boundary where one is near, with
/// an open code fence closed; `true` when anything was cut.
pub(crate) fn truncate(markdown: &str, max_chars: usize) -> (String, bool) {
    let Some((cut, _)) = markdown.char_indices().nth(max_chars) else {
        return (markdown.to_string(), false);
    };
    let head = &markdown[..cut];
    let head = match head.rfind('\n') {
        Some(newline) if newline >= cut / 2 && !markdown[cut..].starts_with('\n') => {
            &head[..newline]
        }
        _ => head,
    };
    let mut out = head.trim_end().to_string();
    if let (_, Some(fence)) = fenced_lines(&out) {
        out.push('\n');
        out.push_str(&fence);
    }
    (out, true)
}
