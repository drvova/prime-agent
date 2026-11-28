//! Why a session was closed on purpose and what the close does to what the
//! session keeps: its scheduled jobs and its `archived` state. The old
//! worker's `kill` and the runtime daemon's `session.stop` apply the same
//! rule.
//!
//! - `killed` (a client's stop): every job of the session is cancelled in
//!   the store, so nothing wakes it again, and its transcript records the
//!   `archived` state.
//! - `replaced` (a child closed as its parent's session is replaced): a
//!   subagent's RLM heartbeats are cancelled, its plain jobs stay, and the
//!   transcript records `archived`.
//! - `shutdown` (a child closed as its parent's daemon goes down): every job
//!   stays and nothing is archived, so the next daemon resumes it.

use pa_types::daemon::DaemonSessionClosedReason;

/// Why a session was closed on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCloseReason {
    Killed,
    Shutdown,
    Replaced,
}

/// What a close does to the session's scheduled jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedJobs {
    /// Every job of the session is cancelled.
    CancelAll,
    /// A subagent's RLM heartbeats are cancelled; a root cancels nothing.
    CancelSubagentHeartbeats,
    /// Every job stays.
    Keep,
}

impl SessionCloseReason {
    /// The close a stop's `rlmCloseReason` marker names (a child-close
    /// cascade's `shutdown` or `replaced`); a stop without one is `killed`.
    ///
    /// # Errors
    ///
    /// The marker names no close reason.
    pub fn from_marker(marker: Option<&str>) -> Result<Self, String> {
        match marker {
            None => Ok(Self::Killed),
            Some("shutdown") => Ok(Self::Shutdown),
            Some("replaced") => Ok(Self::Replaced),
            Some(other) => Err(format!(
                "{other:?} is no close reason of a stop (shutdown, replaced)"
            )),
        }
    }

    /// What the close does to the session's jobs.
    #[must_use]
    pub fn jobs(self) -> ClosedJobs {
        match self {
            Self::Killed => ClosedJobs::CancelAll,
            Self::Replaced => ClosedJobs::CancelSubagentHeartbeats,
            Self::Shutdown => ClosedJobs::Keep,
        }
    }

    /// Whether the close records the session's `archived` state.
    #[must_use]
    pub fn archives(self) -> bool {
        match self {
            Self::Killed | Self::Replaced => true,
            Self::Shutdown => false,
        }
    }

    /// The reason its clients are told (`session_closed`, `agent/<id>/closed`).
    #[must_use]
    pub fn wire(self) -> DaemonSessionClosedReason {
        match self {
            Self::Killed => DaemonSessionClosedReason::Killed,
            Self::Shutdown => DaemonSessionClosedReason::Shutdown,
            Self::Replaced => DaemonSessionClosedReason::Replaced,
        }
    }
}
