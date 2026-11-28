//! Saved-session catalog resolve (TS catalog `resolve`): the selector-to-session-file
//! lookup both daemons use when a command addresses a session that is not open. Local
//! (cwd-scoped) matches win over global ones; an ambiguous selector fails with the TS error
//! so callers can tell it apart from an unknown-target miss.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use super::{list_sessions, SessionInfo};
use crate::session::manager::get_session_file_path;

/// One selector match against saved sessions: the session id is addressable
/// by prefix, the name only exactly; more than one match is ambiguous.
fn catalog_session_match<'a>(
    sessions: impl IntoIterator<Item = &'a SessionInfo>,
    selector: &str,
) -> Result<Option<&'a SessionInfo>> {
    let matches: Vec<&SessionInfo> = sessions
        .into_iter()
        .filter(|info| info.id.starts_with(selector) || info.name.as_deref() == Some(selector))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(anyhow!("Ambiguous session selector \"{selector}\"")),
    }
}

/// Resolve one selector to a saved session: cwd-scoped sessions first, the
/// whole live catalog second. A live miss lazily restores a match from
/// `archive_dir` (`sessions-archive/`, written by older Rust releases) into
/// `sessions_dir` before the caller wakes it. Misses are `None` (the caller
/// answers its unknown-target error, not a catalog one).
///
/// # Errors
///
/// The selector is ambiguous, or the archived match could not be restored.
pub fn resolve_saved_session(
    sessions_dir: &Path,
    archive_dir: &Path,
    selector: &str,
    cwd: &str,
) -> Result<Option<SessionInfo>> {
    let all = list_sessions(sessions_dir);
    let local = catalog_session_match(all.iter().filter(|info| info.cwd == cwd), selector)?;
    if let Some(info) = local {
        return Ok(Some(info.clone()));
    }
    if let Some(info) = catalog_session_match(all.iter(), selector)? {
        return Ok(Some(info.clone()));
    }
    let archived = list_sessions(archive_dir);
    let local = catalog_session_match(archived.iter().filter(|info| info.cwd == cwd), selector)?;
    let matched = match local {
        Some(info) => Some(info),
        None => catalog_session_match(archived.iter(), selector)?,
    };
    let Some(info) = matched else {
        return Ok(None);
    };
    let destination = get_session_file_path(sessions_dir, &info.id);
    crate::session::store::store()
        .restore(&info.path, &destination)
        .with_context(|| format!("restore archived session {}", info.id))?;
    let mut restored = info.clone();
    restored.path = destination;
    Ok(Some(restored))
}
