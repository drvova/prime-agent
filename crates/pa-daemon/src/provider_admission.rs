//! Daemon-wide provider admission: one budget per provider key shared by every worker.
//!
//! The daemon runs one worker process per session (subagents included), so a limiter inside a
//! worker cannot see its siblings. The supervisor owns the shared state ([`AdmissionService`]:
//! pa-ai's adaptive [`LocalAdmission`] pool plus lease bookkeeping and persistence); each
//! worker installs a [`SupervisorAdmission`] source that takes permits over the supervisor link
//! (`provider_admission_acquire` / `provider_admission_release`). A supervisor that does not
//! answer within a bounded wait never stalls a worker: the request falls back to the worker's
//! own local pool.
//!
//! The learned controller state persists to `<agent-dir>/provider-pacing.json` (keys carry a
//! credential hash, never a credential), written off the request path after a short debounce,
//! and seeds the pool on the first request after a supervisor restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_ai::admission::{
    AdmissionKey, AdmissionRequest, AdmissionSource, Grant, GrantFuture, LocalAdmission,
};
use pa_types::control::{LearnedState, Observation};
use pa_types::provider_usage::ProviderPacing;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::supervisor_link::SupervisorLink;

/// The file the learned pacing persists to, under the agent dir.
pub(crate) const PACING_STORE_FILE: &str = "provider-pacing.json";

/// How long one acquire poll may wait in the supervisor before it answers "not yet".
const ACQUIRE_POLL: Duration = Duration::from_secs(1);

/// Slack over [`ACQUIRE_POLL`] before a worker treats the supervisor as hung and falls back to
/// its local pool.
const LINK_GRACE: Duration = Duration::from_secs(1);

/// A lease older than this is reclaimed (its worker died holding it); the longest model stream
/// fits well inside it.
const LEASE_TTL: Duration = Duration::from_mins(30);

/// The debounce between a learned change and its write.
const PERSIST_DEBOUNCE: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
struct PacingStore {
    version: u32,
    entries: Vec<PacingEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PacingEntry {
    key: AdmissionKey,
    learned: LearnedState,
}

struct Lease {
    grant: Grant,
    granted_at: std::time::Instant,
}

/// The supervisor's shared admission state.
pub(crate) struct AdmissionService {
    pool: LocalAdmission,
    leases: Mutex<HashMap<u64, Lease>>,
    next_lease: AtomicU64,
    store_path: PathBuf,
    loaded: tokio::sync::OnceCell<()>,
    persist_scheduled: AtomicBool,
}

impl AdmissionService {
    pub(crate) fn new(agent_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            pool: LocalAdmission::new(pa_ai::admission::provider_controller_config()),
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
            store_path: agent_dir.join(PACING_STORE_FILE),
            loaded: tokio::sync::OnceCell::new(),
            persist_scheduled: AtomicBool::new(false),
        })
    }

    /// Seed the pool from the persisted state once; an absent or unreadable file starts fresh.
    async fn ensure_loaded(&self) {
        self.loaded
            .get_or_init(|| async {
                let path = self.store_path.clone();
                let store = tokio::task::spawn_blocking(move || {
                    let text = std::fs::read_to_string(path).ok()?;
                    serde_json::from_str::<PacingStore>(&text).ok()
                })
                .await
                .ok()
                .flatten();
                if let Some(store) = store {
                    self.pool.seed(
                        store
                            .entries
                            .into_iter()
                            .map(|entry| (entry.key, entry.learned)),
                    );
                }
            })
            .await;
    }

    /// Wait up to `wait` for a permit: the lease id and the time waited, or `None` (not yet).
    pub(crate) async fn acquire(
        &self,
        request: &AdmissionRequest,
        wait: Duration,
    ) -> Option<(u64, u64)> {
        self.ensure_loaded().await;
        self.reclaim_expired_leases();
        let grant = self
            .pool
            .acquire_until(request, Some(tokio::time::Instant::now() + wait))
            .await?;
        // The waiter's own clock: polls before this one count too.
        let waited_ms = pa_ai::admission::now_ms().saturating_sub(request.enqueued_at_ms);
        let lease_id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                lease_id,
                Lease {
                    grant,
                    granted_at: std::time::Instant::now(),
                },
            );
        Some((lease_id, waited_ms))
    }

    /// Return a lease with its observation; `true` when the controller cut capacity. An
    /// unknown lease (reclaimed, or a restart in between) is ignored.
    pub(crate) fn release(self: &Arc<Self>, lease_id: u64, observation: Observation) -> bool {
        let lease = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&lease_id);
        let Some(lease) = lease else {
            return false;
        };
        let cut = lease.grant.finish(observation);
        self.schedule_persist();
        cut
    }

    /// The read-only controller view for the `/usage` page.
    pub(crate) fn pacing(&self) -> Vec<ProviderPacing> {
        self.pool.pacing()
    }

    fn reclaim_expired_leases(&self) {
        let expired: Vec<Lease> = {
            let mut leases = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids: Vec<u64> = leases
                .iter()
                .filter(|(_, lease)| lease.granted_at.elapsed() >= LEASE_TTL)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| leases.remove(id)).collect()
        };
        // Dropping an unfinished grant returns its permit without a capacity signal.
        drop(expired);
    }

    /// Write the learned state after a short debounce, off the request path: one write in flight
    /// at a time, the latest state wins.
    fn schedule_persist(self: &Arc<Self>) {
        if self.persist_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.persist_scheduled.store(false, Ordering::Release);
            return;
        };
        let service = Arc::clone(self);
        runtime.spawn(async move {
            tokio::time::sleep(PERSIST_DEBOUNCE).await;
            service.persist_scheduled.store(false, Ordering::Release);
            if let Err(error) = service.persist().await {
                pa_ai::utils::log::get_logger("provider-admission").warn(
                    "provider pacing was not saved",
                    json!({ "error": format!("{error:#}") }),
                );
            }
        });
    }

    /// Write the learned state now.
    pub(crate) async fn persist(&self) -> anyhow::Result<()> {
        let store = PacingStore {
            version: 1,
            entries: self
                .pool
                .learned()
                .into_iter()
                .map(|(key, learned)| PacingEntry { key, learned })
                .collect(),
        };
        let path = self.store_path.clone();
        let text = serde_json::to_string_pretty(&store)?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let staging = path.with_extension("json.tmp");
            std::fs::write(&staging, text)?;
            std::fs::rename(&staging, &path)?;
            Ok(())
        })
        .await?
    }

    /// The learned state per key (tests compare it whole across a restart).
    #[cfg(all(test, unix))]
    pub(crate) fn learned(&self) -> Vec<(AdmissionKey, LearnedState)> {
        self.pool.learned()
    }
}

/// Answer one admission command (the supervisor authenticated the worker first).
pub(crate) async fn handle_admission_command(
    service: &Arc<AdmissionService>,
    command: &pa_types::daemon::DaemonCommand,
) -> anyhow::Result<Value> {
    use pa_types::daemon::DaemonCommand;
    match command {
        DaemonCommand::ProviderAdmissionAcquire {
            request, wait_ms, ..
        } => {
            let request: AdmissionRequest = serde_json::from_value(request.clone())?;
            let wait = Duration::from_millis(*wait_ms).min(ACQUIRE_POLL);
            Ok(match service.acquire(&request, wait).await {
                Some((lease_id, waited_ms)) => {
                    json!({ "granted": true, "leaseId": lease_id, "waitedMs": waited_ms })
                }
                None => json!({ "granted": false }),
            })
        }
        DaemonCommand::ProviderAdmissionRelease {
            lease_id,
            observation,
            ..
        } => Ok(json!({ "decreased": service.release(*lease_id, *observation) })),
        other => anyhow::bail!(
            "not an admission command: {}",
            crate::protocol::command_type_name(other)
        ),
    }
}

/// A worker's permit source: the supervisor's daemon-wide budget, with the worker's own pool as
/// the fallback when the supervisor does not answer in time.
pub(crate) struct SupervisorAdmission {
    link: Arc<SupervisorLink>,
    worker_token: String,
}

impl SupervisorAdmission {
    pub(crate) fn new(link: Arc<SupervisorLink>, worker_token: String) -> Self {
        Self { link, worker_token }
    }
}

impl AdmissionSource for SupervisorAdmission {
    fn acquire(&self, request: AdmissionRequest) -> GrantFuture {
        let link = Arc::clone(&self.link);
        let worker_token = self.worker_token.clone();
        Box::pin(async move {
            loop {
                let answer = link
                    .request_success(
                        json!({
                            "type": "provider_admission_acquire",
                            "workerToken": worker_token,
                            "request": request,
                            "waitMs": u64::try_from(ACQUIRE_POLL.as_millis()).unwrap_or(u64::MAX),
                        }),
                        ACQUIRE_POLL + LINK_GRACE,
                    )
                    .await;
                let answer = match answer {
                    Ok(answer) => answer,
                    Err(error) => {
                        pa_ai::utils::log::get_logger("provider-admission").warn(
                            "provider admission fell back to the local pool",
                            json!({ "error": format!("{error:#}") }),
                        );
                        return pa_ai::admission::local().acquire(request).await;
                    }
                };
                if answer.get("granted").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let lease_id = answer.get("leaseId").and_then(Value::as_u64).unwrap_or(0);
                let waited_ms = answer.get("waitedMs").and_then(Value::as_u64).unwrap_or(0);
                let link = Arc::clone(&link);
                let worker_token = worker_token.clone();
                return Grant::new(
                    waited_ms,
                    Box::new(move |observation| {
                        // Fire-and-forget: the request already finished; the supervisor's
                        // verdict on a capacity cut arrives later and is counted then.
                        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                            runtime.spawn(async move {
                                let released = link
                                    .request_success(
                                        json!({
                                            "type": "provider_admission_release",
                                            "workerToken": worker_token,
                                            "leaseId": lease_id,
                                            "observation": observation,
                                        }),
                                        ACQUIRE_POLL + LINK_GRACE,
                                    )
                                    .await;
                                if released
                                    .ok()
                                    .and_then(|data| data.get("decreased").and_then(Value::as_bool))
                                    == Some(true)
                                {
                                    pa_ai::admission::note_decrease();
                                }
                            });
                        }
                        false
                    }),
                );
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests;
