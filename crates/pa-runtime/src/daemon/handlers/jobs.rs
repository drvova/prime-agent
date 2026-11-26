//! The scheduled jobs as clients see them (the CLI's `schedule`): the
//! `schedules` query (`{session?, all?}`: the jobs of one session, or of
//! every session the daemon holds open or dormant; active and paused ones
//! unless `all`), `schedule.add` (`{session, schedule, prompt}` on an open
//! session) and `schedule.cancel` (`{job}`), each answering the job as the
//! old daemon's `cron_*` commands do (`{"job": …}`, `{"jobs": […]}`).

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
}
