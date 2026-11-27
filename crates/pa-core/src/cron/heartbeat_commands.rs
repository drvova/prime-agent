//! The heartbeat commands as both daemons answer them (8d-4c): `heartbeat_get`,
//! `heartbeat_set`, `heartbeat_update` and `heartbeat_manage` over a session's store
//! ([`AgentCronJobStore`]), moved from the old worker's arms (pa-daemon
//! `scheduled_jobs/arms.rs`) and the supervisor's catalog arm (`scheduling_catalog.rs`); the
//! old daemon and the runtime daemon (`pa-runtime` `daemon/schedules.rs`) call them.
//!
//! - [`HeartbeatSet`]: the set's parse (its delivery mode's refusal), and the replacement's
//!   input, which keeps the previous heartbeat's delivery mode unless the command names one.
//! - [`HeartbeatUpdate`]: pause, resume, or (any other action) clear.
//! - [`heartbeat_manage_action`]: pause, stop, or (any other action) resume; an unknown job is
//!   [`no_active_heartbeat`].
//! - [`withdraws_queued_fire`]: every change but a resume withdraws the heartbeat's queued fire
//!   (the old `heartbeat:<id>` queue key).
//! - [`heartbeat_reply`]: each command's `{heartbeat}` (`null` for none).

use serde_json::{json, Value};

use super::store::{
    AgentCronJobStore, CreateAgentCronJobInput, HeartbeatManagementAction, SessionBinding,
};
use super::{
    normalize_heartbeat_delivery_mode, normalize_heartbeat_schedule, AgentCronJob, DeliveryMode,
};

/// The refusal of a heartbeat on a session that keeps no transcript.
pub const HEARTBEATS_NEED_A_SESSION_FILE: &str = "Heartbeats require a persisted session file";

/// `heartbeat_manage`'s refusal of a job the session's store has no live heartbeat for.
#[must_use]
pub fn no_active_heartbeat(job_id: &str) -> String {
    format!("No active heartbeat found: {job_id}")
}

/// One `heartbeat_set`: the session's heartbeat is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatSet {
    /// The schedule, normalized (a bare duration reads `every <duration>`).
    pub schedule_text: String,
    pub prompt: String,
    pub delivery_mode: Option<DeliveryMode>,
}

impl HeartbeatSet {
    /// Reads the command's `schedule`, `prompt` and `deliveryMode`.
    ///
    /// # Errors
    ///
    /// A `deliveryMode` other than `steer` or `follow_up`.
    pub fn parse(args: &Value) -> anyhow::Result<Self> {
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        Ok(HeartbeatSet {
            delivery_mode: normalize_heartbeat_delivery_mode(text("deliveryMode"))?,
            schedule_text: normalize_heartbeat_schedule(Some(text("schedule").unwrap_or_default())),
            prompt: text("prompt").unwrap_or_default().to_string(),
        })
    }

    /// The replacement's input for the session `binding` names, `previous` its live
    /// heartbeat: the previous delivery mode is kept unless the command names one.
    #[must_use]
    pub fn input(
        &self,
        binding: SessionBinding,
        runtime_kind: &str,
        previous: Option<&AgentCronJob>,
    ) -> CreateAgentCronJobInput {
        CreateAgentCronJobInput {
            active_session_id: binding.active_session_id,
            session_id: binding.session_id,
            session_file: binding.session_file,
            cwd: binding.cwd,
            runtime_kind: Some(runtime_kind.to_string()),
            delivery_mode: self
                .delivery_mode
                .or(previous.and_then(|job| job.delivery_mode)),
            schedule_text: self.schedule_text.clone(),
            prompt: self.prompt.clone(),
            ..Default::default()
        }
    }
}

/// One `heartbeat_update` of the session's heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatUpdate {
    Pause,
    Resume,
    /// Any action but `pause` and `resume` (TS `updateHeartbeatForState`).
    Clear,
}

impl HeartbeatUpdate {
    /// The command's `action`.
    #[must_use]
    pub fn parse(action: Option<&str>) -> Self {
        match action {
            Some("pause") => HeartbeatUpdate::Pause,
            Some("resume") => HeartbeatUpdate::Resume,
            _ => HeartbeatUpdate::Clear,
        }
    }

    /// Applies the update to session `active_session_id`'s heartbeat in `store`: the job it
    /// changed, `None` when the session has none.
    ///
    /// # Errors
    ///
    /// The store could not be written.
    pub fn apply(
        self,
        store: &AgentCronJobStore,
        active_session_id: &str,
        now: u64,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        match self {
            HeartbeatUpdate::Pause => store.pause_heartbeat(active_session_id, now),
            HeartbeatUpdate::Resume => store.resume_heartbeat(active_session_id, now),
            HeartbeatUpdate::Clear => store.clear_heartbeat(active_session_id, now),
        }
    }

    /// The action as `heartbeat_manage` names it, for [`withdraws_queued_fire`].
    #[must_use]
    pub fn as_management(self) -> HeartbeatManagementAction {
        match self {
            HeartbeatUpdate::Pause => HeartbeatManagementAction::Pause,
            HeartbeatUpdate::Resume => HeartbeatManagementAction::Resume,
            HeartbeatUpdate::Clear => HeartbeatManagementAction::Stop,
        }
    }
}

/// `heartbeat_manage`'s `action`: pause, stop, or (any other) resume.
#[must_use]
pub fn heartbeat_manage_action(action: Option<&str>) -> HeartbeatManagementAction {
    match action {
        Some("pause") => HeartbeatManagementAction::Pause,
        Some("stop") => HeartbeatManagementAction::Stop,
        _ => HeartbeatManagementAction::Resume,
    }
}

/// Whether a change by `action` withdraws the heartbeat's queued fire: every one but a resume.
#[must_use]
pub fn withdraws_queued_fire(action: HeartbeatManagementAction) -> bool {
    match action {
        HeartbeatManagementAction::Pause | HeartbeatManagementAction::Stop => true,
        HeartbeatManagementAction::Resume => false,
    }
}

/// A heartbeat command's reply: `{heartbeat}`, the job or `null`.
#[must_use]
pub fn heartbeat_reply(job: Option<&AgentCronJob>) -> Value {
    json!({
        "heartbeat": job
            .and_then(|job| serde_json::to_value(job).ok())
            .unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_keeps_the_previous_delivery_mode_unless_it_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = AgentCronJobStore::for_session_artifacts();
        store.register_session_artifact("s", dir.path());
        let binding = SessionBinding {
            active_session_id: "s".to_string(),
            session_id: "s".to_string(),
            session_file: dir.path().join("s.jsonl").display().to_string(),
            cwd: "/".to_string(),
        };
        let set = |args: Value, previous: Option<&AgentCronJob>| {
            let input =
                HeartbeatSet::parse(&args)
                    .unwrap()
                    .input(binding.clone(), "top-level", previous);
            store.create_heartbeat(&input).unwrap()
        };
        let first = set(
            json!({ "schedule": "5m", "prompt": "p", "deliveryMode": "follow_up" }),
            None,
        );
        let kept = set(json!({ "schedule": "5m", "prompt": "q" }), Some(&first));
        let named = set(
            json!({ "prompt": "r", "deliveryMode": "steer" }),
            Some(&kept),
        );
        let refused = HeartbeatSet::parse(&json!({ "deliveryMode": "later" }))
            .map_err(|error| error.to_string());
        assert_eq!(
            (
                first.schedule.expression,
                kept.delivery_mode,
                named.delivery_mode,
                refused
            ),
            (
                "every 5m".to_string(),
                Some(DeliveryMode::FollowUp),
                Some(DeliveryMode::Steer),
                Err("Heartbeat delivery mode must be \"steer\" or \"follow_up\"".to_string())
            )
        );
    }

    #[test]
    fn unknown_actions_clear_an_update_and_resume_a_manage() {
        assert_eq!(
            (
                [Some("pause"), Some("resume"), Some("drop"), None].map(HeartbeatUpdate::parse),
                [Some("pause"), Some("stop"), Some("drop")].map(heartbeat_manage_action),
            ),
            (
                [
                    HeartbeatUpdate::Pause,
                    HeartbeatUpdate::Resume,
                    HeartbeatUpdate::Clear,
                    HeartbeatUpdate::Clear,
                ],
                [
                    HeartbeatManagementAction::Pause,
                    HeartbeatManagementAction::Stop,
                    HeartbeatManagementAction::Resume,
                ],
            )
        );
    }
}
