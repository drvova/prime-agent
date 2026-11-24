//! What a read gives back for an HTML page beyond the default markdown:
//! `#:format` picks `spider_transformations`' `CommonMark`, plain text, the
//! `html2text` layout, the HTML itself or the page as XML, and `#:transform`
//! runs its readability pass and content filters (main content only, no
//! images, no SVG, a root selector, an exclude selector) first. The default
//! (markdown, no transform) is `web.read`'s own renderer, untouched.
//!
//! spider's HTML parsers recurse with the page's nesting, and a stack
//! overflow aborts the daemon instead of unwinding, so a page nested deeper
//! than [`MAX_NESTING`] elements is refused, and the work runs on its own
//! big-stack thread ([`on_own_thread`]) with its panics caught.

use anyhow::{anyhow, bail};
use serde_json::Value;
use spider_transformations::transformation::content::{
    transform_content_input, ReturnFormat, SelectorConfiguration, TransformConfig, TransformInput,
};
use url::Url;

use super::html;

/// The deepest element nesting a page may have for spider's parsers, and
/// the deepest tree `web.read`'s own renderer builds (it flattens a page
/// nested deeper instead of refusing it).
pub(crate) const MAX_NESTING: usize = 2_000;
/// The stack of a transformation thread: room for [`MAX_NESTING`] levels of
/// every parser's recursion many times over.
const STACK_BYTES: usize = 64 * 1024 * 1024;
/// The longest selector or option text accepted.
pub(crate) const MAX_SELECTOR_CHARS: usize = 1_000;

/// The text a read returns for an HTML page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Format {
    /// `web.read`'s own markdown, with heading anchors.
    #[default]
    Markdown,
    /// spider's `CommonMark`.
    CommonMark,
    /// The page's text, without markup.
    Text,
    /// The text laid out as a terminal browser shows it (`html2text`).
    Html2Text,
    /// The HTML itself (after any transform).
    Html,
    /// The page as well-formed XML.
    Xml,
}

impl Format {
    /// The language a fenced view of this format is labelled with; `None` for
    /// the markdown formats, which are not fenced.
    fn fence(self) -> Option<&'static str> {
        match self {
            Format::Markdown | Format::CommonMark => None,
            Format::Text | Format::Html2Text => Some("text"),
            Format::Html => Some("html"),
            Format::Xml => Some("xml"),
        }
    }
}

/// One of spider's content filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Filter {
    /// Keep the article only (Mozilla's readability algorithm).
    Readability,
    /// Drop the navigation, the first header, the footer and side panels.
    MainContent,
    Images,
    Svg,
    /// Drop scripts, styles, comments and attributes the reader does not need.
    CleanHtml,
}

/// The HTML transformations run before a page is formatted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Transform {
    pub(crate) filters: std::collections::BTreeSet<Filter>,
    /// Keep only the first element matching this CSS selector.
    pub(crate) root: Option<String>,
    /// Drop every element matching this CSS selector.
    pub(crate) exclude: Option<String>,
}

/// How a read shapes an HTML page: its format and the transform first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Shaping {
    pub(crate) format: Format,
    pub(crate) transform: Transform,
}

impl Shaping {
    /// The `format` and `transform` options of a `who` payload.
    ///
    /// # Errors
    ///
    /// A format `spider_transformations` has no output for, a transform key it
    /// does not know, or a selector that is not valid CSS.
    pub(crate) fn parse(data: &Value, who: &str) -> anyhow::Result<Shaping> {
        let format = match data.get("format") {
            None | Some(Value::Null) => Format::Markdown,
            Some(Value::String(format)) => match format.as_str() {
                "markdown" => Format::Markdown,
                "commonmark" => Format::CommonMark,
                "text" => Format::Text,
                "html2text" => Format::Html2Text,
                "html" => Format::Html,
                "xml" => Format::Xml,
                other => bail!(
                    "{who} format must be markdown, commonmark, text, html2text, html or xml; got {other:?}"
                ),
            },
            Some(_) => bail!("{who} format must be a format name"),
        };
        let mut transform = Transform::default();
        match data.get("transform") {
            None | Some(Value::Null) => {}
            Some(Value::Object(options)) => {
                for (key, value) in options {
                    let mut flag = |filter| {
                        match value.as_bool() {
                            Some(true) => transform.filters.insert(filter),
                            Some(false) => transform.filters.remove(&filter),
                            None => bail!("{who} transform {key} must be #t or #f"),
                        };
                        Ok(())
                    };
                    let selector = || -> anyhow::Result<Option<String>> {
                        let selector = value
                            .as_str()
                            .map(str::trim)
                            .filter(|selector| !selector.is_empty())
                            .ok_or_else(|| {
                                anyhow!("{who} transform {key} must be a CSS selector")
                            })?;
                        if selector.chars().count() > MAX_SELECTOR_CHARS
                            || spider_utils::build_selectors_base::<&str, &str, [&str; 1]>(
                                [("check", [selector])].into_iter().collect(),
                            )
                            .css
                            .is_empty()
                        {
                            bail!("{who} transform {key}: {selector:?} is not a CSS selector");
                        }
                        Ok(Some(selector.to_string()))
                    };
                    match key.as_str() {
                        "readability" => flag(Filter::Readability)?,
                        "main-content" => flag(Filter::MainContent)?,
                        "filter-images" => flag(Filter::Images)?,
                        "filter-svg" => flag(Filter::Svg)?,
                        "clean-html" => flag(Filter::CleanHtml)?,
                        "root" => transform.root = selector()?,
                        "exclude" => transform.exclude = selector()?,
                        other => bail!(
                            "{who} transform has no {other:?}; it takes readability, main-content, filter-images, filter-svg, clean-html, root and exclude"
                        ),
                    }
                }
            }
            Some(_) => bail!("{who} transform must be a hash of options"),
        }
        Ok(Shaping { format, transform })
    }

    /// Whether this is the default: `web.read`'s own markdown, untransformed.
    pub(crate) fn is_default(&self) -> bool {
        *self == Shaping::default()
    }
}

/// What shaping made of a page: the rendering its title and links come
/// from, and, for a format other than markdown, the formatted text with the
/// language its view is fenced as (`None` for `CommonMark`).
pub(crate) struct Shaped {
    pub(crate) rendered: html::Rendered,
    pub(crate) formatted: Option<(Option<&'static str>, String)>,
}

/// Shape `page` (HTML read from `url`) as `shaping` asks. Blocking: run it
/// with [`on_own_thread`].
///
/// # Errors
///
/// The page nests deeper than [`MAX_NESTING`] elements.
pub(crate) fn shape(page: &str, url: &Url, shaping: &Shaping) -> anyhow::Result<Shaped> {
    check_nesting(page)?;
    let transform = &shaping.transform;
    let on = |filter| transform.filters.contains(&filter);
    let selectors = SelectorConfiguration {
        root_selector: transform.root.clone(),
        exclude_selector: transform.exclude.clone(),
    };
    let formatted = |format| {
        transform_content_input(
            TransformInput {
                url: Some(url),
                content: page.as_bytes(),
                screenshot_bytes: None,
                encoding: None,
                selector_config: Some(&selectors),
                ignore_tags: None,
            },
            &TransformConfig {
                readability: on(Filter::Readability),
                return_format: format,
                filter_images: on(Filter::Images),
                clean_html: on(Filter::CleanHtml),
                filter_svg: on(Filter::Svg),
                main_content: on(Filter::MainContent),
            },
        )
    };
    let original = html::render_page(page, url);
    Ok(match shaping.format {
        // `web.read`'s markdown of the transformed page, under the page's
        // own title (a root selector may leave the `<title>` out).
        Format::Markdown => {
            let shaped = html::render_page(&formatted(ReturnFormat::Raw), url);
            Shaped {
                rendered: html::Rendered {
                    title: original.title,
                    ..shaped
                },
                formatted: None,
            }
        }
        format @ (Format::CommonMark
        | Format::Text
        | Format::Html2Text
        | Format::Html
        | Format::Xml) => Shaped {
            formatted: Some((
                format.fence(),
                formatted(match format {
                    Format::CommonMark => ReturnFormat::CommonMark,
                    Format::Text => ReturnFormat::Text,
                    Format::Html2Text => ReturnFormat::Html2Text,
                    Format::Xml => ReturnFormat::XML,
                    Format::Html | Format::Markdown => ReturnFormat::Raw,
                }),
            )),
            rendered: original,
        },
    })
}

/// Refuse `page` when it nests deeper than [`MAX_NESTING`] elements.
///
/// # Errors
///
/// The nesting, measured as [`html::parse`] builds the tree.
pub(crate) fn check_nesting(page: &str) -> anyhow::Result<()> {
    let nesting = html::nesting(page);
    if nesting > MAX_NESTING {
        bail!(
            "the page nests {nesting} elements deep, past the {MAX_NESTING} spider's parsers are given; read it without #:format, #:transform or extraction"
        );
    }
    Ok(())
}

/// Run `work` on its own thread with a big stack, in the calling session's
/// scope, and wait for it without blocking the runtime. A panic in `work`
/// is its error. Dropping the returned future (a stopped cell) leaves the
/// thread to finish its bounded work and drop the result.
///
/// # Errors
///
/// The thread could not start, `work` failed, or it panicked.
pub(crate) async fn on_own_thread<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BYTES)
        .spawn(pa_types::session_scope::propagate(move || {
            let outcome =
                pa_types::session_scope::catching_panics(std::panic::AssertUnwindSafe(work))
                    .unwrap_or_else(|_| {
                        Err(anyhow!(
                            "the page could not be processed (the parser failed)"
                        ))
                    });
            let _ = sender.send(outcome);
        }))
        .map_err(|error| anyhow!("{name} thread: {error}"))?;
    receiver
        .await
        .map_err(|_| anyhow!("the {name} thread stopped"))?
}

#[cfg(test)]
mod tests;
