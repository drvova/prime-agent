//! The scheduling protocol arms on the worker (`cron_list`, `heartbeats_list`,
//! `heartbeat_manage`, `cron_add`, `cron_cancel`, `heartbeat_get`,
//! `heartbeat_set`, `heartbeat_update`): payload validation and the store
//! calls behind each `scheduled`-surface command. The heartbeat commands'
//! rules and replies are pa-core's (`cron::heartbeat_commands`), which the
//! runtime daemon shares.
use pa_core::cron::heartbeat_commands::{
    heartbeat_manage_action, heartbeat_reply, no_active_heartbeat, withdraws_queued_fire,
    HeartbeatSet, HeartbeatUpdate, HEARTBEATS_NEED_A_SESSION_FILE,
};
use pa_core::cron::store::SessionBinding;

use super::{
    is_heartbeat_cron_job, json, response_failure, response_success, CreateAgentCronJobInput,
    DaemonResponse, JobStatus, Value, Worker,
};

impl Worker {
    /// `cron_list` (TS daemon-mode case): the store's jobs filtered by the
    /// selector and the inactive cut.
    pub(crate) fn handle_cron_list(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_list") {
            return response;
        }
        let include_inactive = payload
            .get("includeInactive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let selector = payload.get("activeSessionId").and_then(Value::as_str);
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
        }
        let jobs: Vec<Value> = self
            .scheduled
            .store()
            .list()
            .into_iter()
            .filter(|job| {
                if !include_inactive && !matches!(job.status, JobStatus::Active | JobStatus::Paused)
                {
                    return false;
                }
                match selector {
                    Some(selector) => job.active_session_id == selector,
                    None => true,
                }
            })
            .filter_map(|job| serde_json::to_value(&job).ok())
            .collect();
        response_success(None, "cron_list", Some(json!({ "jobs": jobs })))
    }

    /// `heartbeats_list` (TS daemon-mode `listHeartbeats`): the live or
    /// paused heartbeat jobs as `{ job, sessionName?, firstMessage? }`.
    pub(crate) fn handle_heartbeats_list(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeats_list") {
            return response;
        }
        let summary = {
            let (core, inputs) = self.summary_inputs();
            self.bind_store_artifact(&core);
            self.summary_locked(&core, inputs)
        };
        let heartbeats: Vec<Value> = self
            .scheduled
            .store()
            .list()
            .into_iter()
            .filter(|job| {
                is_heartbeat_cron_job(job)
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .map(|job| {
                let mut heartbeat = json!({
                    "job": serde_json::to_value(&job).unwrap_or(Value::Null),
                });
                if let Some(name) = summary.session_name.as_deref() {
                    heartbeat["sessionName"] = json!(name);
                }
                if let Some(first) = summary.first_message.as_deref() {
                    heartbeat["firstMessage"] = json!(first);
                }
                heartbeat
            })
            .collect();
        response_success(
            None,
            "heartbeats_list",
            Some(json!({ "heartbeats": heartbeats })),
        )
    }

    /// `heartbeat_manage` (TS daemon-mode case over `manageHeartbeat`): pause/resume/stop
    /// a heartbeat by job id; an unknown id answers the TS error.
    pub(crate) async fn handle_heartbeat_manage(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_manage") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let job_id = payload
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The TS store treats any non-pause/non-stop action as resume.
        let action = heartbeat_manage_action(payload.get("action").and_then(Value::as_str));
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
        }
        let managed = self.scheduled.store().manage_heartbeat(
            &active_session_id,
            &job_id,
            action,
            crate::util::now_ms(),
        );
        let Ok(Some(job)) = managed else {
            return response_failure(
                None,
                "heartbeat_manage",
                &no_active_heartbeat(&job_id),
                None,
            );
        };
        if withdraws_queued_fire(action) {
            self.scheduled.remove_queued_heartbeat_follow_up(&job);
        }
        self.scheduled.wake().await;
        response_success(None, "heartbeat_manage", Some(heartbeat_reply(Some(&job))))
    }

    /// `cron_add` (TS daemon-mode case over `createCronJobForState`).
    pub(crate) async fn handle_cron_add(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_add") {
            return response;
        }
        let input = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
            let store = match core.store.as_ref() {
                Some(store) if !store.path.as_os_str().is_empty() => store,
                _ => {
                    return response_failure(
                        None,
                        "cron_add",
                        "Cron jobs require a persisted session file",
                        None,
                    )
                }
            };
            CreateAgentCronJobInput {
                active_session_id: core.active_session_id.clone(),
                session_id: store.session_id().to_string(),
                session_file: store.path.to_string_lossy().to_string(),
                cwd: core.cwd.clone(),
                runtime_kind: Some(core.runtime_kind.clone()),
                prompt: payload
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                schedule_text: payload
                    .get("schedule")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                ..Default::default()
            }
        };
        match self.scheduled.store().create(&input) {
            Ok(job) => {
                self.scheduled.wake().await;
                response_success(
                    None,
                    "cron_add",
                    Some(json!({ "job": serde_json::to_value(&job).unwrap_or(Value::Null) })),
                )
            }
            Err(error) => response_failure(None, "cron_add", &error.to_string(), None),
        }
    }

    /// `cron_cancel` (TS daemon-mode case): cancel by job id, drop any
    /// queued fire, and re-arm the timer.
    pub(crate) async fn handle_cron_cancel(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_cancel") {
            return response;
        }
        let job_id = payload
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
        }
        match self
            .scheduled
            .store()
            .cancel(&job_id, crate::util::now_ms())
        {
            Ok(Some(job)) => {
                self.scheduled.remove_queued_heartbeat_follow_up(&job);
                self.scheduled.wake().await;
                response_success(
                    None,
                    "cron_cancel",
                    Some(json!({ "job": serde_json::to_value(&job).unwrap_or(Value::Null) })),
                )
            }
            Ok(None) => response_failure(
                None,
                "cron_cancel",
                &format!("No cron job found: {job_id}"),
                None,
            ),
            Err(error) => response_failure(None, "cron_cancel", &error.to_string(), None),
        }
    }

    /// `heartbeat_get` (TS daemon-mode case): the session's live or paused
    /// heartbeat, or null.
    pub(crate) fn handle_heartbeat_get(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_get") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
        }
        let heartbeat = self.scheduled.store().get_heartbeat(&active_session_id);
        response_success(
            None,
            "heartbeat_get",
            Some(heartbeat_reply(heartbeat.as_ref())),
        )
    }

    /// `heartbeat_set` (TS daemon-mode case over `createHeartbeatForState`): replace the
    /// session's heartbeat.
    pub(crate) async fn handle_heartbeat_set(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_set") {
            return response;
        }
        let set = match HeartbeatSet::parse(payload) {
            Ok(set) => set,
            Err(error) => return response_failure(None, "heartbeat_set", &error.to_string(), None),
        };
        let (previous, input) = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
            let store = match core.store.as_ref() {
                Some(store) if !store.path.as_os_str().is_empty() => store,
                _ => {
                    return response_failure(
                        None,
                        "heartbeat_set",
                        HEARTBEATS_NEED_A_SESSION_FILE,
                        None,
                    )
                }
            };
            let previous = self
                .scheduled
                .store()
                .get_heartbeat(&core.active_session_id);
            // A replacement keeps the previous delivery mode unless the
            // command carries one (TS `createHeartbeatForState`).
            let input = set.input(
                SessionBinding {
                    active_session_id: core.active_session_id.clone(),
                    session_id: store.session_id().to_string(),
                    session_file: store.path.to_string_lossy().to_string(),
                    cwd: core.cwd.clone(),
                },
                &core.runtime_kind,
                previous.as_ref(),
            );
            (previous, input)
        };
        match self.scheduled.store().create_heartbeat(&input) {
            Ok(job) => {
                if let Some(previous) = previous {
                    self.scheduled.remove_queued_heartbeat_follow_up(&previous);
                }
                self.scheduled.wake().await;
                response_success(None, "heartbeat_set", Some(heartbeat_reply(Some(&job))))
            }
            Err(error) => response_failure(None, "heartbeat_set", &error.to_string(), None),
        }
    }

    /// `heartbeat_update` (TS daemon-mode case over `updateHeartbeatForState`):
    /// pause/resume/clear the session's heartbeat.
    pub(crate) async fn handle_heartbeat_update(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_update") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let update = HeartbeatUpdate::parse(payload.get("action").and_then(Value::as_str));
        let now = crate::util::now_ms();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.bind_store_artifact(&core);
        }
        let outcome = update.apply(self.scheduled.store(), &active_session_id, now);
        let outcome = match outcome {
            Ok(job) => job,
            Err(error) => {
                return response_failure(None, "heartbeat_update", &error.to_string(), None)
            }
        };
        if let Some(job) = &outcome {
            if withdraws_queued_fire(update.as_management()) {
                self.scheduled.remove_queued_heartbeat_follow_up(job);
            }
        }
        self.scheduled.wake().await;
        response_success(
            None,
            "heartbeat_update",
            Some(heartbeat_reply(outcome.as_ref())),
        )
    }
}
