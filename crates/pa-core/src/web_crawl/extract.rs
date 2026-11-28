//! Extraction with spider's selectors (`spider_utils`): a map of names to
//! CSS selectors or `XPath` expressions, and for each name the values its
//! selectors found on a page. The kernel's `(web-read source #:select
//! selectors ...)` reaches it through the `web.extract` host request, and
//! `(web-crawl ... #:extract selectors)` runs it on every HTML page.
//!
//! A CSS match gives the element's text, or for `meta` its `content`, for
//! `link` its `href`, for `script` its `src`, and for `img`/`source` its
//! `[src]("alt")`. An `XPath` expression gives the string value of what it
//! selects first (`//a/@href`, `string(//h1)`, `count(//li)`); the page is
//! first made well-formed XML ([`crate::web_read::to_xml`]), since spider's
//! `XPath` reads XML only. A selector that is neither is refused before
//! anything runs (spider itself skips it with a log line).
//!
//! Bounded: at most [`MAX_NAMES`] names of [`MAX_SELECTORS`] selectors each,
//! [`MAX_VALUES`] values per name, each value cut at [`MAX_VALUE_CHARS`], and
//! past [`TOTAL_CHARS`] the extraction fails with a note to narrow it. The
//! work runs on its own big-stack thread, after the page's nesting check.

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};
use spider::hashbrown::HashMap;
use spider_utils::DocumentSelectors;

use crate::web_read::{check_nesting, MAX_SELECTOR_CHARS};

/// Names one extraction may ask for.
const MAX_NAMES: usize = 100;
/// Selectors one name may list.
const MAX_SELECTORS: usize = 20;
/// Values kept per name.
const MAX_VALUES: usize = 1_000;
/// Characters kept of one value.
const MAX_VALUE_CHARS: usize = 10_000;
/// Characters all values of one page hold together.
pub(crate) const TOTAL_CHARS: usize = 400_000;

/// What to extract: each name and its selectors, every selector checked to
/// be CSS or `XPath`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Selectors {
    entries: Vec<(String, Vec<String>)>,
}

impl Selectors {
    /// The selectors of a `who` payload's `key` option: a map of names to a
    /// selector or a list of them; `None` when the option is absent.
    ///
    /// # Errors
    ///
    /// A map of the wrong shape, past the bounds, or a selector that is
    /// neither CSS nor `XPath` (named in the error).
    pub(crate) fn parse(data: &Value, key: &str, who: &str) -> anyhow::Result<Option<Selectors>> {
        let entries = match data.get(key) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Object(entries)) => entries,
            Some(_) => bail!("{who} {key} must map names to CSS selectors or XPath expressions"),
        };
        if entries.is_empty() || entries.len() > MAX_NAMES {
            bail!("{who} {key} takes 1 to {MAX_NAMES} names");
        }
        let entries = entries
            .iter()
            .map(|(name, selectors)| {
                let selectors: Vec<String> = match selectors {
                    Value::String(one) => vec![one.trim().to_string()],
                    Value::Array(many) => many
                        .iter()
                        .map(|one| {
                            one.as_str().map(|one| one.trim().to_string()).ok_or_else(|| {
                                anyhow!("{who} {key} {name:?} takes selector strings")
                            })
                        })
                        .collect::<anyhow::Result<_>>()?,
                    _ => bail!("{who} {key} {name:?} must be a selector or a list of them"),
                };
                if selectors.is_empty() || selectors.len() > MAX_SELECTORS {
                    bail!("{who} {key} {name:?} takes 1 to {MAX_SELECTORS} selectors");
                }
                for selector in &selectors {
                    let built = spider_utils::build_selectors_base::<&str, &str, [&str; 1]>(
                        [("check", [selector.as_str()])].into_iter().collect(),
                    );
                    if selector.is_empty()
                        || selector.chars().count() > MAX_SELECTOR_CHARS
                        || built.css.is_empty() && built.xpath.is_empty()
                    {
                        bail!("{who} {key} {name:?}: {selector:?} is neither a CSS selector nor an XPath expression");
                    }
                }
                Ok((name.clone(), selectors))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Some(Selectors { entries }))
    }

    /// The values each name's selectors find in `page` (HTML): `{name:
    /// [value ...]}`, every name present. Blocking: run it with
    /// [`crate::web_read::on_own_thread`].
    ///
    /// # Errors
    ///
    /// The page nests too deeply, or its values pass [`TOTAL_CHARS`].
    pub(crate) fn extract(&self, page: &str) -> anyhow::Result<Value> {
        check_nesting(page)?;
        let built = spider_utils::build_selectors_base::<String, &str, Vec<&str>>(
            self.entries
                .iter()
                .map(|(name, selectors)| {
                    (name.clone(), selectors.iter().map(String::as_str).collect())
                })
                .collect(),
        );
        let mut found: HashMap<String, Vec<String>> = HashMap::new();
        if !built.css.is_empty() {
            let css = DocumentSelectors {
                css: built.css,
                xpath: HashMap::new(),
            };
            found.extend(spider_utils::css_query_select_map(page, &css));
        }
        if !built.xpath.is_empty() {
            let xpath = DocumentSelectors {
                css: HashMap::new(),
                xpath: built.xpath,
            };
            for (name, mut values) in
                spider_utils::css_query_select_map(&crate::web_read::to_xml(page), &xpath)
            {
                found.entry(name).or_default().append(&mut values);
            }
        }
        let mut chars = 0;
        let mut values = Map::new();
        for (name, _) in &self.entries {
            let kept: Vec<Value> = found
                .remove(name)
                .unwrap_or_default()
                .into_iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .take(MAX_VALUES)
                .map(|value| {
                    let value = match value.char_indices().nth(MAX_VALUE_CHARS) {
                        Some((cut, _)) => format!("{}…", &value[..cut]),
                        None => value,
                    };
                    chars += value.chars().count();
                    json!(value)
                })
                .collect();
            if chars > TOTAL_CHARS {
                bail!(
                    "the selectors found more than {TOTAL_CHARS} chars on the page; narrow them (at {name:?})"
                );
            }
            values.insert(name.clone(), Value::Array(kept));
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests;
