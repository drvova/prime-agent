//! The kernel's read-only `usage.*` host requests (the REPL's `rlm.usage`
//! module): what the session has spent so far and, where a daemon serves
//! the session, the provider quota windows the `/usage` page shows.
//!
//! - `usage.session {}` answers the session's spend: the branch's billed
//!   rows (assistant turns with their subagents' attributed usage folded in,
//!   compactions, branch summaries, `model_usage` rows) plus a windowed
//!   load's folded older path, the same totals `get_session_stats` reports;
//!   `cost.subagents` is the share the child-usage attributions carry.
//! - `usage.quotas {refresh?}` answers the `/usage` page's `QuotaReport`.
//!   The daemon worker registers the real handler (the supervisor's own
//!   `get_usage` fetch and cache); a session without a daemon keeps the
//!   fallback registered here, which says why no quota is available instead
//!   of failing.
//!
//! Neither reply carries a credential: the quota report holds provider ids,
//! plans, and windows only.

use std::sync::{Arc, Weak};

use pa_types::ai::Usage;
use pa_types::session::{AgentMessage, FileEntry};
use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::session::manager::SessionManager;
use crate::session_engine::rlm_usage::add_assistant_usage;

/// The request type the daemon worker overrides with its quota fetch.
pub const USAGE_QUOTAS_REQUEST_TYPE: &str = "usage.quotas";

/// Observes each answered `usage.session` (adoption telemetry); never sees a
/// number.
pub(crate) type UsageObserver = Arc<dyn Fn() + Send + Sync>;

/// The active branch's billed usage (TS `getSessionStats`' token and cost
/// fold): assistant rows, whose usage already carries their subagents'
/// attributed spend, plus the rows that bill a model call outside the loop.
#[must_use]
pub fn branch_spend(manager: &SessionManager) -> Usage {
    let mut spend = Usage::default();
    for entry in manager.get_branch(None) {
        let usage = match entry {
            FileEntry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => Some(assistant.usage),
            _ => crate::session::billed_row::billed_file_entry(entry).map(|row| row.usage),
        };
        if let Some(usage) = usage {
            add_assistant_usage(&mut spend, &usage);
        }
    }
    spend
}

/// The subagents' share of the branch spend: the child-usage attributions
/// that target the branch's assistant rows, each row id once (a retried
/// durable append can land the same id twice).
fn subagent_spend(manager: &SessionManager) -> Usage {
    let branch_assistants = manager
        .get_branch(None)
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::Message {
                message: AgentMessage::Assistant(_),
                ..
            } => entry.id(),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut seen = std::collections::HashSet::new();
    let mut spend = Usage::default();
    for entry in manager.retained_entries() {
        let FileEntry::ChildUsageAttributed { payload, base } = entry else {
            continue;
        };
        let first = base.id.as_deref().is_none_or(|id| seen.insert(id));
        if first && branch_assistants.contains(payload.target_id.as_str()) {
            add_assistant_usage(&mut spend, &payload.child_usage);
        }
    }
    spend
}

/// The `usage.session` reply.
fn session_usage_json(manager: &SessionManager) -> Value {
    let mut spend = branch_spend(manager);
    let mut total_cost = spend.cost.total.as_f64();
    // A windowed load's folded older path is spend on this branch too.
    if let Some(older) = manager.older_path_stats() {
        spend.input += older.input;
        spend.output += older.output;
        spend.cache_read += older.cache_read;
        spend.cache_write += older.cache_write;
        total_cost += older.cost;
    }
    let subagents = subagent_spend(manager).cost.total.as_f64();
    json!({
        "cost": {
            "total": total_cost,
            "own": (total_cost - subagents).max(0.0),
            "subagents": subagents,
        },
        "tokens": {
            "input": spend.input,
            "output": spend.output,
            "cacheRead": spend.cache_read,
            "cacheWrite": spend.cache_write,
            "total": spend.input + spend.output + spend.cache_read + spend.cache_write,
        },
    })
}

/// Register `usage.session`, and the no-daemon `usage.quotas` fallback
/// unless the embedding already registered the real one.
pub(crate) fn register_usage_host_handlers(
    handlers: &mut HostRequestHandlers,
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    observer: Option<UsageObserver>,
) {
    // Weak: the kernel holds its handlers for its whole life.
    let session: Weak<tokio::sync::Mutex<SessionManager>> = Arc::downgrade(session);
    handlers.register(
        "usage.session",
        host_handler(move |_payload| {
            let session = session.clone();
            let observer = observer.clone();
            async move {
                let Some(session) = session.upgrade() else {
                    anyhow::bail!("the session ended before the request could be served");
                };
                let reply = session_usage_json(&*session.lock().await);
                if let Some(observer) = &observer {
                    observer();
                }
                Ok(reply)
            }
        }),
    );
    if handlers.get(USAGE_QUOTAS_REQUEST_TYPE).is_none() {
        handlers.register(
            USAGE_QUOTAS_REQUEST_TYPE,
            host_handler(|_payload| async {
                Ok(json!({
                    "providers": [],
                    "unavailable": "provider quotas are read by the prime-agent daemon; this session runs without one",
                }))
            }),
        );
    }
}

#[cfg(test)]
mod tests;
