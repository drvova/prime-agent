//! The agents view's controls of one session, as both daemons answer them (8d-4b), moved from
//! the old worker's arms (pa-daemon `agent_message_ingest.rs`, `rlm_surface.rs` and the agent
//! engine's `set_rlm_max_depth`); the old worker and the runtime daemon (`pa-runtime`
//! `daemon/handlers/agent_controls.rs`) call them:
//!
//! - **Agent-message ingest.** [`AgentMessagePause`], the receiving session's pause flag and its
//!   delivery refusal ([`AGENT_MESSAGING_PAUSED`]); [`agent_message_safety_status`], the
//!   `agent_messages_status`/`_pause`/`_resume` reply; [`unscoped_agent_message_status`], the
//!   selector-less status while no session runs; [`cleared_agent_messages`], the
//!   `agent_messages_clear` reply (the removed prompts per lane).
//! - **The RLM depth bound.** [`rlm_max_depth_status`], `get_rlm_max_depth_status`'s reply;
//!   [`SetRlmMaxDepth`], `set_rlm_max_depth`'s parse, its durable `rlm_max_depth_state` row
//!   ([`RLM_MAX_DEPTH_STATE_CUSTOM_TYPE`]), the global settings write and the reply that reports
//!   it (`globalSaved`, `globalError`).
//! - **The subagent delete.** [`SubagentDeleteOutcome`], `delete_rlm_subagent`'s outcomes and
//!   their reply.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use super::agent_messaging::{
    DEFAULT_AGENT_MESSAGE_MAX_CHARS, DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
    DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY, DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
};

/// The delivery refusal while a receiver's agent messaging is paused (TS
/// `sendAgentSessionMessage`'s text).
pub const AGENT_MESSAGING_PAUSED: &str = "Agent messaging is paused";

/// The selector-less `agent_messages_status` while no session runs: the old supervisor's
/// empty status.
#[must_use]
pub fn unscoped_agent_message_status() -> Value {
    json!({ "paused": false, "limits": {} })
}

/// The session row that keeps a chat override of the depth bound: a session opened again
/// resolves its bound from it (`rlm_host::resolve_rlm_max_depth`).
pub const RLM_MAX_DEPTH_STATE_CUSTOM_TYPE: &str = "rlm_max_depth_state";

/// `set_rlm_max_depth`'s refusal of a `maxDepth` that is not a count.
pub const RLM_MAX_DEPTH_REFUSAL: &str = "RLM max depth must be a non-negative integer.";

/// A receiving session's agent-message pause: while set, every delivery to it is refused with
/// [`AGENT_MESSAGING_PAUSED`]. It lasts the session's open life, as the old worker's flag did.
#[derive(Debug, Default)]
pub struct AgentMessagePause(AtomicBool);

impl AgentMessagePause {
    /// Whether deliveries are refused.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Pauses (`agent_messages_pause`) or resumes (`agent_messages_resume`) the ingest.
    pub fn set(&self, paused: bool) {
        self.0.store(paused, Ordering::SeqCst);
    }

    /// The delivery gate.
    ///
    /// # Errors
    ///
    /// [`AGENT_MESSAGING_PAUSED`] while the ingest is paused.
    pub fn admit(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.paused(), AGENT_MESSAGING_PAUSED);
        Ok(())
    }
}

/// The safety status `agent_messages_status`, `_pause` and `_resume` answer: the pause flag
/// and the ingest's limits.
#[must_use]
pub fn agent_message_safety_status(paused: bool) -> Value {
    json!({
        "paused": paused,
        "maxMessageChars": DEFAULT_AGENT_MESSAGE_MAX_CHARS,
        "maxPendingPerSession": DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
        "rateLimitCapacity": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY,
        "rateLimitRefillMs": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
    })
}

/// `agent_messages_clear`'s (and the pause's) removed agent messages: each one's prompt, in its
/// lane, in queue order. Client prompts are never removed.
#[must_use]
pub fn cleared_agent_messages(steering: &[String], follow_up: &[String]) -> Value {
    json!({ "steering": steering, "followUp": follow_up })
}

/// `get_rlm_max_depth_status`'s reply: the session's bound and where it comes from (`default`,
/// `env`, `global`, `inherited` or `chat`).
#[must_use]
pub fn rlm_max_depth_status(max_depth: u32, source: &str) -> Value {
    json!({ "maxDepth": max_depth, "source": source })
}

/// One `set_rlm_max_depth`: the session's bound becomes `max_depth` (a chat override, kept as the
/// session's [`RLM_MAX_DEPTH_STATE_CUSTOM_TYPE`] row), and with `global` the global settings
/// default too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetRlmMaxDepth {
    pub max_depth: u64,
    pub global: bool,
}

impl SetRlmMaxDepth {
    /// Reads the command's `maxDepth` and `global`.
    ///
    /// # Errors
    ///
    /// [`RLM_MAX_DEPTH_REFUSAL`] for a `maxDepth` that is missing or not a count.
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        let max_depth = args
            .get("maxDepth")
            .and_then(Value::as_u64)
            .ok_or(RLM_MAX_DEPTH_REFUSAL)?;
        Ok(SetRlmMaxDepth {
            max_depth,
            global: args.get("global").and_then(Value::as_bool) == Some(true),
        })
    }

    /// The bound as the session's delegation depth holds it.
    #[must_use]
    pub fn bound(&self) -> u32 {
        u32::try_from(self.max_depth).unwrap_or(u32::MAX)
    }

    /// The data of the session's [`RLM_MAX_DEPTH_STATE_CUSTOM_TYPE`] row.
    #[must_use]
    pub fn state_row(&self) -> Value {
        json!({ "maxDepth": self.max_depth })
    }

    /// Writes the global settings default under `agent_dir` when the command asks for it (a
    /// blocking file write): `None` when it did not ask, else the write's outcome.
    #[must_use]
    pub fn write_global(&self, cwd: &Path, agent_dir: &Path) -> Option<Result<(), String>> {
        self.global.then(|| {
            crate::settings::SettingsManager::create(cwd, agent_dir)
                .set_rlm_max_depth(self.max_depth)
                .map_err(|error| error.to_string())
        })
    }

    /// The reply (TS `SetRlmMaxDepthResult`): the bound, its `chat` source and whether the
    /// global default was saved; a failed global write is reported as `globalError`, never as
    /// the command's failure.
    #[must_use]
    pub fn reply(&self, global: Option<Result<(), String>>) -> Value {
        let mut reply = json!({
            "maxDepth": self.max_depth,
            "source": "chat",
            "globalSaved": matches!(global, Some(Ok(()))),
        });
        if let Some(Err(error)) = global {
            reply["globalError"] = json!(error);
        }
        reply
    }
}

/// How one `delete_rlm_subagent` went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentDeleteOutcome {
    /// The settled child (or one deleted before) is gone.
    Deleted,
    /// The child has live work: nothing was deleted.
    Running,
    /// No child of the session has the id.
    NotFound,
}

impl SubagentDeleteOutcome {
    /// The reply: `{deleted}`, with `reason: "running"` for a child with live work.
    #[must_use]
    pub fn reply(self) -> Value {
        match self {
            SubagentDeleteOutcome::Deleted => json!({ "deleted": true }),
            SubagentDeleteOutcome::Running => json!({ "deleted": false, "reason": "running" }),
            SubagentDeleteOutcome::NotFound => json!({ "deleted": false }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paused_ingest_refuses_deliveries_until_resumed() {
        let pause = AgentMessagePause::default();
        pause.set(true);
        let refused = pause.admit().map_err(|error| error.to_string());
        pause.set(false);
        assert_eq!(
            (refused, pause.admit().map_err(|error| error.to_string())),
            (Err(AGENT_MESSAGING_PAUSED.to_string()), Ok(()))
        );
    }

    #[test]
    fn set_rlm_max_depth_reports_its_global_write() {
        let parsed = [
            json!({ "maxDepth": 3 }),
            json!({ "maxDepth": 4, "global": true }),
            json!({ "maxDepth": -1 }),
            json!({}),
        ]
        .map(|args| SetRlmMaxDepth::parse(&args));
        let set = SetRlmMaxDepth {
            max_depth: 4,
            global: true,
        };
        assert_eq!(
            (
                parsed,
                set.reply(None),
                set.reply(Some(Ok(()))),
                set.reply(Some(Err("read-only".to_string())))
            ),
            (
                [
                    Ok(SetRlmMaxDepth {
                        max_depth: 3,
                        global: false
                    }),
                    Ok(set),
                    Err(RLM_MAX_DEPTH_REFUSAL),
                    Err(RLM_MAX_DEPTH_REFUSAL),
                ],
                json!({ "maxDepth": 4, "source": "chat", "globalSaved": false }),
                json!({ "maxDepth": 4, "source": "chat", "globalSaved": true }),
                json!({ "maxDepth": 4, "source": "chat", "globalSaved": false,
                        "globalError": "read-only" }),
            )
        );
    }

    #[test]
    fn a_global_write_lands_in_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let set = SetRlmMaxDepth {
            max_depth: 5,
            global: true,
        };
        let written = set.write_global(dir.path(), dir.path());
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (written, &settings["rlmMaxDepth"]),
            (Some(Ok(())), &json!(5))
        );
    }
}
