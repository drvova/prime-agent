//! HTML to readable markdown for `web.read`.
//!
//! A tolerant tag-soup tree builder (void and raw-text elements, the implied
//! end tags of `p`/`li`/`dt`/`dd`/`tr`/`td`/`th`/`option`, stray end tags
//! ignored) feeds a renderer that keeps what a reader cites — headings with
//! their anchors (`## Usage {#usage}`: the heading's own id, else the id of
//! the section that opens with it), code blocks with their language, GFM
//! tables, lists, quotes, inline code and links — and drops page chrome:
//! `nav`, `aside`, scripts and styles, forms, hidden and ARIA navigation
//! regions, and, when the page has no `main`/`article`, its header and
//! footer. The renderer reads the page's `main` (or its only `article`) when
//! it has one.

use url::Url;

use super::PageLink;
use crate::web_search::http::decode_entities;

/// Elements with no content.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Elements whose content is raw text up to their end tag.
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp"];
/// Elements never rendered.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    "form", "button", "select", "input", "textarea", "nav", "aside", "dialog", "head", "link",
    "meta",
];
/// ARIA roles of page chrome.
const CHROME_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "dialog",
];
/// Class tokens of page chrome and screen-reader-only text.
const CHROME_CLASSES: &[&str] = &[
    "sidebar",
    "navbar",
    "breadcrumb",
    "breadcrumbs",
    "skip-link",
    "sr-only",
    "visually-hidden",
    "headerlink",
    // arXiv's LaTeXML title footnotes (`\thanks`).
    "ltx_role_thanks",
    // Wikipedia's language menu, section edit links and navigation boxes.
    "vector-dropdown",
    "mw-editsection",
    "navbox",
    "header-anchor",
    "anchorjs-link",
];
/// Elements that start a block (and so end an open paragraph).
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "blockquote",
    "body",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    pub(crate) name: String,
    attrs: Vec<(String, String)>,
    pub(crate) children: Vec<Node>,
}

impl Element {
    fn new(name: &str, attrs: Vec<(String, String)>) -> Self {
        Self {
            name: name.to_string(),
            attrs,
            children: Vec::new(),
        }
    }

    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|classes| classes.split_whitespace().any(|token| token == class))
    }

    /// Every descendant element named `name`, in document order.
    pub(crate) fn find_all<'e>(&'e self, name: &str, found: &mut Vec<&'e Element>) {
        for child in &self.children {
            if let Node::Element(element) = child {
                if element.name == name {
                    found.push(element);
                }
                element.find_all(name, found);
            }
        }
    }

    /// The first descendant matching `matches`, depth first.
    pub(crate) fn find(&self, matches: &dyn Fn(&Element) -> bool) -> Option<&Element> {
        self.children.iter().find_map(|child| match child {
            Node::Element(element) if matches(element) => Some(element),
            Node::Element(element) => element.find(matches),
            Node::Text(_) => None,
        })
    }

    /// The decoded text of every descendant, whitespace kept.
    pub(crate) fn raw_text(&self) -> String {
        let mut text = String::new();
        collect_text(&self.children, &mut text);
        text
    }
}

fn collect_text(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Element(element) if element.name == "br" => out.push('\n'),
            Node::Element(element) => collect_text(&element.children, out),
        }
    }
}

/// Parse `html` into a tree under a synthetic `#root` element.
pub(crate) fn parse(html: &str) -> Element {
    let lower = html.to_ascii_lowercase();
    let mut builder = TreeBuilder {
        stack: vec![Element::new("#root", Vec::new())],
    };
    let mut at = 0;
    while at < html.len() {
        let Some(offset) = html[at..].find('<') else {
            builder.text(&html[at..]);
            break;
        };
        if offset > 0 {
            builder.text(&html[at..at + offset]);
        }
        at += offset;
        let rest = &html[at..];
        if rest.starts_with("<!--") {
            at = lower[at + 4..]
                .find("-->")
                .map_or(html.len(), |end| at + 4 + end + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            at = html[at..].find('>').map_or(html.len(), |end| at + end + 1);
        } else if let Some(name_rest) = rest.strip_prefix("</") {
            let name_len = name_rest
                .find(|ch: char| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
                .unwrap_or(name_rest.len());
            builder.close(&lower[at + 2..at + 2 + name_len]);
            at = html[at..].find('>').map_or(html.len(), |end| at + end + 1);
        } else if rest[1..].starts_with(|ch: char| ch.is_ascii_alphabetic()) {
            let (tag, end) = parse_tag(html, &lower, at);
            at = end;
            if RAW_TEXT.contains(&tag.name.as_str()) && !tag.self_closing {
                let closing = format!("</{}", tag.name);
                let content_end = lower[at..]
                    .find(&closing)
                    .map_or(html.len(), |end| at + end);
                let mut element = Element::new(&tag.name, tag.attrs);
                if !matches!(tag.name.as_str(), "script" | "style") {
                    element
                        .children
                        .push(Node::Text(decode_entities(&html[at..content_end])));
                }
                builder.leaf(element);
                at = html[content_end..]
                    .find('>')
                    .map_or(html.len(), |end| content_end + end + 1);
            } else {
                builder.open(tag);
            }
        } else {
            builder.text("<");
            at += 1;
        }
    }
    builder.finish()
}

struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
}

/// The tag starting at `start` (`<name attrs...>`) and the offset after it.
fn parse_tag(html: &str, lower: &str, start: usize) -> (Tag, usize) {
    let bytes = html.as_bytes();
    let mut at = start + 1;
    let name_start = at;
    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !matches!(bytes[at], b'>' | b'/')
    {
        at += 1;
    }
    let name = lower[name_start..at].to_string();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        match bytes[at] {
            b'>' => {
                at += 1;
                break;
            }
            b'/' => {
                self_closing = true;
                at += 1;
                continue;
            }
            _ => {}
        }
        self_closing = false;
        let key_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'=' | b'>' | b'/')
        {
            at += 1;
        }
        let key = lower[key_start..at].to_string();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = String::new();
        if at < bytes.len() && bytes[at] == b'=' {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if at < bytes.len() && matches!(bytes[at], b'"' | b'\'') {
                let quote = bytes[at];
                let value_start = at + 1;
                let value_end = bytes[value_start..]
                    .iter()
                    .position(|byte| *byte == quote)
                    .map_or(bytes.len(), |end| value_start + end);
                value = decode_entities(&html[value_start..value_end]);
                at = (value_end + 1).min(bytes.len());
            } else {
                let value_start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                value = decode_entities(&html[value_start..at]);
            }
        }
        if key.is_empty() {
            at += 1;
        } else {
            attrs.push((key, value));
        }
    }
    (
        Tag {
            name,
            attrs,
            self_closing,
        },
        at,
    )
}

struct TreeBuilder {
    stack: Vec<Element>,
}

impl TreeBuilder {
    fn text(&mut self, text: &str) {
        let decoded = decode_entities(text);
        let top = self.stack.last_mut().expect("root stays open");
        if let Some(Node::Text(previous)) = top.children.last_mut() {
            previous.push_str(&decoded);
        } else {
            top.children.push(Node::Text(decoded));
        }
    }

    fn leaf(&mut self, element: Element) {
        self.stack
            .last_mut()
            .expect("root stays open")
            .children
            .push(Node::Element(element));
    }

    /// Pop the innermost open `targets` element if it sits above every
    /// `boundaries` element (the implied end tags).
    fn close_implied(&mut self, targets: &[&str], boundaries: &[&str]) {
        let position = self.stack.iter().rposition(|element| {
            targets.contains(&element.name.as_str()) || boundaries.contains(&element.name.as_str())
        });
        if let Some(position) = position {
            if position > 0 && targets.contains(&self.stack[position].name.as_str()) {
                self.pop_to(position);
            }
        }
    }

    fn open(&mut self, tag: Tag) {
        let name = tag.name.as_str();
        if BLOCKS.contains(&name) && self.stack.last().is_some_and(|top| top.name == "p") {
            self.pop_to(self.stack.len() - 1);
        }
        match name {
            "li" => self.close_implied(&["li"], &["ul", "ol", "menu"]),
            "dt" | "dd" => self.close_implied(&["dt", "dd"], &["dl"]),
            "tr" => self.close_implied(&["tr", "td", "th"], &["table", "thead", "tbody", "tfoot"]),
            "td" | "th" => self.close_implied(&["td", "th"], &["tr", "table"]),
            "thead" | "tbody" | "tfoot" => {
                self.close_implied(&["thead", "tbody", "tfoot", "tr", "td", "th"], &["table"]);
            }
            "option" => self.close_implied(&["option"], &["select", "datalist"]),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.close_implied(&["h1", "h2", "h3", "h4", "h5", "h6"], BLOCKS);
            }
            _ => {}
        }
        let element = Element::new(name, tag.attrs);
        if VOID.contains(&name) || tag.self_closing {
            self.leaf(element);
        } else {
            self.stack.push(element);
        }
    }

    fn close(&mut self, name: &str) {
        if let Some(position) = self.stack.iter().rposition(|element| element.name == name) {
            if position > 0 {
                self.pop_to(position);
            }
        }
    }

    /// Close every element from the top down to (and including) `position`.
    fn pop_to(&mut self, position: usize) {
        while self.stack.len() > position {
            let element = self.stack.pop().expect("above the root");
            self.leaf(element);
        }
    }

    fn finish(mut self) -> Element {
        self.pop_to(1);
        self.stack.pop().expect("root")
    }
}

/// A rendered page: its markdown, title, and the links it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rendered {
    pub(crate) title: Option<String>,
    pub(crate) markdown: String,
    pub(crate) links: Vec<PageLink>,
}

/// Render a whole page: the `main`/`article` region when it has one.
pub(crate) fn render_page(html: &str, page_url: &Url) -> Rendered {
    let document = parse(html);
    let base = document
        .find(&|element| element.name == "base" && element.attr("href").is_some())
        .and_then(|element| page_url.join(element.attr("href")?).ok())
        .unwrap_or_else(|| page_url.clone());
    let title = document
        .find(&|element| element.name == "title")
        .map(Element::raw_text)
        .and_then(|title| crate::web_search::http::normalize_text(&title))
        .or_else(|| {
            document
                .find(&|element| element.name == "h1")
                .and_then(|h1| crate::web_search::http::normalize_text(&h1.raw_text()))
        });
    let mut articles = Vec::new();
    document.find_all("article", &mut articles);
    let main =
        document.find(&|element| element.name == "main" || element.attr("role") == Some("main"));
    let (root, chrome) = match (main, articles.as_slice()) {
        (Some(main), _) => (main, Chrome::Kept),
        (None, [only]) => (*only, Chrome::Kept),
        (None, _) => (&document, Chrome::Dropped),
    };
    let mut renderer = Renderer::new(base, page_url.clone(), chrome);
    let markdown = renderer.fragment(&root.children);
    Rendered {
        title,
        markdown,
        links: renderer.links,
    }
}

/// Render an HTML fragment (an API's post body) to markdown.
pub(crate) fn render_fragment(html: &str, page_url: &Url) -> (String, Vec<PageLink>) {
    let fragment = parse(html);
    let mut renderer = Renderer::new(page_url.clone(), page_url.clone(), Chrome::Kept);
    let markdown = renderer.fragment(&fragment.children);
    (markdown, renderer.links)
}

/// How deeply `html` nests elements as [`parse`] builds its tree. The tree
/// is taken apart as it is measured, so even a hostile page's depth never
/// recurses (dropping the tree whole would).
pub(crate) fn nesting(html: &str) -> usize {
    let mut deepest = 0;
    let mut open = vec![(parse(html), 0_usize)];
    while let Some((mut element, depth)) = open.pop() {
        deepest = deepest.max(depth);
        for child in std::mem::take(&mut element.children) {
            match child {
                Node::Element(child) => open.push((child, depth + 1)),
                Node::Text(_) => {}
            }
        }
    }
    deepest
}

/// `html` as well-formed XML for an `XPath` query, without recursion: one
/// root element (the page's `html`, else an `html` around everything),
/// element and attribute names that XML cannot carry (a namespace prefix,
/// an odd character) left out with their children kept, the first of a
/// repeated attribute kept, and text escaped. Script and style content is
/// not kept ([`parse`] drops it).
pub(crate) fn to_xml(html: &str) -> String {
    fn xml_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    }
    fn escape(text: &str, out: &mut String) {
        for ch in text.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                // Characters XML 1.0 has no form for.
                '\u{0}'..='\u{8}'
                | '\u{b}'
                | '\u{c}'
                | '\u{e}'..='\u{1f}'
                | '\u{fffe}'
                | '\u{ffff}' => {}
                other => out.push(other),
            }
        }
    }
    enum Step<'e> {
        Node(&'e Node),
        Close(&'e str),
    }
    let root = parse(html);
    let elements: Vec<&Element> = root
        .children
        .iter()
        .filter_map(|child| match child {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
        .collect();
    let whitespace_only = root.children.iter().all(|child| match child {
        Node::Element(_) => true,
        Node::Text(text) => text.trim().is_empty(),
    });
    let mut out = String::with_capacity(html.len());
    let mut steps: Vec<Step> = Vec::new();
    if !(whitespace_only && matches!(elements.as_slice(), [only] if only.name == "html")) {
        out.push_str("<html>");
        steps.push(Step::Close("html"));
    }
    steps.extend(root.children.iter().rev().map(Step::Node));
    while let Some(step) = steps.pop() {
        match step {
            Step::Close(name) => {
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
            Step::Node(Node::Text(text)) => escape(text, &mut out),
            Step::Node(Node::Element(element)) => {
                if xml_name(&element.name) {
                    out.push('<');
                    out.push_str(&element.name);
                    let mut seen = std::collections::HashSet::new();
                    for (key, value) in &element.attrs {
                        if xml_name(key) && seen.insert(key.as_str()) {
                            out.push(' ');
                            out.push_str(key);
                            out.push_str("=\"");
                            escape(value, &mut out);
                            out.push('"');
                        }
                    }
                    out.push('>');
                    steps.push(Step::Close(&element.name));
                }
                steps.extend(element.children.iter().rev().map(Step::Node));
            }
        }
    }
    out
}

/// Whether page headers and footers render: kept inside a `main`/`article`
/// region (an article's own header holds its title), dropped page-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chrome {
    Kept,
    Dropped,
}

struct Renderer {
    base: Url,
    page: Url,
    chrome: Chrome,
    links: Vec<PageLink>,
    /// The id of the section that opened last, until text renders: a
    /// heading without its own id takes it.
    pending_anchor: Option<String>,
}

impl Renderer {
    fn new(base: Url, page: Url, chrome: Chrome) -> Self {
        Self {
            base,
            page,
            chrome,
            links: Vec::new(),
            pending_anchor: None,
        }
    }

    fn fragment(&mut self, nodes: &[Node]) -> String {
        let mut blocks = Vec::new();
        self.blocks(nodes, &mut blocks);
        let joined = blocks.join("\n\n");
        let mut out = String::with_capacity(joined.len());
        let mut blank_run = 0;
        let mut fence: Option<&str> = None;
        for line in joined.lines() {
            let line = line.trim_end();
            let marker = line.trim_start();
            if let Some(open) = fence {
                if marker.starts_with(open) && marker.trim_start_matches('`').is_empty() {
                    fence = None;
                }
            } else if marker.starts_with("```") {
                fence = Some(&marker[..marker.len() - marker.trim_start_matches('`').len()]);
            }
            if line.is_empty() && fence.is_none() {
                blank_run += 1;
                if blank_run > 1 {
                    continue;
                }
            } else {
                blank_run = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.trim().to_string()
    }

    fn skipped(&self, element: &Element) -> bool {
        SKIPPED.contains(&element.name.as_str())
            || element.attr("hidden").is_some()
            || element.attr("aria-hidden") == Some("true")
            || element
                .attr("role")
                .is_some_and(|role| CHROME_ROLES.contains(&role))
            || CHROME_CLASSES.iter().any(|class| element.has_class(class))
            || (self.chrome == Chrome::Dropped
                && matches!(element.name.as_str(), "header" | "footer"))
    }

    /// Render `nodes` as blocks: runs of inline content become paragraphs.
    fn blocks(&mut self, nodes: &[Node], out: &mut Vec<String>) {
        let mut inline = String::new();
        for node in nodes {
            match node {
                Node::Text(text) => push_collapsed(&mut inline, text),
                Node::Element(element) if self.skipped(element) => {}
                Node::Element(element) if BLOCKS.contains(&element.name.as_str()) => {
                    self.flush(&mut inline, out);
                    self.block(element, out);
                }
                Node::Element(element) => {
                    let rendered = self.inline(element);
                    inline.push_str(&rendered);
                }
            }
        }
        self.flush(&mut inline, out);
    }

    fn flush(&mut self, inline: &mut String, out: &mut Vec<String>) {
        let paragraph: Vec<&str> = inline
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if !paragraph.is_empty() {
            out.push(paragraph.join("\n"));
            self.pending_anchor = None;
        }
        inline.clear();
    }

    fn block(&mut self, element: &Element, out: &mut Vec<String>) {
        let name = element.name.as_str();
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                let text = self.inline_children(&element.children);
                let mut text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                // A heading that is one permalink to itself (`[Usage](#usage)`)
                // shows its label.
                if let Some(label) = text
                    .strip_prefix('[')
                    .and_then(|rest| rest.split_once("](#"))
                    .filter(|(label, target)| target.ends_with(')') && !label.contains(']'))
                    .map(|(label, _)| label.to_string())
                {
                    text = label;
                }
                if text.is_empty() {
                    return;
                }
                let anchor = element
                    .attr("id")
                    .map(str::to_string)
                    .or_else(|| {
                        element
                            .find(&|inner| {
                                inner.name == "a"
                                    && (inner.attr("id").is_some() || inner.attr("name").is_some())
                            })
                            .and_then(|inner| inner.attr("id").or_else(|| inner.attr("name")))
                            .map(str::to_string)
                    })
                    .or_else(|| self.pending_anchor.take())
                    .filter(|anchor| !anchor.is_empty() && !anchor.contains(char::is_whitespace));
                self.pending_anchor = None;
                let suffix = anchor
                    .map(|anchor| format!(" {{#{anchor}}}"))
                    .unwrap_or_default();
                out.push(format!("{} {text}{suffix}", "#".repeat(level)));
            }
            "pre" => {
                let code = element.raw_text();
                let code = code.strip_prefix('\n').unwrap_or(&code).trim_end();
                if code.trim().is_empty() {
                    return;
                }
                let language = code_language(element)
                    .or_else(|| {
                        element
                            .find(&|inner| inner.name == "code")
                            .and_then(code_language)
                    })
                    .unwrap_or_default();
                let fence = fence_for(code);
                out.push(format!("{fence}{language}\n{code}\n{fence}"));
                self.pending_anchor = None;
            }
            "ul" | "ol" => {
                let list = self.list(element);
                if !list.is_empty() {
                    out.push(list);
                    self.pending_anchor = None;
                }
            }
            "table" => self.table(element, out),
            "blockquote" => {
                let mut inner = Vec::new();
                self.blocks(&element.children, &mut inner);
                let quoted: Vec<String> = inner
                    .join("\n\n")
                    .lines()
                    .map(|line| {
                        if line.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {line}")
                        }
                    })
                    .collect();
                if !quoted.is_empty() {
                    out.push(quoted.join("\n"));
                }
            }
            "hr" => out.push("---".to_string()),
            "dt" => {
                let text = self.inline_children(&element.children);
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if text.is_empty() {
                    return;
                }
                // An anchored term (an API entry: Sphinx's `dt id="asyncio.gather"`)
                // is a level-6 heading, so its anchor reads its span.
                match element
                    .attr("id")
                    .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace))
                {
                    Some(id) => out.push(format!("###### {text} {{#{id}}}")),
                    None => out.push(format!("**{text}**")),
                }
                self.pending_anchor = None;
            }
            _ => {
                if let Some(id) = element.attr("id").filter(|id| !id.is_empty()) {
                    self.pending_anchor = Some(id.to_string());
                }
                self.blocks(&element.children, out);
            }
        }
    }

    fn list(&mut self, list: &Element) -> String {
        let ordered = list.name == "ol";
        let mut number: i64 = list
            .attr("start")
            .and_then(|start| start.trim().parse().ok())
            .unwrap_or(1);
        let mut items = Vec::new();
        for child in &list.children {
            let Node::Element(item) = child else {
                continue;
            };
            if self.skipped(item) {
                continue;
            }
            let mut blocks = Vec::new();
            if item.name == "li" {
                self.blocks(&item.children, &mut blocks);
            } else {
                self.block(item, &mut blocks);
            }
            let body = blocks.join("\n");
            if body.trim().is_empty() {
                continue;
            }
            let marker = if ordered {
                let marker = format!("{number}.");
                number += 1;
                marker
            } else {
                "-".to_string()
            };
            let indent = " ".repeat(marker.len() + 1);
            let mut lines = body.lines();
            let mut rendered = format!("{marker} {}", lines.next().unwrap_or_default());
            for line in lines {
                rendered.push('\n');
                if !line.is_empty() {
                    rendered.push_str(&indent);
                    rendered.push_str(line);
                }
            }
            items.push(rendered);
        }
        items.join("\n")
    }

    fn table(&mut self, table: &Element, out: &mut Vec<String>) {
        let mut rows: Vec<&Element> = Vec::new();
        collect_rows(table, &mut rows);
        let layout = rows.iter().any(|row| {
            row.find(&|inner| {
                matches!(
                    inner.name.as_str(),
                    "table" | "pre" | "ul" | "ol" | "h1" | "h2" | "h3"
                )
            })
            .is_some()
        });
        let widest = rows.iter().map(|row| cells(row).len()).max().unwrap_or(0);
        if layout || widest < 2 {
            // A layout table (or one column): its cells are just blocks.
            self.blocks(&table.children, out);
            return;
        }
        let mut lines = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let mut texts: Vec<String> = cells(row)
                .iter()
                .map(|cell| {
                    self.inline_children(&cell.children)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace('|', "\\|")
                })
                .collect();
            texts.resize(widest, String::new());
            lines.push(format!("| {} |", texts.join(" | ")));
            if index == 0 {
                lines.push(format!("|{}", " --- |".repeat(widest)));
            }
        }
        if !lines.is_empty() {
            if let Some(caption) = table.find(&|inner| inner.name == "caption") {
                let caption = self.inline_children(&caption.children);
                if !caption.trim().is_empty() {
                    out.push(caption.trim().to_string());
                }
            }
            out.push(lines.join("\n"));
            self.pending_anchor = None;
        }
    }

    fn inline_children(&mut self, nodes: &[Node]) -> String {
        let mut text = String::new();
        for node in nodes {
            match node {
                Node::Text(raw) => push_collapsed(&mut text, raw),
                Node::Element(element) if self.skipped(element) => {}
                Node::Element(element) => {
                    let rendered = self.inline(element);
                    text.push_str(&rendered);
                }
            }
        }
        text
    }

    fn inline(&mut self, element: &Element) -> String {
        match element.name.as_str() {
            "a" => {
                let text = self.inline_children(&element.children);
                let label = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let Some(href) = element
                    .attr("href")
                    .map(str::trim)
                    .filter(|href| !href.is_empty())
                else {
                    return text;
                };
                if href.starts_with("javascript:") {
                    return text;
                }
                let Ok(target) = self.base.join(href) else {
                    return text;
                };
                if !matches!(target.scheme(), "http" | "https") {
                    return text;
                }
                if !label.is_empty() && !self.links.iter().any(|link| link.url == target.as_str()) {
                    self.links.push(PageLink {
                        text: label.clone(),
                        url: target.to_string(),
                    });
                }
                if label.is_empty() {
                    return String::new();
                }
                let same_page = {
                    let mut page = self.page.clone();
                    page.set_fragment(None);
                    let mut bare = target.clone();
                    bare.set_fragment(None);
                    page == bare
                };
                let shown = match (same_page, target.fragment()) {
                    (true, Some(fragment)) => format!("#{fragment}"),
                    _ => target.to_string(),
                };
                format!("[{label}]({shown})")
            }
            "code" | "kbd" | "samp" | "tt" => {
                let code = element.raw_text();
                let code = code.split_whitespace().collect::<Vec<_>>().join(" ");
                if code.is_empty() {
                    return String::new();
                }
                let ticks = "`".repeat(longest_run(&code, '`') + 1);
                let pad = if code.starts_with('`') || code.ends_with('`') {
                    " "
                } else {
                    ""
                };
                format!("{ticks}{pad}{code}{pad}{ticks}")
            }
            "strong" | "b" => wrap(&self.inline_children(&element.children), "**"),
            "em" | "i" => wrap(&self.inline_children(&element.children), "*"),
            "del" | "s" | "strike" => wrap(&self.inline_children(&element.children), "~~"),
            "br" => "\n".to_string(),
            "img" => match (element.attr("alt").map(str::trim), element.attr("src")) {
                (Some(alt), Some(src)) if !alt.is_empty() => match self.base.join(src) {
                    Ok(src) if matches!(src.scheme(), "http" | "https") => {
                        format!("![{alt}]({src})")
                    }
                    _ => alt.to_string(),
                },
                _ => String::new(),
            },
            "math" => match element.attr("alttext") {
                Some(tex) if !tex.trim().is_empty() => format!("${}$", tex.trim()),
                _ => element.raw_text(),
            },
            _ => self.inline_children(&element.children),
        }
    }
}

/// Append `text` with whitespace runs collapsed to one space.
fn push_collapsed(out: &mut String, text: &str) {
    let mut last_space = out.ends_with(|ch: char| ch.is_whitespace()) || out.is_empty();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
}

fn wrap(text: &str, marker: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }
    let lead = if text.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if text.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    format!("{lead}{marker}{trimmed}{marker}{trail}")
}

fn longest_run(text: &str, ch: char) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for current in text.chars() {
        if current == ch {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// A code fence longer than any backtick run inside `code`.
pub(crate) fn fence_for(code: &str) -> String {
    "`".repeat(longest_run(code, '`').max(2) + 1)
}

/// `language-rust`, `lang-rust`, `highlight-source-rust`, or `brush: rust`.
fn code_language(element: &Element) -> Option<String> {
    let classes = element.attr("class")?;
    classes.split_whitespace().find_map(|class| {
        ["language-", "lang-", "highlight-source-"]
            .iter()
            .find_map(|prefix| class.strip_prefix(prefix))
            .filter(|language| {
                !language.is_empty()
                    && language
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '#' | '-' | '_'))
            })
            .map(str::to_ascii_lowercase)
    })
}

/// The table's rows, not descending into nested tables.
fn collect_rows<'e>(element: &'e Element, rows: &mut Vec<&'e Element>) {
    for child in &element.children {
        if let Node::Element(child) = child {
            match child.name.as_str() {
                "tr" => rows.push(child),
                "thead" | "tbody" | "tfoot" => collect_rows(child, rows),
                _ => {}
            }
        }
    }
}

fn cells(row: &Element) -> Vec<&Element> {
    row.children
        .iter()
        .filter_map(|child| match child {
            Node::Element(cell) if matches!(cell.name.as_str(), "td" | "th") => Some(cell),
            Node::Element(_) | Node::Text(_) => None,
        })
        .collect()
}
