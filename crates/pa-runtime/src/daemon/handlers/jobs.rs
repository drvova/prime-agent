//! The scheduled jobs as clients see them (the CLI's `schedule`): the
//! `schedules` query (`{session?, all?}`: the jobs of one session, or of
//! every session the daemon holds open or dormant; active and paused ones
//! unless `all`), `schedule.add` (`{session, schedule, prompt}` on an open
//! session) and `schedule.cancel` (`{job}`), each answering the job as the
//! old daemon's `cron_*` commands do (`{"job": …}`, `{"jobs": […]}`).
//!
//! A session's heartbeat (8d-4c), as the old worker's `heartbeat_*` arms
//! answer it (`{"heartbeat": job | null}`, pa-core's
//! `cron::heartbeat_commands`): the query `session.heartbeat {session}`
//! (`heartbeat_get`), and the calls `session.set_heartbeat {session,
//! schedule, prompt, deliveryMode?}` (`heartbeat_set`: the replacement),
//! `session.update_heartbeat {session, action}` (`heartbeat_update`: pause,
//! resume, or any other action clears it) on an open session, and
//! `session.manage_heartbeat {session, jobId, action}` (`heartbeat_manage`:
//! pause, stop, or any other action resumes; the session open or saved).

use pa_core::cron::heartbeat_commands::{
    heartbeat_manage_action, heartbeat_reply, HeartbeatSet, HeartbeatUpdate,
};
use serde_json::{json, Value};

use super::super::schedules::JobsShown;
use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) fn schedules(&self, args: &Value) -> anyhow::Result<Value> {
        let session = optional_string_arg(args, "session")?;
        let shown = match args.get("all") {
            None | Some(Value::Null | Value::Bool(false)) => JobsShown::Live,
            Some(Value::Bool(true)) => JobsShown::All,
            Some(_) => anyhow::bail!("the `all` argument must be a boolean"),
        };
        let jobs = self.schedules.jobs(session.as_deref(), shown);
        Ok(json!({ "jobs": jobs }))
    }

    pub(super) async fn schedule_add(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        // A session's jobs are armed as its engine is built.
        self.runtime.built_engine(&id).await?;
        let job = self.schedules.add(
            &id,
            &string_arg(args, "schedule")?,
            &string_arg(args, "prompt")?,
        )?;
        Ok(json!({ "job": job }))
    }

    pub(super) fn schedule_cancel(&self, args: &Value) -> anyhow::Result<Value> {
        let job = self.schedules.cancel(&string_arg(args, "job")?)?;
        Ok(json!({ "job": job }))
    }

    pub(super) async fn heartbeat(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        // A session's jobs are armed as its engine is built.
        self.runtime.built_engine(&id).await?;
        Ok(heartbeat_reply(self.schedules.heartbeat(&id).as_ref()))
    }

    pub(super) async fn set_heartbeat(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let set = HeartbeatSet::parse(args)?;
        self.runtime.built_engine(&id).await?;
        let job = self.schedules.set_heartbeat(&id, &set)?;
        Ok(heartbeat_reply(Some(&job)))
    }

    pub(super) async fn update_heartbeat(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let update = HeartbeatUpdate::parse(optional_string_arg(args, "action")?.as_deref());
        self.runtime.built_engine(&id).await?;
        let job = self.schedules.update_heartbeat(&id, update)?;
        Ok(heartbeat_reply(job.as_ref()))
    }

    pub(super) async fn manage_heartbeat(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let job = string_arg(args, "jobId")?;
        let action = heartbeat_manage_action(optional_string_arg(args, "action")?.as_deref());
        if open {
            self.runtime.built_engine(&id).await?;
        }
        let schedules = std::sync::Arc::clone(&self.schedules);
        let managed = tokio::task::spawn_blocking(move || {
            schedules.manage_heartbeat(&id, &path, &job, action)
        })
        .await
        .map_err(|error| anyhow::anyhow!("the heartbeat's store write panicked: {error}"))??;
        Ok(heartbeat_reply(Some(&managed)))
    }
}
