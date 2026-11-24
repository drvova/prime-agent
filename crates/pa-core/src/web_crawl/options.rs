//! The crawl's further options: how far the frontier reaches (path budgets,
//! other hosts of the same crawl, every resource rather than pages,
//! de-duplicated URLs, retries) and, for a rendered crawl, what the browser
//! does on each page (waits, viewport, JavaScript, automation steps and
//! scripts). Each maps onto one spider setting.

use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::Value;
use spider::configuration::{Viewport, WaitForDelay, WaitForIdleNetwork, WaitForSelector};
use spider::features::chrome_common::{RequestInterceptConfiguration, WebAutomation};
use spider::hashbrown::HashMap;
use spider::website::Website;

/// The most pages one budget entry may allow.
const MAX_BUDGET: u64 = 200;
/// The most retries a failed page may get.
const MAX_RETRY: u64 = 3;
/// The longest any one browser wait may be.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// The largest viewport side, in CSS pixels.
const MAX_VIEWPORT: u64 = 8_192;

/// The frontier options.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Reach {
    /// Pages allowed under each path prefix (`"*"` for all paths).
    pub(crate) budget: Vec<(String, u32)>,
    /// Hosts crawled as part of the start site.
    pub(crate) external_domains: Vec<String>,
    /// Keep every resource the pages link (PDFs, JSON, text), not only HTML.
    pub(crate) full_resources: bool,
    /// Treat URLs that differ only in a trailing slash and the like as one.
    pub(crate) normalize: bool,
    pub(crate) retry: u8,
}

/// What the browser does on each page of a rendered crawl.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Browsing {
    /// Wait until the network has been idle (at most this long).
    pub(crate) wait_idle: Option<Duration>,
    /// Wait for a CSS selector to match.
    pub(crate) wait_selector: Option<String>,
    /// Wait for the DOM under a selector to stop changing.
    pub(crate) wait_dom: Option<String>,
    /// Wait a fixed time.
    pub(crate) wait_delay: Option<Duration>,
    pub(crate) viewport: Option<(u32, u32)>,
    /// Keep the pages' own JavaScript from running.
    pub(crate) no_javascript: bool,
    /// Automation steps per URL or path.
    pub(crate) actions: Vec<(String, Vec<WebAutomation>)>,
    /// JavaScript to evaluate per URL or path.
    pub(crate) scripts: Vec<(String, String)>,
    /// The requests the browser refuses to make.
    pub(crate) block: Option<Blocking>,
    /// JavaScript run in every document before the page's own scripts.
    pub(crate) init_script: Option<String>,
    /// The locale the pages see (`de-DE`).
    pub(crate) locale: Option<String>,
    /// The time zone the pages see (`Europe/Berlin`).
    pub(crate) timezone: Option<String>,
}

/// What a rendered crawl's browser refuses to load: kinds of resources,
/// first-party or not, and URLs matching patterns.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Blocking {
    pub(crate) kinds: std::collections::BTreeSet<BlockKind>,
    /// URL patterns (substrings or `*` globs) blocked outright.
    pub(crate) patterns: Vec<String>,
}

/// A kind of resource a rendered crawl can refuse to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BlockKind {
    /// Images, media and fonts.
    Images,
    Stylesheets,
    Scripts,
    /// Analytics and tracking requests.
    Analytics,
    /// Requests spider's ad-block engine (its built-in filter list) matches.
    Ads,
}

impl Blocking {
    /// The `block` option: a list of kinds (`images`, `stylesheets`,
    /// `scripts`, `analytics`, `ads`) and URL patterns.
    ///
    /// # Errors
    ///
    /// An entry that is neither a known kind nor a pattern.
    fn parse(value: &Value) -> anyhow::Result<Blocking> {
        let entries = match value {
            Value::String(one) => vec![Value::String(one.clone())],
            Value::Array(many) => many.clone(),
            _ => bail!("web.crawl block must be a kind, a URL pattern, or a list of them"),
        };
        let mut blocking = Blocking::default();
        for entry in entries {
            let Value::String(entry) = entry else {
                bail!("web.crawl block takes kinds and URL patterns as strings");
            };
            match entry.as_str() {
                "images" => {
                    blocking.kinds.insert(BlockKind::Images);
                }
                "stylesheets" => {
                    blocking.kinds.insert(BlockKind::Stylesheets);
                }
                "scripts" => {
                    blocking.kinds.insert(BlockKind::Scripts);
                }
                "analytics" => {
                    blocking.kinds.insert(BlockKind::Analytics);
                }
                "ads" => {
                    blocking.kinds.insert(BlockKind::Ads);
                }
                pattern if pattern.contains(['/', '.', '*']) => {
                    blocking.patterns.push(pattern.to_string());
                }
                unknown => bail!(
                    "web.crawl block: {unknown:?} is not a kind (images, stylesheets, scripts, analytics, ads) or a URL pattern"
                ),
            }
        }
        Ok(blocking)
    }
}

impl Reach {
    /// The frontier options of a `web.crawl` payload.
    ///
    /// # Errors
    ///
    /// An option of the wrong shape or out of range.
    pub(crate) fn parse(data: &Value) -> anyhow::Result<Reach> {
        let budget = match data.get("budget") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(entries)) => entries
                .iter()
                .map(|(path, pages)| {
                    pages
                        .as_u64()
                        .filter(|pages| (1..=MAX_BUDGET).contains(pages))
                        .and_then(|pages| u32::try_from(pages).ok())
                        .map(|pages| (path.clone(), pages))
                        .ok_or_else(|| {
                            anyhow!("web.crawl budget for {path:?} must be 1 to {MAX_BUDGET} pages")
                        })
                })
                .collect::<anyhow::Result<_>>()?,
            Some(_) => bail!("web.crawl budget must map paths to page counts"),
        };
        let external_domains: Vec<String> = match data.get("externalDomains") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(many)) => many
                .iter()
                .map(|one| {
                    one.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("web.crawl externalDomains takes strings"))
                })
                .collect::<anyhow::Result<_>>()?,
            Some(_) => bail!("web.crawl externalDomains must be a host or a list of them"),
        };
        if let Some(any) = external_domains.iter().find(|domain| domain.contains('*')) {
            bail!("web.crawl externalDomains names hosts; {any:?} is a pattern");
        }
        let retry = match data.get("retry") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .filter(|retry| *retry <= MAX_RETRY)
                .and_then(|retry| u8::try_from(retry).ok())
                .ok_or_else(|| anyhow!("web.crawl retry must be 0 to {MAX_RETRY}"))?,
        };
        Ok(Reach {
            budget,
            external_domains,
            full_resources: flag(data, "fullResources"),
            normalize: flag(data, "normalize"),
            retry,
        })
    }

    /// Set these options on `website`, whose page limit is `max_pages`.
    pub(crate) fn apply(&self, website: &mut Website, max_pages: u32) {
        if !self.budget.is_empty() {
            // spider matches a path's segments joined with nothing between
            // them ("/docs/api" is "docsapi"), and holds the page limit as
            // the "*" budget, which a budget of its own replaces.
            let mut budget: HashMap<String, u32> = self
                .budget
                .iter()
                .map(|(path, pages)| {
                    let key = if path == "*" {
                        path.clone()
                    } else {
                        path.split('/')
                            .filter(|segment| !segment.is_empty())
                            .collect()
                    };
                    (key, *pages)
                })
                .collect();
            let all = budget.entry("*".to_string()).or_insert(max_pages);
            *all = (*all).min(max_pages);
            website.with_budget(Some(
                budget
                    .iter()
                    .map(|(path, pages)| (path.as_str(), *pages))
                    .collect(),
            ));
        }
        if !self.external_domains.is_empty() {
            website.with_external_domains(Some(
                self.external_domains
                    .iter()
                    .map(|domain| format!("https://{}", host_of(domain))),
            ));
        }
        website
            .with_full_resources(self.full_resources)
            .with_normalize(self.normalize)
            .with_retry(self.retry);
    }

    /// Whether `host` is one of the crawl's other hosts.
    pub(crate) fn includes_host(&self, host: &str) -> bool {
        self.external_domains
            .iter()
            .any(|domain| host_of(domain).trim_start_matches("www.") == host)
    }
}

impl Browsing {
    /// The browser options of a `web.crawl` payload; `rendered` says whether
    /// the crawl uses a browser at all.
    ///
    /// # Errors
    ///
    /// A browser option on a crawl without `render`, an option of the wrong
    /// shape or out of range, or an automation step spider does not know.
    pub(crate) fn parse(data: &Value, rendered: bool) -> anyhow::Result<Browsing> {
        const OPTIONS: [&str; 12] = [
            "block",
            "initScript",
            "locale",
            "timezone",
            "waitIdleSeconds",
            "waitSelector",
            "waitDom",
            "waitDelaySeconds",
            "viewport",
            "noJavascript",
            "actions",
            "scripts",
        ];
        if !rendered {
            if let Some(option) = OPTIONS
                .iter()
                .find(|option| data.get(**option).is_some_and(|value| !value.is_null()))
            {
                bail!("web.crawl {option} needs render: the browser does it");
            }
            return Ok(Browsing::default());
        }
        let viewport = match data.get("viewport") {
            None | Some(Value::Null) => None,
            Some(Value::Array(sides)) if sides.len() == 2 => {
                let side = |value: &Value| {
                    value
                        .as_u64()
                        .filter(|side| (1..=MAX_VIEWPORT).contains(side))
                        .and_then(|side| u32::try_from(side).ok())
                };
                match (side(&sides[0]), side(&sides[1])) {
                    (Some(width), Some(height)) => Some((width, height)),
                    (Some(_) | None, None) | (None, Some(_)) => {
                        bail!("web.crawl viewport sides must be 1 to {MAX_VIEWPORT} pixels")
                    }
                }
            }
            Some(_) => bail!("web.crawl viewport must be (width height)"),
        };
        let actions = per_url(data, "actions")?
            .into_iter()
            .map(|(target, steps)| {
                let steps: Vec<WebAutomation> = serde_json::from_value(steps)
                    .map_err(|error| anyhow!("web.crawl actions for {target:?}: {error}"))?;
                // A screenshot step writes where the session works, not where
                // the daemon was started: a relative output is taken under the
                // session's directory.
                let steps = steps
                    .into_iter()
                    .map(|step| match step {
                        WebAutomation::Screenshot {
                            full_page,
                            omit_background,
                            output,
                        } => WebAutomation::Screenshot {
                            full_page,
                            omit_background,
                            output: pa_types::session_scope::current_dir()
                                .map(|cwd| cwd.join(&output).to_string_lossy().into_owned())
                                .unwrap_or(output),
                        },
                        other @ (WebAutomation::Evaluate(_)
                        | WebAutomation::Click(_)
                        | WebAutomation::ClickAll(_)
                        | WebAutomation::ClickPoint { .. }
                        | WebAutomation::ClickHold { .. }
                        | WebAutomation::ClickHoldPoint { .. }
                        | WebAutomation::ClickDrag { .. }
                        | WebAutomation::ClickDragPoint { .. }
                        | WebAutomation::ClickAllClickable()
                        | WebAutomation::Wait(_)
                        | WebAutomation::WaitForNavigation
                        | WebAutomation::WaitForLoad { .. }
                        | WebAutomation::WaitForNetworkIdle { .. }
                        | WebAutomation::WaitForNetworkAlmostIdle { .. }
                        | WebAutomation::WaitForDom { .. }
                        | WebAutomation::WaitFor(_)
                        | WebAutomation::WaitForWithTimeout { .. }
                        | WebAutomation::WaitForAndClick(_)
                        | WebAutomation::ScrollX(_)
                        | WebAutomation::ScrollY(_)
                        | WebAutomation::Fill { .. }
                        | WebAutomation::Type { .. }
                        | WebAutomation::InfiniteScroll(_)
                        | WebAutomation::ValidateChain) => other,
                    })
                    .collect();
                Ok((target, steps))
            })
            .collect::<anyhow::Result<_>>()?;
        let scripts = per_url(data, "scripts")?
            .into_iter()
            .map(|(target, script)| match script {
                Value::String(script) => Ok((target, script)),
                _ => Err(anyhow!("web.crawl scripts for {target:?} must be a string")),
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Browsing {
            wait_idle: seconds(data, "waitIdleSeconds")?,
            wait_selector: text(data, "waitSelector"),
            wait_dom: text(data, "waitDom"),
            wait_delay: seconds(data, "waitDelaySeconds")?,
            viewport,
            no_javascript: flag(data, "noJavascript"),
            actions,
            scripts,
            block: match data.get("block") {
                None | Some(Value::Null) => None,
                Some(value) => Some(Blocking::parse(value)?),
            },
            init_script: text(data, "initScript"),
            locale: text(data, "locale"),
            timezone: text(data, "timezone"),
        })
    }

    pub(crate) fn apply(&self, website: &mut Website) {
        if let Some(wait) = self.wait_idle {
            website.with_wait_for_idle_network(Some(WaitForIdleNetwork::new(Some(wait))));
        }
        if let Some(selector) = &self.wait_selector {
            website.with_wait_for_selector(Some(WaitForSelector::new(
                Some(MAX_WAIT),
                selector.clone(),
            )));
        }
        if let Some(selector) = &self.wait_dom {
            website.with_wait_for_idle_dom(Some(WaitForSelector::new(
                Some(MAX_WAIT),
                selector.clone(),
            )));
        }
        if let Some(wait) = self.wait_delay {
            website.with_wait_for_delay(Some(WaitForDelay::new(Some(wait))));
        }
        if let Some((width, height)) = self.viewport {
            website.with_viewport(Some(Viewport::new(width, height)));
        }
        website.with_disable_javascript(self.no_javascript);
        if !self.actions.is_empty() {
            website.with_automation_scripts(Some(self.actions.iter().cloned().collect()));
        }
        if !self.scripts.is_empty() {
            website.with_execution_scripts(Some(self.scripts.iter().cloned().collect()));
        }
        if let Some(block) = &self.block {
            let blocks = |kind| block.kinds.contains(&kind);
            // Nothing the blocking names is let through for being first-party.
            website.with_chrome_intercept(RequestInterceptConfiguration {
                enabled: true,
                block_visuals: blocks(BlockKind::Images),
                block_stylesheets: blocks(BlockKind::Stylesheets),
                block_javascript: blocks(BlockKind::Scripts),
                block_analytics: blocks(BlockKind::Analytics),
                block_ads: blocks(BlockKind::Ads),
                allow_first_party_visuals: !blocks(BlockKind::Images),
                allow_first_party_stylesheets: !blocks(BlockKind::Stylesheets),
                allow_first_party_javascript: !blocks(BlockKind::Scripts),
                blacklist_patterns: (!block.patterns.is_empty()).then(|| block.patterns.clone()),
                ..RequestInterceptConfiguration::default()
            });
        }
        if let Some(script) = &self.init_script {
            website.with_evaluate_on_new_document(Some(Box::new(script.clone())));
        }
        website
            .with_locale(self.locale.clone())
            .with_timezone_id(self.timezone.clone());
    }
}

/// The host a domain option names: a bare host, or a URL's host.
fn host_of(domain: &str) -> &str {
    let rest = domain.split_once("://").map_or(domain, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

fn flag(data: &Value, key: &str) -> bool {
    data.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn seconds(data: &Value, key: &str) -> anyhow::Result<Option<Duration>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| Some(Duration::from_secs_f64(seconds).min(MAX_WAIT)))
            .ok_or_else(|| anyhow!("web.crawl {key} must be a positive number of seconds")),
    }
}

/// A per-URL option: a map from a URL or path to its value.
fn per_url(data: &Value, key: &str) -> anyhow::Result<Vec<(String, Value)>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(entries)) => Ok(entries
            .iter()
            .map(|(target, value)| (target.clone(), value.clone()))
            .collect()),
        Some(_) => bail!("web.crawl {key} must map URLs or paths to their value"),
    }
}
