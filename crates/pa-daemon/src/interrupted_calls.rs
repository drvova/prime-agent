//! The tool calls a session's previous lifetime left without a result,
//! answered once in its transcript before the engine adopts it
//! (`pa_core::session::revival::interrupted_call_results` decides which and
//! with what).

use std::collections::BTreeMap;

use pa_core::session::revival::{interrupted_call_results, LifetimeEnd};
use serde_json::json;

use crate::session_store::SessionFile;

/// Answer, in `store`, each tool call of the branch's last assistant turn
/// that has no result. `crash_kind` is how the previous lifetime ended (the
/// create replay's `sessionCrash` kind; `None` for a plain reopen) and
/// `reaped_programs` how many programs its commands left running were
/// stopped after it ended. Returns how many calls were answered, by class.
///
/// # Errors
///
/// Returns an error when a result row cannot be saved; the rows saved
/// before it stay.
pub(crate) fn report(
    store: &mut SessionFile,
    crash_kind: Option<&str>,
    reaped_programs: u64,
) -> anyhow::Result<BTreeMap<&'static str, u64>> {
    let (results, counts) = {
        let branch = store.branch();
        interrupted_call_results(
            branch
                .iter()
                .map(|entry| (entry.type_.as_str(), &entry.fields)),
            LifetimeEnd::of_crash_kind(crash_kind),
            reaped_programs,
        )
    };
    for message in results {
        store.persist_entry("message", json!({ "message": message }))?;
    }
    Ok(counts)
}
