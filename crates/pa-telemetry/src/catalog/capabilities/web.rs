//! The `web` capability: reading a URL and crawling a site. Its search counters are not a standalone event: they
//! ride `agent session ended` (`web_search_*`), whose rule stays in the catalog.

use super::super::{count, duration, required, uuid, EventRule};

/// Kernel `web.read` adoption for one session (v2), emitted at session end beside `agent session
/// ended` (which sits near the backend's 128-key cap) only when the session read a URL: reads,
/// failures, reads by kind (a site route such as arXiv/GitHub/Stack Exchange, a PDF, a generic
/// page), truncated reads, and the slowest read. Counts only: never a URL, a title, or content.
pub(in crate::catalog) const AGENT_WEB_READS: EventRule = EventRule {
    name: "agent web reads",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("web_read_count", required(count())),
        ("web_read_failed_count", required(count())),
        ("web_read_site_count", required(count())),
        ("web_read_pdf_count", required(count())),
        ("web_read_page_count", required(count())),
        ("web_read_truncated_count", required(count())),
        ("web_read_max_ms", required(duration())),
    ],
};

/// Kernel `web.crawl` adoption for one session, emitted at session end beside `agent web reads`
/// only when the session crawled a site: crawls, crawls that failed outright, plain, rendered
/// (a local browser) and smart (HTTP first, the browser when a page needs it) crawls, pages returned, crawls whose identity the user's `web.crawl` settings
/// changed (robots.txt, user agent, headers, cookies, proxies, stealth), crawls that wrote a WARC
/// archive, the slowest crawl, and
/// `web-screenshot` / `web-pdf` captures and their failures.
/// Counts only: never a URL, a pattern, a title, a setting's value, or content.
pub(in crate::catalog) const AGENT_WEB_CRAWLS: EventRule = EventRule {
    name: "agent web crawls",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("web_crawl_count", required(count())),
        ("web_crawl_failed_count", required(count())),
        ("web_crawl_plain_count", required(count())),
        ("web_crawl_rendered_count", required(count())),
        ("web_crawl_smart_count", required(count())),
        ("web_crawl_page_count", required(count())),
        ("web_crawl_identity_count", required(count())),
        ("web_crawl_warc_count", required(count())),
        ("web_crawl_max_ms", required(duration())),
        ("web_screenshot_count", required(count())),
        ("web_pdf_count", required(count())),
        ("web_capture_failed_count", required(count())),
    ],
};
