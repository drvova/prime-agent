//! The kernel `schedule.add/list/cancel` host requests: session-owned scheduled jobs on the
//! embedding's cron store. A session reaches only the jobs it created; an embedding without a
//! scheduler (headless and daemonless runs) answers every request with an error naming why,
//! instead of accepting a job nothing would ever fire.

use std::sync::Arc;

use serde_json::{json, Value};

use super::SessionRuntime;
use crate::cron::store::{
    AgentCronJobStore, CreateAgentCronJobInput, ReplScheduleListing, ReplScheduleWhen,
};
use crate::cron::{AgentCronJob, JobStatus, ScheduleKind};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::session_engine::goal_driver::GoalTelemetrySlot;
use crate::session_engine::host_requests::{RlmHeartbeatMutation, RlmHeartbeatMutationHook};

/// Why scheduling is unavailable in an embedding without a scheduler.
const NO_SCHEDULER: &str = "scheduled jobs need the prime-agent daemon, which fires them later; \
     this run is headless or daemonless, so nothing was scheduled";

/// One `schedule.*` call, as adoption telemetry counts it (never a prompt or a schedule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleFact {
    /// A one-shot job was added.
    AddedOnce,
    /// A recurring (interval or cron) job was added.
    AddedRecurring,
    /// The session listed its jobs.
    Listed,
    /// One of the session's jobs was cancelled.
    Cancelled,
    /// The request was refused (invalid, over a cap, unknown id, or no scheduler).
    Rejected,
}

/// What one `schedule.*` handler needs, shared across its three request types.
struct ScheduleHost {
    store: Option<Arc<AgentCronJobStore>>,
    active_session_id: String,
    session_id: String,
    session_file: String,
    cwd: String,
    mutation_hook: Option<RlmHeartbeatMutationHook>,
    telemetry: GoalTelemetrySlot,
}

impl SessionRuntime {
    /// Register `schedule.add`, `schedule.list`, and `schedule.cancel`.
    pub(super) fn register_schedule_handlers(&self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(ScheduleHost {
            store: self.cron_store.clone(),
            active_session_id: self.active_session_id.clone(),
            session_id: self.binding.session_id.clone(),
            session_file: self.binding.session_file.clone(),
            cwd: self.binding.cwd.clone(),
            mutation_hook: self.cron_mutation_hook.clone(),
            telemetry: Arc::clone(&self.goal_telemetry),
        });
        for request_type in ["schedule.add", "schedule.list", "schedule.cancel"] {
            let host = Arc::clone(&host);
            handlers.register(
                request_type,
                host_handler(move |payload| {
                    let host = Arc::clone(&host);
                    Box::pin(async move { host.handle(request_type, &payload.data).await })
                }),
            );
        }
    }
}

impl ScheduleHost {
    #[tracing::instrument(skip(self, data))]
    async fn handle(&self, request_type: &str, data: &Value) -> anyhow::Result<Value> {
        let outcome = self.run(request_type, data);
        let fact = match &outcome {
            Ok((_, fact, _)) => *fact,
            Err(_) => ScheduleFact::Rejected,
        };
        if let Some(counters) = self.telemetry.get() {
            counters.note_repl_schedule(fact);
        }
        let (response, _, mutation) = outcome?;
        if let (Some(mutation), Some(hook)) = (mutation, &self.mutation_hook) {
            // The daemon's post-mutation work: re-arm the scheduler for the new fire time.
            hook(mutation).await;
        }
        Ok(response)
    }

    fn run(
        &self,
        request_type: &str,
        data: &Value,
    ) -> anyhow::Result<(Value, ScheduleFact, Option<RlmHeartbeatMutation>)> {
        let Some(store) = &self.store else {
            anyhow::bail!("{NO_SCHEDULER}");
        };
        let now = crate::cron::store::now_millis();
        match request_type {
            "schedule.add" => {
                if self.session_file.is_empty() {
                    anyhow::bail!(
                        "scheduled jobs belong to a saved session, and this session is not saved"
                    );
                }
                let prompt = data
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = |key: &str| {
                    data.get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|text| !text.trim().is_empty())
                };
                let when = match (text("at"), text("every"), text("cron")) {
                    (Some(at), None, None) => ReplScheduleWhen::At(at),
                    (None, Some(every), None) => ReplScheduleWhen::Every(every),
                    (None, None, Some(cron)) => ReplScheduleWhen::Cron(cron),
                    _ => anyhow::bail!("pass exactly one of at=, every=, or cron="),
                };
                let job = store.create_repl_schedule(
                    &CreateAgentCronJobInput {
                        active_session_id: self.active_session_id.clone(),
                        session_id: self.session_id.clone(),
                        session_file: self.session_file.clone(),
                        cwd: self.cwd.clone(),
                        label: text("label"),
                        prompt: prompt.to_string(),
                        now: Some(now),
                        ..Default::default()
                    },
                    &when,
                )?;
                let fact = match job.schedule.kind {
                    ScheduleKind::Once => ScheduleFact::AddedOnce,
                    ScheduleKind::Interval | ScheduleKind::Cron => ScheduleFact::AddedRecurring,
                };
                Ok((
                    json!({ "job": schedule_job_response(&job) }),
                    fact,
                    Some(RlmHeartbeatMutation {
                        job,
                        drop_queued: false,
                    }),
                ))
            }
            "schedule.list" => {
                let listing = match data.get("include_inactive").and_then(Value::as_bool) {
                    Some(true) => ReplScheduleListing::All,
                    Some(false) | None => ReplScheduleListing::Live,
                };
                let jobs = store.list_repl_schedules(&self.active_session_id, listing);
                Ok((
                    json!({ "jobs": jobs.iter().map(schedule_job_response).collect::<Vec<_>>() }),
                    ScheduleFact::Listed,
                    None,
                ))
            }
            "schedule.cancel" => {
                let id = data.get("id").and_then(Value::as_str).unwrap_or_default();
                let Some(job) = store.cancel_repl_schedule(&self.active_session_id, id, now)?
                else {
                    anyhow::bail!("this session has no scheduled job {id:?}");
                };
                Ok((
                    json!({ "job": schedule_job_response(&job) }),
                    ScheduleFact::Cancelled,
                    Some(RlmHeartbeatMutation {
                        job,
                        drop_queued: true,
                    }),
                ))
            }
            other => anyhow::bail!("unknown schedule request type {other:?}"),
        }
    }
}

/// The `snake_case` job the kernel's `schedule` client parses.
fn schedule_job_response(job: &AgentCronJob) -> Value {
    json!({
        "id": job.id,
        "prompt": job.prompt,
        "label": job.label,
        "kind": match job.schedule.kind {
            ScheduleKind::Once => "once",
            ScheduleKind::Interval => "interval",
            ScheduleKind::Cron => "cron",
        },
        "schedule": job.schedule.expression,
        "status": match job.status {
            JobStatus::Active => "active",
            JobStatus::Paused => "paused",
            JobStatus::Completed => "completed",
            JobStatus::Cancelled => "cancelled",
        },
        "next_run_at": job.next_run_at,
        "last_run_at": job.last_run_at,
        "run_count": job.run_count,
        "last_error": job.last_error,
        "created_at": job.created_at,
    })
}

#[cfg(test)]
mod tests {
    use tokio::sync::Mutex;

    use super::*;
    use crate::kernel::shared::HostRequestPayload;
    use crate::session::manager::SessionManager;
    use crate::session_engine::host_requests::SessionBinding;

    fn runtime(store: Option<Arc<AgentCronJobStore>>) -> (tempfile::TempDir, SessionRuntime) {
        let dir = tempfile::TempDir::new().unwrap();
        let session = SessionManager::in_memory(dir.path());
        let runtime = SessionRuntime::new(
            &session,
            store,
            "live-1".to_string(),
            SessionBinding {
                session_id: "session-1".to_string(),
                session_file: "/w/s.jsonl".to_string(),
                cwd: "/w".to_string(),
            },
        );
        (dir, runtime)
    }

    async fn call(
        handlers: &HostRequestHandlers,
        request_type: &str,
        data: Value,
    ) -> anyhow::Result<Value> {
        let handler = handlers.get(request_type).expect("registered").clone();
        handler(HostRequestPayload { data }).await
    }

    /// The kernel adds, lists, and cancels its own jobs; the user's job on the same session is
    /// neither listed nor cancellable; every mutation re-arms the scheduler.
    #[tokio::test]
    async fn schedule_requests_reach_only_the_sessions_own_jobs() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(AgentCronJobStore::new(dir.path().join("jobs.json")));
        let users = store
            .create(&CreateAgentCronJobInput {
                active_session_id: "live-1".to_string(),
                prompt: "user job".to_string(),
                schedule_text: "every 1h".to_string(),
                ..Default::default()
            })
            .unwrap();
        let (_session_dir, mut runtime) = runtime(Some(Arc::clone(&store)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&mutations);
        runtime.set_cron_mutation_hook(Arc::new(move |mutation: RlmHeartbeatMutation| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock()
                    .await
                    .push((mutation.job.id, mutation.drop_queued));
            })
        }));
        let mut handlers = HostRequestHandlers::default();
        runtime.register_schedule_handlers(&mut handlers);

        let added = call(
            &handlers,
            "schedule.add",
            json!({ "prompt": "check CI", "every": "30m", "label": "ci" }),
        )
        .await
        .unwrap();
        let job = &added["job"];
        let id = job["id"].as_str().unwrap().to_string();
        assert_eq!(
            json!({ "kind": job["kind"], "schedule": job["schedule"], "status": job["status"], "label": job["label"], "prompt": job["prompt"] }),
            json!({ "kind": "interval", "schedule": "every 30m", "status": "active", "label": "ci", "prompt": "check CI" })
        );
        let listed = call(&handlers, "schedule.list", json!({})).await.unwrap();
        assert_eq!(listed, json!({ "jobs": [job.clone()] }));
        assert_eq!(
            call(&handlers, "schedule.cancel", json!({ "id": users.id }))
                .await
                .unwrap_err()
                .to_string(),
            format!("this session has no scheduled job {:?}", users.id)
        );
        let cancelled = call(&handlers, "schedule.cancel", json!({ "id": id }))
            .await
            .unwrap();
        assert_eq!(cancelled["job"]["status"], "cancelled");
        assert_eq!(
            call(
                &handlers,
                "schedule.add",
                json!({ "prompt": "x", "at": "in 5m", "cron": "@daily" })
            )
            .await
            .unwrap_err()
            .to_string(),
            "pass exactly one of at=, every=, or cron="
        );
        assert_eq!(
            *mutations.lock().await,
            vec![(id.clone(), false), (id, true)]
        );
        assert_eq!(
            store
                .list()
                .into_iter()
                .find(|job| job.id == users.id)
                .map(|job| job.status),
            Some(JobStatus::Active)
        );
    }

    /// Without a scheduler every request says why instead of accepting a job nothing would fire.
    #[tokio::test]
    async fn without_a_scheduler_every_request_refuses_with_the_reason() {
        let (_session_dir, runtime) = runtime(None);
        let mut handlers = HostRequestHandlers::default();
        runtime.register_schedule_handlers(&mut handlers);
        for (request_type, data) in [
            (
                "schedule.add",
                json!({ "prompt": "check CI", "at": "in 30m" }),
            ),
            ("schedule.list", json!({})),
            ("schedule.cancel", json!({ "id": "job-1" })),
        ] {
            assert_eq!(
                call(&handlers, request_type, data)
                    .await
                    .unwrap_err()
                    .to_string(),
                NO_SCHEDULER
            );
        }
    }
}
