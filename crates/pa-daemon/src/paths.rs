//! Filesystem layout: agent dir, sessions dir, logs, worker descriptors.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};

pub const AGENT_DIR_ENV: &str = "PRIME_AGENT_CODING_AGENT_DIR";
pub const SESSION_DIR_ENV: &str = "PRIME_AGENT_SESSION_DIR";

/// The home directory for state layout. Unresolvable home is an explicit
/// error, not a degraded `/tmp` default: the daemon must refuse to start
/// rather than write durable state outside the user profile.
///
/// # Errors
///
/// Returns an error when the home directory cannot be resolved.
pub fn home_dir() -> Result<PathBuf> {
    pa_types::platform::home_dir()
        .ok_or_else(|| anyhow!("home directory not found: set HOME (or USERPROFILE on Windows)"))
}

/// Expand a leading `~`/`~/` against [`home_dir`]; other paths pass through.
///
/// # Errors
///
/// Returns an error when expanding `~`/`~/` needs the home directory and
/// [`home_dir`] cannot resolve it.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        Ok(home_dir()?.join(rest))
    } else if path == "~" {
        home_dir()
    } else {
        Ok(PathBuf::from(path))
    }
}

/// The agent state root: the `PRIME_AGENT_CODING_AGENT_DIR` override when
/// set (tilde expanded), else this build's per-user dir under the home
/// directory (`.prime/agent`, or a distribution build's own,
/// [`pa_types::platform::StateLayout`]).
///
/// # Errors
///
/// Returns an error when the override cannot be tilde-expanded, or when the
/// fallback needs the home directory and [`home_dir`] cannot resolve it.
pub fn agent_dir() -> Result<PathBuf> {
    match pa_types::session_scope::var_os(AGENT_DIR_ENV) {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir.to_string_lossy()),
        _ => Ok(pa_types::platform::StateLayout::BUILD.home_agent_dir(&home_dir()?)),
    }
}

/// The sessions root: the `PRIME_AGENT_SESSION_DIR` override when set
/// (tilde expanded), else `<agent-dir>/sessions`.
///
/// # Errors
///
/// Returns an error when the override cannot be tilde-expanded.
pub fn sessions_dir(agent_dir: &Path) -> Result<PathBuf> {
    match pa_types::session_scope::var_os(SESSION_DIR_ENV) {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir.to_string_lossy()),
        _ => Ok(agent_dir.join("sessions")),
    }
}

#[must_use]
pub fn logs_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("logs")
}

/// Create the directory (and any missing parents), then restrict its
/// permissions to the current user.
///
/// # Errors
///
/// Returns an error when directory creation fails; the permission
/// restriction is best effort.
pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let _ = pa_core::platform::perms::restrict_dir(path);
    Ok(())
}

/// Age of a file's mtime, for stale-lease detection.
#[must_use]
pub fn mtime_age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(modified).ok()
}
