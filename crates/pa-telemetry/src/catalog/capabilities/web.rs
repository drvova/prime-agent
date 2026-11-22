//! The `web` capability: reading a URL. Its search counters are not a standalone event: they
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
