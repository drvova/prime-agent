//! The update boot's restore (spec §6), shared by both daemons: the boot
//! sweep, the roster consumed from the spawn env, the restore pass over the
//! roster's rows ([`restore_pass`], driven on a daemon through its
//! [`RestoreHost`]), the TS manifest's replay, and the pass's progress
//! ([`RestoreProgress`]: the hello resume contract, the
//! `update_restore_status` reply, the queue a reconnecting client waits in).
//! Moved from pa-daemon's `update_restore.rs`, unchanged.
//!
//! THE NO-AUTO-RESUME CONTRACT: a boot never creates a session the user did
//! not ask for; due heartbeats stay dormant (restore never fails the boot).

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use pa_types::daemon::update_flow::DaemonUpdateResume;
use pa_types::daemon::update_flow::{
    legacy_update_restart_status, legacy_update_restarts_dir, socket_update_dir, UpdateRoster,
    UpdateRosterSession, UpdateStatusCounts, UpdateStatusFailure, UPDATE_ROSTER_ENV,
};
use pa_types::daemon::UpdateId;
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::socket_hash;
use crate::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;

/// The client id a daemon opens roster rows' sessions under.
pub const UPDATE_RESTORE_CLIENT_ID: &str = "update-restore";

/// How long a client attach queues behind an in-flight restore pass (spec §10.4).
const RESTORE_ATTACH_WAIT_MS: u64 = 120_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `suffix` names `candidate` the way the daemon's session registry
/// resolves a selector: a case- and dash-insensitive suffix.
#[must_use]
pub fn selector_matches(candidate: &str, suffix: &str) -> bool {
    let normalize = |value: &str| -> String { value.replace('-', "").to_lowercase() };
    let candidate = normalize(candidate);
    let suffix = normalize(suffix);
    !candidate.is_empty() && !suffix.is_empty() && candidate.ends_with(&suffix)
}

/// One old worker command's answer, as the restore pass reads it: the old
/// response's `success`, `error` and `data`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteReply {
    pub success: bool,
    pub error: Option<String>,
    pub data: Option<Value>,
}

impl RouteReply {
    /// A success with `data`.
    #[must_use]
    pub fn ok(data: Option<Value>) -> Self {
        RouteReply {
            success: true,
            error: None,
            data,
        }
    }

    /// A refusal with `error`.
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        RouteReply {
            success: false,
            error: Some(error.into()),
            data: None,
        }
    }
}

// Shared restore state (hello contract, status RPC, queued attaches)

/// One roster row's settle outcome, for attach queuing (spec §10.4).
#[derive(Debug, Clone)]
struct RestoreTarget {
    active_session_id: String,
    session_file: String,
    /// The row's session name: `SessionRegistry::resolve` accepts name selectors.
    name: Option<String>,
    /// The row needs the TS-parity continuation treatment (§10.5): an
    /// early settle must not wake its waiters ahead of the prompt.
    needs_continuation: bool,
    /// Set the moment the pass finishes this row (or adoption brings the worker up).
    settled: bool,
    failure: Option<String>,
}

/// The supervisor's restore pass state (hello contract, `update_restore_status` RPC, attach queue):
/// a brief `std` mutex, no awaits inside; waiters park on the notify.
#[derive(Debug, Default)]
pub struct RestoreProgress {
    update_id: Mutex<Option<UpdateId>>,
    state: Mutex<RestoreInner>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct RestoreInner {
    done: bool,
    /// Bumped on every row settle: the per-row waiters' budget re-arms
    /// while the pass keeps making progress.
    settled_generation: u64,
    targets: BTreeMap<String, RestoreTarget>,
    counts: UpdateStatusCounts,
    failures: Vec<UpdateStatusFailure>,
}

impl RestoreProgress {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the boot's update identity before serving, and register
    /// the roster rows: a mid-recovery reconnect queues behind its row (spec §10.4).
    pub fn begin(&self, roster: Option<&UpdateRoster>) {
        *lock(&self.update_id) = roster.map(|roster| roster.update_id.clone());
        if let Some(roster) = roster {
            self.register_targets(roster);
        }
    }

    pub fn update_id(&self) -> Option<UpdateId> {
        lock(&self.update_id).clone()
    }

    /// The hello resume contract (spec §10.3).
    pub fn hello_resume(&self) -> DaemonUpdateResume {
        let state = lock(&self.state);
        DaemonUpdateResume {
            update_id: self.update_id(),
            complete: state.done,
        }
    }

    /// Register the roster rows the restore pass will settle.
    fn register_targets(&self, roster: &UpdateRoster) {
        let mut state = lock(&self.state);
        for row in &roster.sessions {
            state.targets.insert(
                row.session_id.clone(),
                RestoreTarget {
                    active_session_id: row.active_session_id.clone(),
                    session_file: row.session_file.clone(),
                    name: row.name.clone(),
                    needs_continuation: roster
                        .rest
                        .get("legacy_ts_restart")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                        || (row.should_resume && row.in_flight.streaming),
                    settled: false,
                    failure: None,
                },
            );
        }
    }

    /// Record one row's settle outcome the moment the recovery finishes
    /// it: the row's waiters wake immediately. Idempotent.
    pub fn settle_target(&self, selector: &str, failure: Option<String>) {
        {
            let mut state = lock(&self.state);
            let Some(target) = restore_target_mut(&mut state.targets, selector) else {
                return;
            };
            if target.settled {
                return;
            }
            target.settled = true;
            target.failure = failure;
            state.settled_generation += 1;
        }
        self.notify.notify_waiters();
    }

    /// Settle a row an adoption or live (re-)registration brought up, not
    /// the pass itself; a row pending its continuation treatment (§10.5)
    /// stays queued — the skip still bumps the generation.
    pub fn settle_adopted(&self, selector: &str) {
        {
            let mut state = lock(&self.state);
            let Some(target) = restore_target_mut(&mut state.targets, selector) else {
                return;
            };
            if target.settled {
                return;
            }
            if target.needs_continuation {
                state.settled_generation += 1;
            } else {
                target.settled = true;
                state.settled_generation += 1;
            }
        }
        self.notify.notify_waiters();
    }

    /// Mark the pass settled: per-row outcomes, counts, and the waiters' wakeup. Idempotent.
    fn settle(&self, counts: UpdateStatusCounts, failures: Vec<UpdateStatusFailure>) {
        {
            let mut state = lock(&self.state);
            if state.done {
                return;
            }
            state.done = true;
            state.counts = counts;
            let mut by_file: BTreeMap<&str, &str> = BTreeMap::new();
            for failure in &failures {
                by_file.insert(failure.session_file.as_str(), failure.message.as_str());
            }
            for target in state.targets.values_mut() {
                target.settled = true;
                target.failure = by_file
                    .get(target.session_file.as_str())
                    .map(std::string::ToString::to_string);
            }
            state.settled_generation += 1;
            state.failures = failures;
        }
        self.notify.notify_waiters();
    }

    /// One row's failure message once settled, if any (spec §10.4).
    fn settled_failure(&self, selector: &str) -> Option<(String, String)> {
        let state = lock(&self.state);
        let target = restore_target(&state.targets, selector)?;
        target
            .failure
            .as_ref()
            .map(|message| (target.session_file.clone(), message.clone()))
    }

    /// Whether an in-flight pass owns the selector: any roster row the
    /// registry-shaped selector resolves to.
    fn owns_target(&self, selector: &str) -> bool {
        let state = lock(&self.state);
        !state.done && restore_target(&state.targets, selector).is_some()
    }

    /// Spec §10.4: queues behind an in-flight restore pass that owns the
    /// selector (any selector shape the daemon's session registry
    /// accepts), then returns; at once when no pass is in flight or the
    /// selector is not a roster row, so an unrelated dead session fails
    /// immediately.
    pub async fn await_target(&self, selector: &str) {
        if !self.owns_target(selector) {
            return;
        }
        // Queue behind this session's own row only (spec §10.4).
        self.wait_for_settle_target(selector).await;
    }

    /// Spec §10.4: the settled per-row failure, as the typed attach error
    /// with the session file path and the manual-resume hint.
    #[must_use]
    pub fn failure_for(&self, selector: &str) -> Option<String> {
        self.settled_failure(selector)
            .map(|(session_file, message)| {
                format!(
                    "Session {selector} failed to restore: {message}. The session file is at \
                     {session_file} — run `prime-agent attach` to resume it manually."
                )
            })
    }

    /// The `update_restore_status` reply (the coordinator's `Restoring`
    /// report, spec §9).
    #[must_use]
    pub fn status_body(&self) -> Value {
        let state = lock(&self.state);
        json!({
            "updateId": self.update_id().map(|id| id.to_string()),
            "inFlight": !state.done,
            "complete": state.done,
            "counts": {
                "total": state.counts.total,
                "restored": state.counts.restored,
                "resumed": state.counts.resumed,
                "failed": state.counts.failed,
            },
            "failures": state.failures,
        })
    }

    /// Wait for one target's settle outcome, not the whole pass (spec
    /// §10.4). The deadline bounds the queue's quiet time: every settle
    /// progress re-arms it, so a wedged pass cannot hold a client.
    async fn wait_for_settle_target(&self, selector: &str) {
        let quiet = std::time::Duration::from_millis(RESTORE_ATTACH_WAIT_MS.max(1));
        let (mut last_generation, mut deadline) = {
            let state = lock(&self.state);
            (
                state.settled_generation,
                tokio::time::Instant::now() + quiet,
            )
        };
        loop {
            // Register interest before re-checking: a settle that runs
            // between the check and the registration must still wake us.
            let notified = self.notify.notified();
            let (settled, generation) = {
                let state = lock(&self.state);
                (
                    state.done
                        || restore_target(&state.targets, selector)
                            .is_none_or(|target| target.settled),
                    state.settled_generation,
                )
            };
            if settled {
                return;
            }
            if generation != last_generation {
                last_generation = generation;
                deadline = tokio::time::Instant::now() + quiet;
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }
}

/// Whether one target answers the registry-shaped selector (the durable
/// id is the map key, checked first) — the same shapes `SessionRegistry::resolve` accepts.
fn matches_selector(target: &RestoreTarget, selector: &str) -> bool {
    if target.active_session_id == selector {
        return true;
    }
    let stem = Path::new(&target.session_file)
        .file_stem()
        .map_or_default(|stem| stem.to_string_lossy().to_string());
    if selector_matches(&target.active_session_id, selector)
        || (!stem.is_empty() && selector_matches(&stem, selector))
    {
        return true;
    }
    target
        .name
        .as_deref()
        .is_some_and(|name| !name.is_empty() && name == selector)
}

/// The map key a selector addresses: the exact durable id, or the key
/// of the single row the selector matches (ambiguous matches nothing).
fn restore_target_key(targets: &BTreeMap<String, RestoreTarget>, selector: &str) -> Option<String> {
    if targets.contains_key(selector) {
        return Some(selector.to_string());
    }
    let mut matches = targets
        .iter()
        .filter(|(_, target)| matches_selector(target, selector));
    let (key, _) = matches.next()?;
    matches.next().is_none().then(|| key.clone())
}

/// The roster row a selector addresses: any selector shape the registry
/// accepts, resolved to exactly one row.
fn restore_target<'a>(
    targets: &'a BTreeMap<String, RestoreTarget>,
    selector: &str,
) -> Option<&'a RestoreTarget> {
    restore_target_key(targets, selector).and_then(|key| targets.get(&key))
}

/// The mutable counterpart of [`restore_target`].
fn restore_target_mut<'a>(
    targets: &'a mut BTreeMap<String, RestoreTarget>,
    selector: &str,
) -> Option<&'a mut RestoreTarget> {
    restore_target_key(targets, selector).and_then(|key| targets.get_mut(&key))
}

// Spec §6 step 1: the boot sweep

/// Delete this socket's scratch directory and stale legacy names after consuming
/// the roster env. Keep a TS manifest while its socket-bound prepare attempt is
/// pending: an ordinary boot after a lost reply must not destroy recovery input.
/// Flat TS status records remain available to callers waiting for their result.
pub fn boot_sweep(agent_dir: &Path, socket_path: &Path) {
    let _ = std::fs::remove_dir_all(socket_update_dir(agent_dir, &socket_hash(socket_path)));
    let legacy_dir = legacy_update_restarts_dir(agent_dir);
    if legacy_dir.is_symlink() {
        // Never walk a symlink while deleting scratch entries.
        let _ = std::fs::remove_file(&legacy_dir);
    } else if let Ok(entries) = std::fs::read_dir(&legacy_dir) {
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("daemon: could not inspect legacy update scratch: {error}");
                    continue;
                }
            };
            let path = entry.path();
            let pending = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|key| {
                    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                        return false;
                    }
                    let attempt_path = agent_dir
                        .join("legacy-update-recovery")
                        .join(format!("{key}.attempt.json"));
                    let attempt = std::fs::read(attempt_path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
                    let Some(attempt) = attempt else {
                        return false;
                    };
                    let Some(socket) = attempt["socket"]
                        .as_str()
                        .filter(|socket| !socket.is_empty())
                    else {
                        return false;
                    };
                    // Retain attempts for every socket, including older target versions.
                    // The CLI owns full predecessor/freshness validation and removes the
                    // attempt only after confirmed restoration or a safe prepare refusal.
                    socket_hash(Path::new(socket)) == key
                        && attempt["version"]
                            .as_str()
                            .is_some_and(|version| !version.is_empty())
                        && attempt["updateId"]
                            .as_str()
                            .is_some_and(|id| !id.is_empty())
                        && attempt["startedAt"].as_u64().is_some()
                        && attempt["hello"]["supervisorPid"]
                            .as_u64()
                            .is_some_and(|pid| pid > 0)
                        && attempt["hello"]["supervisorProcessStartId"]
                            .as_str()
                            .is_some_and(|id| !id.is_empty())
                        && attempt["hello"]["supervisorGeneration"]
                            .as_str()
                            .is_some_and(|generation| !generation.is_empty())
                });
            if !pending {
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(path);
                } else {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        // Only remove an empty directory; pending sources keep their original paths.
        let _ = std::fs::remove_dir(&legacy_dir);
    }
    let _ = std::fs::remove_file(legacy_update_restart_status(agent_dir));
}

// Spec §6 step 2: consume the roster from the spawn env

/// Read `PRIME_AGENT_UPDATE_ROSTER`: a coordinator-passed path, never
/// discovered. `None` on a normal boot; a malformed roster never fails the boot (spec §9).
#[must_use]
pub fn consume_roster_env() -> Option<UpdateRoster> {
    let path = pa_types::session_scope::var(UPDATE_ROSTER_ENV).ok()?;
    let path = Path::new(&path);
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            eprintln!(
                "daemon: could not read the update roster at {}: {error}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(roster) => Some(roster),
        Err(error) => {
            eprintln!(
                "daemon: malformed update roster at {}: {error}",
                path.display()
            );
            None
        }
    }
}

// Spec §6 steps 2-3: the restore pass

/// What the restore pass drives on the daemon booting for an update: its
/// sessions, found, opened and sent the old worker commands the pass needs.
///
/// Implementations answer each method from the daemon's own sessions:
/// `find` must see a session another path already brought up (an adoption, a
/// revival, a client's open), so the pass never opens one twice; `open` opens
/// a row's session as the daemon's own create would, and fails (never
/// panics) when it cannot; `route` runs one command on the session and
/// answers it as the old worker answered it (`success`, `error`, `data`): a
/// transport failure is an `Err`, a refusal a reply with `success: false`.
pub trait RestoreHost: Sync {
    /// One of the daemon's open sessions.
    type Session: Send + Sync;

    /// The session already serving `row`'s transcript, if any.
    fn find(&self, row: &UpdateRosterSession)
        -> impl Future<Output = Option<Self::Session>> + Send;

    /// Opens `row`'s session from the create command its roster row
    /// carries; for a TS roster, `restored_ids` maps the old active handles
    /// to the restored sessions' ([`remap_legacy_parent`]).
    fn open(
        &self,
        row: &UpdateRosterSession,
        restored_ids: Option<&BTreeMap<String, String>>,
    ) -> impl Future<Output = Result<Self::Session>> + Send;

    /// The active id `session` is addressed by.
    fn active_session_id(&self, session: &Self::Session) -> impl Future<Output = String> + Send;

    /// Runs the old worker command `command` with `payload` on `session`:
    /// `prompt`, and for a TS roster `acquire_session_input_pause`,
    /// `append_custom_message`, `restore_next_turn`, `restore_actions`,
    /// `abort` and `release_session_input_pause`.
    fn route(
        &self,
        session: &Self::Session,
        command: &'static str,
        payload: Value,
    ) -> impl Future<Output = Result<RouteReply>> + Send;

    /// Writes `line` to the daemon's log.
    fn log(&self, line: &str);

    /// Reports the due scheduled jobs of the sessions that stayed down
    /// (the no-auto-resume contract; [`report_dormant_jobs`]); run after the
    /// replay.
    fn report_dormant(&self) -> impl Future<Output = ()> + Send;
}

/// The restore pass: run once whatever brings sessions up at boot settled,
/// then walk Rust rows bottom-up (spec §8), or TS rows parent-first so their
/// fresh active handles can be remapped. NOT-RUNNING sessions stay down; the
/// dormant report runs AFTER the replay. `progress` began with the same
/// roster ([`RestoreProgress::begin`]).
pub async fn restore_pass<H: RestoreHost>(
    host: &H,
    progress: &RestoreProgress,
    roster: Option<UpdateRoster>,
) {
    let Some(roster) = roster else {
        host.report_dormant().await;
        progress.settle(UpdateStatusCounts::default(), Vec::new());
        return;
    };
    let mut rows: Vec<&UpdateRosterSession> = roster.sessions.iter().collect();
    let legacy = roster
        .rest
        .get("legacy_ts_restart")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if legacy {
        rows.sort_by_key(|row| row.rlm_depth);
    } else {
        sort_rows_bottom_up(&mut rows);
    }
    let mut restored_ids = BTreeMap::new();
    let mut counts = UpdateStatusCounts::default();
    let mut failures: Vec<UpdateStatusFailure> = Vec::new();
    for row in rows {
        counts.total += 1;
        let restored = match host.find(row).await {
            Some(session) => Ok(session),
            None => host.open(row, legacy.then_some(&restored_ids)).await,
        };
        let restored = match restored {
            Ok(session) if legacy => {
                restored_ids.insert(
                    row.active_session_id.clone(),
                    host.active_session_id(&session).await,
                );
                let origin = roster
                    .rest
                    .get("restart_origin_active_session_id")
                    .and_then(serde_json::Value::as_str);
                match restore_legacy_session(
                    row,
                    origin,
                    &roster.binary.to_version,
                    |command, payload| host.route(&session, command, payload),
                )
                .await
                {
                    Ok(resumed) => {
                        counts.resumed += u64::from(resumed);
                        Ok(session)
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(session) => {
                progress.settle_target(&row.session_id, None);
                continuation_treatment(host, &session, row, &mut counts).await;
                Ok(session)
            }
            Err(error) => Err(error),
        };
        match restored {
            Ok(_) => {
                counts.restored += 1;
                progress.settle_target(&row.session_id, None);
            }
            Err(error) => {
                let message = format!("{error:#}");
                host.log(&format!(
                    "update restore: could not restore {}: {message}",
                    row.session_file
                ));
                progress.settle_target(&row.session_id, Some(message.clone()));
                failures.push(UpdateStatusFailure {
                    session_file: row.session_file.clone(),
                    message,
                });
                counts.failed += 1;
            }
        }
    }
    // The dormant report runs here, AFTER the replay: only the sessions that stayed down count.
    host.report_dormant().await;
    progress.settle(counts, failures);
}

/// Order roster rows bottom-up (spec §8): deepest first so parents attach to existing children;
/// subagents before top-level rows of equal depth; stable by session id otherwise.
fn sort_rows_bottom_up(rows: &mut [&UpdateRosterSession]) {
    rows.sort_by(|a, b| {
        b.rlm_depth.cmp(&a.rlm_depth).then_with(|| {
            let subagent = |row: &UpdateRosterSession| {
                row.kind == pa_types::daemon::update_flow::UpdateRosterSessionKind::Subagent
            };
            u8::from(subagent(b)).cmp(&u8::from(subagent(a)))
        })
    });
}

/// Replay the TS manifest, whose queues have no Rust recovery journal. The
/// command boundary is injectable so regression tests exercise the same ordering
/// and acknowledgement handling as the live daemon route. Whether the row
/// resumed work.
///
/// # Errors
///
/// A command failed or was refused (the input pause, once taken, is
/// released first).
pub async fn restore_legacy_session<F, Fut>(
    row: &UpdateRosterSession,
    origin: Option<&str>,
    version: &str,
    mut route: F,
) -> Result<bool>
where
    F: FnMut(&'static str, serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<RouteReply>>,
{
    let actions = row
        .queue
        .actions
        .get("actions")
        .and_then(serde_json::Value::as_array);
    let has_actions = actions.is_some_and(|actions| !actions.is_empty());
    // restore_actions wakes Rust's runner immediately. Hold its existing input
    // pause across queue replay and continuation admission, including unrelated
    // wakeups, so a follow-up cannot overtake the interrupted turn.
    let pause = if row.should_resume && has_actions {
        let response = route(
            "acquire_session_input_pause",
            json!({
                "leaseKey": "legacy-update-restore", "clientId": UPDATE_RESTORE_CLIENT_ID,
            }),
        )
        .await?;
        anyhow::ensure!(
            response.success,
            "legacy update acquire_session_input_pause: {}",
            response.error.unwrap_or_default()
        );
        Some(
            response
                .data
                .as_ref()
                .and_then(|data| data.get("pauseId"))
                .and_then(serde_json::Value::as_str)
                .context("legacy update input pause returned no pause id")?
                .to_string(),
        )
    } else {
        None
    };
    let outcome: Result<bool> = async {
        let mut commands = Vec::new();
        if origin == Some(row.active_session_id.as_str()) {
            commands.push(("append_custom_message", json!({ "message": {
                "customType": "prime-agent.update_complete",
                "content": format!("[update-complete]\n\nPrime Agent updated to v{version}. This daemon session was restored after the update."),
                "display": true,
                "details": { "version": version },
            }})));
        }
        if !row.queue.next_turn.is_empty() {
            commands.push((
                "restore_next_turn",
                json!({ "messages": row.queue.next_turn }),
            ));
        }
        if row.should_resume && has_actions {
            commands.push(("restore_actions", json!({ "snapshot": row.queue.actions })));
        }
        for (command, payload) in commands {
            let response = route(command, payload)
                .await
                .with_context(|| format!("legacy update {command}"))?;
            anyhow::ensure!(
                response.success,
                "legacy update {command}: {}",
                response.error.unwrap_or_default()
            );
        }
        if !row.should_resume {
            return Ok(false);
        }
        let accepted_turn = actions.is_some_and(|actions| {
            actions.iter().any(|action| {
                let payload = &action["payload"];
                payload["kind"] == "turn"
                    && payload["queueVisible"] == false
                    && payload["acceptedBeforeCompletion"] == true
            })
        });
        let in_flight = row.in_flight;
        let interrupted = in_flight.streaming
            || in_flight.compacting
            || in_flight.bash_running
            || in_flight.rlm_children
            || in_flight.retrying
            || in_flight.prompt_in_flight;
        let mut resumed = false;
        if interrupted && !accepted_turn {
            let response = route(
                "prompt",
                json!({
                    "message": UPDATE_RESTART_CONTINUATION_PROMPT, "expandPromptTemplates": false,
                }),
            )
            .await?;
            resumed = response.success;
            if !resumed {
                anyhow::bail!(
                    "legacy update continuation: {}",
                    response.error.unwrap_or_default()
                );
            }
        }
        Ok(resumed || has_actions)
    }
    .await;
    if let Some(pause_id) = pause {
        // A partial restore remains parked on failure, but release the temporary
        // lease so the ordinary user resume action can recover the session.
        if outcome.is_err() {
            // Best-effort abort: its failure must not skip the release below
            // and leave the input pause held.
            match route("abort", json!({})).await {
                Ok(response) if response.success => {}
                Ok(response) => eprintln!(
                    "daemon: legacy update abort after failed restore also failed: {}",
                    response.error.unwrap_or_default()
                ),
                Err(error) => eprintln!(
                    "daemon: legacy update abort after failed restore also failed: {error:#}"
                ),
            }
        }
        let response = route(
            "release_session_input_pause",
            json!({
                "pauseId": pause_id, "clientId": UPDATE_RESTORE_CLIENT_ID,
            }),
        )
        .await?;
        anyhow::ensure!(
            response.success,
            "legacy update release_session_input_pause: {}",
            response.error.unwrap_or_default()
        );
    }
    outcome
}

/// TS restores parent sessions first and substitutes their new active handles;
/// omit an unavailable parent handle instead of retaining a dead TS handle.
pub fn remap_legacy_parent(
    payload: &mut serde_json::Value,
    restored_ids: &BTreeMap<String, String>,
) {
    let Some(metadata) = payload
        .get_mut("runtimeMetadata")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if metadata.get("kind").and_then(serde_json::Value::as_str) != Some("subagent") {
        return;
    }
    let Some(old_parent) = metadata.remove("parentActiveSessionId") else {
        return;
    };
    if let Some(new_parent) = old_parent.as_str().and_then(|id| restored_ids.get(id)) {
        metadata.insert("parentActiveSessionId".into(), json!(new_parent));
    }
}

/// The TS-parity continuation treatment (spec §10.5): a row mid-turn when the snapshot was taken
/// gets the continuation prompt; a failed prompt is a resume failure, not a restore failure.
async fn continuation_treatment<H: RestoreHost>(
    host: &H,
    session: &H::Session,
    row: &UpdateRosterSession,
    counts: &mut UpdateStatusCounts,
) {
    if !row.should_resume {
        return;
    }
    if !row.in_flight.streaming {
        // Queued work: the relaunch/create replay restored the lanes.
        counts.resumed += 1;
        return;
    }
    let response = host
        .route(
            session,
            "prompt",
            json!({ "message": UPDATE_RESTART_CONTINUATION_PROMPT }),
        )
        .await;
    match response {
        Ok(response) if response.success => counts.resumed += 1,
        Ok(response) => host.log(&format!(
            "update restore: could not resume {}: {}",
            row.session_file,
            response.error.unwrap_or_default()
        )),
        Err(error) => host.log(&format!(
            "update restore: could not resume {}: {error:#}",
            row.session_file
        )),
    }
}

/// The boot's dormant-scheduled-jobs report (the no-auto-resume contract): logs the ACTIVE, DUE
/// jobs whose session is not in `live` (the canonical transcripts of the sessions running, or
/// held with their wake armed) — never wakes them. A session that went down with the previous
/// daemon is not dormant: its wake is armed.
pub fn report_dormant_jobs<S: std::hash::BuildHasher>(
    agent_dir: &Path,
    live: &HashSet<String, S>,
    log: impl Fn(&str),
) {
    let jobs = super::roster::scan_scheduled_jobs(agent_dir);
    let now = super::now_ms();
    let dormant: Vec<&crate::cron::AgentCronJob> = jobs
        .iter()
        .filter(|job| {
            job.status == crate::cron::JobStatus::Active
                && crate::cron::is_due_job(job, now)
                && !job.session_file.is_empty()
                && !live.contains(
                    &crate::session::lease::canonical_session_path(Path::new(&job.session_file))
                        .to_string_lossy()
                        .to_string(),
                )
        })
        .collect();
    if dormant.is_empty() {
        return;
    }
    // One log line per dormant job keeps the log greppable (job id + session file).
    for job in &dormant {
        log(&format!(
            "scheduled job {} on {} is due but stays dormant: no session auto-boots on daemon start (resume the session to arm its schedule; the heartbeat catalog surfaces it)",
            job.id, job.session_file
        ));
    }
    log(&format!(
        "{} due scheduled job(s) stayed dormant on not-running sessions (no auto-resume)",
        dormant.len()
    ));
}

/// The session-action recovery snapshot format `restore_actions` restores.
const SESSION_ACTION_RECOVERY_FORMAT_VERSION: u64 = 1;

/// `restore_actions`'s check of a TS session-action recovery snapshot (the
/// old worker's, moved here so both daemons refuse what it refuses, with its
/// messages): the format version, the actions array, each action's unique id
/// and its turn payload's delivery correlation, then `check` (the daemon's
/// own refusal of an action); the actions, once every one passed.
///
/// # Errors
///
/// The first refusal's message.
pub fn validate_actions(
    snapshot: Option<&Value>,
    mut check: impl FnMut(&str, &Value) -> Result<(), String>,
) -> Result<&Vec<Value>, String> {
    let Some(snapshot) = snapshot.and_then(Value::as_object) else {
        return Err("restore_actions requires a snapshot".to_string());
    };
    let format_version = snapshot
        .get("formatVersion")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if format_version != SESSION_ACTION_RECOVERY_FORMAT_VERSION {
        return Err(format!(
            "Unsupported session action recovery format version: {format_version}"
        ));
    }
    let Some(actions) = snapshot.get("actions").and_then(Value::as_array) else {
        return Err("restore_actions requires a snapshot actions array".to_string());
    };
    let mut seen_ids = HashSet::new();
    for action in actions {
        let Some(id) = action.get("id").and_then(Value::as_str) else {
            return Err("restore_actions requires an action id".to_string());
        };
        if !seen_ids.insert(id.to_string()) {
            return Err(format!("Duplicate session action id: {id}"));
        }
        let payload = action.get("payload").unwrap_or(&Value::Null);
        if payload.get("kind").and_then(Value::as_str) == Some("turn") {
            let correlated =
                payload
                    .get("records")
                    .and_then(Value::as_array)
                    .is_none_or(|records| {
                        records.iter().all(|record| {
                            record.get("ownerActionId").and_then(Value::as_str) == Some(id)
                        })
                    });
            if !correlated {
                return Err(format!(
                    "Session action {id} has invalid delivery correlation"
                ));
            }
        }
        check(id, action)?;
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::daemon::update_flow::UpdateStatusCounts;
    use serde_json::Map;

    /// A two-row roster (update `u-1`): row `a-1`, and row `durable-b`
    /// whose session-file stem (`b-2`) differs from its durable id.
    fn two_row_roster() -> UpdateRoster {
        serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "update_id": "u-1",
            "socket_path": "/tmp/s.sock",
            "created_at": "2026-01-01T00:00:00Z",
            "supervisor": { "pid": 1, "process_start_id": "p", "generation": "g" },
            "binary": { "from_version": "0.1", "to_version": "0.2" },
            "sessions": [
                {
                    "session_id": "a-1",
                    "active_session_id": "active-a",
                    "session_file": "/sessions/a-1.jsonl",
                    "name": "alpha",
                    "kind": "top-level",
                    "rlm_depth": 0,
                    "cwd": "/w",
                    "runtime_config": {},
                    "queue": { "next_turn": [], "actions": {} },
                    "in_flight": {
                        "streaming": false, "compacting": false, "bash_running": false,
                        "rlm_children": false, "retrying": false, "prompt_in_flight": false
                    },
                    "should_resume": false
                },
                {
                    "session_id": "durable-b",
                    "active_session_id": "active-b",
                    "session_file": "/sessions/b-2.jsonl",
                    "kind": "top-level",
                    "rlm_depth": 0,
                    "cwd": "/w",
                    "runtime_config": {},
                    "queue": { "next_turn": [], "actions": {} },
                    "in_flight": {
                        "streaming": false, "compacting": false, "bash_running": false,
                        "rlm_children": false, "retrying": false, "prompt_in_flight": false
                    },
                    "should_resume": false
                }
            ],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn legacy_restore_replays_context_and_actions_before_continuing_each_interrupted_kind() {
        for flag in [
            "streaming",
            "compacting",
            "bash_running",
            "rlm_children",
            "retrying",
            "prompt_in_flight",
        ] {
            let mut row = two_row_roster().sessions.remove(0);
            row.should_resume = true;
            let mut in_flight = serde_json::to_value(row.in_flight).unwrap();
            in_flight[flag] = json!(true);
            row.in_flight = serde_json::from_value(in_flight).unwrap();
            row.queue.next_turn = serde_json::from_value(json!([{
                "customType": "saved-context", "content": "keep this", "display": false,
                "timestamp": 1
            }]))
            .unwrap();
            row.queue.actions = json!({ "formatVersion": 1, "actions": [{
                "id": "queued", "payload": { "kind": "turn", "text": "next task", "queueVisible": true }
            }] });
            let mut calls = Vec::new();
            let resumed =
                restore_legacy_session(&row, Some("active-a"), "1.0.0", |command, payload| {
                    calls.push((command, payload));
                    std::future::ready(Ok(RouteReply::ok(Some(json!({ "pauseId": "test-pause" })))))
                })
                .await
                .unwrap();
            assert!(resumed, "{flag}");
            assert_eq!(
                calls,
                vec![
                    (
                        "acquire_session_input_pause",
                        json!({ "leaseKey": "legacy-update-restore", "clientId": UPDATE_RESTORE_CLIENT_ID })
                    ),
                    (
                        "append_custom_message",
                        json!({ "message": {
                            "customType": "prime-agent.update_complete",
                            "content": "[update-complete]\n\nPrime Agent updated to v1.0.0. This daemon session was restored after the update.",
                            "display": true, "details": { "version": "1.0.0" }
                        }})
                    ),
                    (
                        "restore_next_turn",
                        json!({ "messages": row.queue.next_turn })
                    ),
                    ("restore_actions", json!({ "snapshot": row.queue.actions })),
                    (
                        "prompt",
                        json!({ "message": UPDATE_RESTART_CONTINUATION_PROMPT, "expandPromptTemplates": false })
                    ),
                    (
                        "release_session_input_pause",
                        json!({ "pauseId": "test-pause", "clientId": UPDATE_RESTORE_CLIENT_ID })
                    ),
                ],
                "{flag}"
            );
        }
    }

    #[tokio::test]
    async fn legacy_restore_resumes_an_accepted_turn_without_duplicating_its_prompt() {
        let mut row = two_row_roster().sessions.remove(0);
        row.should_resume = true;
        row.in_flight.prompt_in_flight = true;
        row.queue.actions = json!({ "formatVersion": 1, "actions": [{
            "id": "accepted", "payload": { "kind": "turn", "text": "already admitted",
                "queueVisible": false, "acceptedBeforeCompletion": true }
        }] });
        let mut calls = Vec::new();
        assert!(
            restore_legacy_session(&row, None, "1.0.0", |command, payload| {
                calls.push((command, payload));
                std::future::ready(Ok(RouteReply::ok(Some(json!({ "pauseId": "test-pause" })))))
            })
            .await
            .unwrap()
        );
        assert_eq!(
            calls,
            vec![
                (
                    "acquire_session_input_pause",
                    json!({ "leaseKey": "legacy-update-restore", "clientId": UPDATE_RESTORE_CLIENT_ID })
                ),
                ("restore_actions", json!({ "snapshot": row.queue.actions })),
                (
                    "release_session_input_pause",
                    json!({ "pauseId": "test-pause", "clientId": UPDATE_RESTORE_CLIENT_ID })
                ),
            ]
        );
    }

    #[tokio::test]
    async fn legacy_restore_does_not_resume_idle_sessions_and_reports_failed_queue_restore() {
        let mut row = two_row_roster().sessions.remove(0);
        row.queue.actions = json!({ "formatVersion": 1, "actions": [{"id": "queued"}] });
        let mut calls = Vec::new();
        assert!(!restore_legacy_session(&row, None, "1.0.0", |command, _| {
            calls.push(command);
            std::future::ready(Ok(RouteReply::ok(Some(json!({ "pauseId": "test-pause" })))))
        })
        .await
        .unwrap());
        assert!(calls.is_empty());
        row.should_resume = true;
        let mut failed_calls = Vec::new();
        let error = restore_legacy_session(&row, None, "1.0.0", |command, _| {
            failed_calls.push(command);
            let response = if command == "restore_actions" {
                RouteReply::failed("invalid action")
            } else {
                RouteReply::ok(Some(json!({ "pauseId": "test-pause" })))
            };
            std::future::ready(Ok(response))
        })
        .await
        .unwrap_err();
        assert_eq!(
            failed_calls,
            [
                "acquire_session_input_pause",
                "restore_actions",
                "abort",
                "release_session_input_pause"
            ]
        );
        assert_eq!(
            error.to_string(),
            "legacy update restore_actions: invalid action"
        );
    }

    #[tokio::test]
    async fn legacy_restore_releases_input_pause_when_abort_fails() {
        let mut row = two_row_roster().sessions.remove(0);
        row.should_resume = true;
        row.queue.actions = json!({ "formatVersion": 1, "actions": [{"id": "queued"}] });
        let mut calls = Vec::new();
        let error = restore_legacy_session(&row, None, "1.0.0", |command, _| {
            calls.push(command);
            let response = match command {
                "restore_actions" => RouteReply::failed("invalid action"),
                "abort" => RouteReply::failed("abort refused"),
                _ => RouteReply::ok(Some(json!({ "pauseId": "test-pause" }))),
            };
            std::future::ready(Ok(response))
        })
        .await
        .unwrap_err();
        // A failed abort must not skip the release: a leaked pause would leave
        // the session unable to accept input.
        assert_eq!(
            calls,
            [
                "acquire_session_input_pause",
                "restore_actions",
                "abort",
                "release_session_input_pause"
            ]
        );
        assert_eq!(
            error.to_string(),
            "legacy update restore_actions: invalid action"
        );
    }

    #[test]
    fn legacy_child_metadata_replaces_only_the_old_parent_handle() {
        let mut payload = json!({ "runtimeMetadata": {
            "kind": "subagent", "parentActiveSessionId": "old-parent", "rlmChildId": "child"
        }});
        remap_legacy_parent(
            &mut payload,
            &BTreeMap::from([("old-parent".into(), "new-parent".into())]),
        );
        assert_eq!(
            payload,
            json!({ "runtimeMetadata": {
                "kind": "subagent", "parentActiveSessionId": "new-parent", "rlmChildId": "child"
            }})
        );
        remap_legacy_parent(&mut payload, &BTreeMap::new());
        assert_eq!(
            payload,
            json!({ "runtimeMetadata": { "kind": "subagent", "rlmChildId": "child" }})
        );
    }

    #[test]
    fn hello_resume_reports_progress_before_and_after_settle() {
        let progress = RestoreProgress::new();
        progress.begin(Some(&two_row_roster()));
        assert!(!progress.hello_resume().complete);
        assert_eq!(
            progress.hello_resume().update_id,
            Some(UpdateId::from("u-1".to_string()))
        );
        progress.settle(
            UpdateStatusCounts {
                total: 2,
                restored: 1,
                resumed: 1,
                failed: 1,
            },
            vec![UpdateStatusFailure {
                session_file: "/sessions/b.jsonl".to_string(),
                message: "worker create failed".to_string(),
            }],
        );
        let hello = progress.hello_resume();
        assert!(hello.complete);
        assert_eq!(hello.update_id, Some(UpdateId::from("u-1".to_string())));
        // Idempotent settle: a second settle never overwrites the first.
        progress.settle(UpdateStatusCounts::default(), Vec::new());
        assert!(progress.hello_resume().complete);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_attach_unblocks_when_only_its_target_settles() {
        let progress = std::sync::Arc::new(RestoreProgress::new());
        progress.begin(Some(&two_row_roster()));
        let waiter = |progress: &std::sync::Arc<RestoreProgress>, selector: &str| {
            let progress = std::sync::Arc::clone(progress);
            let selector = selector.to_string();
            tokio::spawn(async move { progress.wait_for_settle_target(&selector).await })
        };
        let mut queued_a = waiter(&progress, "a-1");
        let mut queued_b = waiter(&progress, "durable-b");
        // Neither row settled (the deadline, not this test's ticks, bounds the queue).
        let tick = std::time::Duration::from_millis(1);
        assert!(tokio::time::timeout(tick, &mut queued_a).await.is_err());
        assert!(tokio::time::timeout(tick, &mut queued_b).await.is_err());
        // One row settles by its stem selector `b-2`: only that row's waiter wakes.
        progress.settle_target("b-2", None);
        assert!(tokio::time::timeout(tick, queued_b).await.is_ok());
        assert!(tokio::time::timeout(tick, &mut queued_a).await.is_err());
        progress.settle(UpdateStatusCounts::default(), Vec::new());
        assert!(tokio::time::timeout(tick, queued_a).await.is_ok());
    }

    #[test]
    fn settle_target_failure_answers_the_typed_attach_error_per_row() {
        let progress = RestoreProgress::new();
        progress.begin(Some(&two_row_roster()));
        progress.settle_target("durable-b", Some("worker create failed".to_string()));
        // Every selector shape for the failed row resolves the failure.
        for selector in ["durable-b", "active-b", "b-2"] {
            let (file, message) = progress.settled_failure(selector).unwrap();
            assert_eq!(file, "/sessions/b-2.jsonl");
            assert_eq!(message, "worker create failed");
        }
        assert!(progress.settled_failure("a-1").is_none());
        assert!(progress.settled_failure("unknown-id").is_none());
    }

    #[test]
    fn registry_shaped_selectors_own_their_row_and_ambiguous_ones_own_nothing() {
        let progress = RestoreProgress::new();
        progress.begin(Some(&two_row_roster()));
        // Every selector shape `SessionRegistry::resolve` accepts owns the
        // row; the durable id is exact-key only.
        for selector in ["a-1", "active-a", "alpha", "ve-a", "IVEA", "b-2", "e-b"] {
            assert!(progress.owns_target(selector), "owns {selector}");
        }
        assert!(!progress.owns_target("unknown"));
        assert!(!progress.owns_target(""));

        // An ambiguous selector owns nothing, like the registry.
        let mut roster = two_row_roster();
        roster.sessions[1].active_session_id = "xx-active-a".to_string();
        let ambiguous = RestoreProgress::new();
        ambiguous.begin(Some(&roster));
        assert!(!ambiguous.owns_target("active-a"));
        ambiguous.settle_target("active-a", Some("never lands".to_string()));
        assert!(ambiguous.settled_failure("active-a").is_none());
        ambiguous.settle_target("alpha", None);
        assert!(ambiguous.settled_failure("alpha").is_none());
        assert!(ambiguous.hello_resume().update_id.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn an_adoption_settle_skips_a_row_pending_its_continuation() {
        let mut roster = two_row_roster();
        roster.sessions[0].should_resume = true;
        roster.sessions[0].in_flight.streaming = true;
        let progress = std::sync::Arc::new(RestoreProgress::new());
        progress.begin(Some(&roster));
        let waiter = |progress: &std::sync::Arc<RestoreProgress>, selector: &str| {
            let progress = std::sync::Arc::clone(progress);
            let selector = selector.to_string();
            tokio::spawn(async move { progress.wait_for_settle_target(&selector).await })
        };
        let tick = std::time::Duration::from_millis(1);
        let mut queued_a = waiter(&progress, "a-1");
        let queued_b = waiter(&progress, "durable-b");
        // The adoption settles skip the continuation row but settle the ordinary one.
        progress.settle_adopted("a-1");
        progress.settle_adopted("durable-b");
        assert!(tokio::time::timeout(tick, queued_b).await.is_ok());
        assert!(tokio::time::timeout(tick, &mut queued_a).await.is_err());
        // The pass's own settle is unconditional.
        progress.settle_target("a-1", None);
        assert!(tokio::time::timeout(tick, queued_a).await.is_ok());
    }

    #[test]
    fn legacy_adoption_waits_for_manifest_queue_replay_even_when_idle() {
        let mut roster = two_row_roster();
        roster.rest.insert("legacy_ts_restart".into(), json!(true));
        let progress = RestoreProgress::new();
        progress.begin(Some(&roster));
        progress.settle_adopted("a-1");
        assert!(!lock(&progress.state).targets["a-1"].settled);
        progress.settle_target("a-1", None);
        assert!(lock(&progress.state).targets["a-1"].settled);
    }

    #[tokio::test(start_paused = true)]
    async fn a_continuation_skip_is_still_progress_for_the_other_waiters() {
        let mut roster = two_row_roster();
        roster.sessions[0].should_resume = true;
        roster.sessions[0].in_flight.streaming = true;
        roster.sessions[1].should_resume = true;
        roster.sessions[1].in_flight.streaming = true;
        let progress = std::sync::Arc::new(RestoreProgress::new());
        progress.begin(Some(&roster));
        let waiter = |progress: &std::sync::Arc<RestoreProgress>, selector: &str| {
            let progress = std::sync::Arc::clone(progress);
            let selector = selector.to_string();
            tokio::spawn(async move { progress.wait_for_settle_target(&selector).await })
        };
        let mut queued_a = waiter(&progress, "a-1");
        let tick = std::time::Duration::from_millis(1);
        assert!(tokio::time::timeout(tick, &mut queued_a).await.is_err());
        // Quiet for 110s, then an adoption brings a continuation row up:
        // the skipped settle still counts as progress (re-arms the budget).
        tokio::time::advance(std::time::Duration::from_secs(110)).await;
        progress.settle_adopted("durable-b");
        // Past the original deadline: only the re-arm keeps the waiter queued.
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        assert!(
            tokio::time::timeout(tick, &mut queued_a).await.is_err(),
            "the skipped settle did not re-arm the other waiters"
        );
        progress.settle_target("a-1", None);
        assert!(tokio::time::timeout(tick, queued_a).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn settle_progress_rearms_a_waiters_quiet_budget_past_the_first_deadline() {
        let progress = std::sync::Arc::new(RestoreProgress::new());
        progress.begin(Some(&two_row_roster()));
        let waiter = |progress: &std::sync::Arc<RestoreProgress>, selector: &str| {
            let progress = std::sync::Arc::clone(progress);
            let selector = selector.to_string();
            tokio::spawn(async move { progress.wait_for_settle_target(&selector).await })
        };
        let mut queued_a = waiter(&progress, "a-1");
        let tick = std::time::Duration::from_millis(1);
        assert!(tokio::time::timeout(tick, &mut queued_a).await.is_err());
        // Quiet for 110s (budget 120s), then one unrelated settle:
        // the budget re-arms instead of expiring.
        tokio::time::advance(std::time::Duration::from_secs(110)).await;
        progress.settle_target("b-2", None);
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        assert!(
            tokio::time::timeout(tick, &mut queued_a).await.is_err(),
            "the waiter expired on the original deadline instead of the re-armed one"
        );
        tokio::time::advance(std::time::Duration::from_secs(40)).await;
        progress.settle_target("alpha", None);
        assert!(tokio::time::timeout(tick, queued_a).await.is_ok());
    }

    #[test]
    fn settle_target_without_a_registered_pass_is_a_no_op() {
        let progress = RestoreProgress::new();
        // No pass began: the settle must be a silent no-op.
        progress.settle_target("a-1", None);
        progress.settle_target("a-1", Some("never happens".to_string()));
        assert!(progress.settled_failure("a-1").is_none());
    }

    #[test]
    fn restore_rows_sort_bottom_up_deepest_first() {
        let row = |id: &str, depth: u32, subagent: bool| UpdateRosterSession {
            session_id: id.to_string(),
            active_session_id: format!("active-{id}"),
            session_file: format!("/sessions/{id}.jsonl"),
            name: None,
            kind: if subagent {
                pa_types::daemon::update_flow::UpdateRosterSessionKind::Subagent
            } else {
                pa_types::daemon::update_flow::UpdateRosterSessionKind::TopLevel
            },
            parent_session_id: None,
            rlm_depth: depth,
            cwd: "/w".to_string(),
            runtime_config: serde_json::json!({}),
            queue: pa_types::daemon::update_flow::UpdateRosterQueue {
                next_turn: Vec::new(),
                actions: serde_json::json!({}),
            },
            in_flight: pa_types::daemon::update_flow::UpdateRosterInFlight::default(),
            should_resume: false,
            rest: Map::default(),
        };
        let owned = [
            row("top", 0, false),
            row("child", 1, true),
            row("grandchild", 2, true),
        ];
        let mut rows: Vec<&UpdateRosterSession> = owned.iter().collect();
        sort_rows_bottom_up(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
        assert_eq!(ids, ["grandchild", "child", "top"]);
    }

    #[test]
    fn settled_failure_matches_durable_active_and_stem_selectors() {
        let progress = RestoreProgress::new();
        let roster: UpdateRoster = serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "update_id": "u-1",
            "socket_path": "/tmp/s.sock",
            "created_at": "2026-01-01T00:00:00Z",
            "supervisor": { "pid": 1, "process_start_id": "p", "generation": "g" },
            "binary": { "from_version": "0.1", "to_version": "0.2" },
            "sessions": [{
                "session_id": "durable-1",
                "active_session_id": "active-1",
                "session_file": "/sessions/durable-1.jsonl",
                "kind": "top-level",
                "rlm_depth": 0,
                "cwd": "/w",
                "runtime_config": {},
                "queue": { "next_turn": [], "actions": {} },
                "in_flight": {
                    "streaming": false, "compacting": false, "bash_running": false,
                    "rlm_children": false, "retrying": false, "prompt_in_flight": false
                },
                "should_resume": false
            }],
        }))
        .unwrap();
        progress.register_targets(&roster);
        progress.settle(
            UpdateStatusCounts {
                total: 1,
                failed: 1,
                ..Default::default()
            },
            vec![UpdateStatusFailure {
                session_file: "/sessions/durable-1.jsonl".to_string(),
                message: "worker create failed".to_string(),
            }],
        );
        for selector in ["durable-1", "active-1"] {
            let (file, message) = progress.settled_failure(selector).unwrap();
            assert_eq!(file, "/sessions/durable-1.jsonl");
            assert_eq!(message, "worker create failed");
        }
        assert!(progress.settled_failure("unknown-id").is_none());
    }

    #[test]
    fn boot_sweep_removes_socket_dir_and_legacy_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        let socket = dir.path().join("daemon.sock");
        let socket_hash = socket_hash(&socket);
        let scratch = socket_update_dir(&agent_dir, &socket_hash);
        std::fs::create_dir_all(scratch.join("prepared/u-1")).unwrap();
        std::fs::create_dir_all(legacy_update_restarts_dir(&agent_dir)).unwrap();
        std::fs::write(legacy_update_restart_status(&agent_dir), "{}").unwrap();
        let ts_record = pa_types::daemon::update_flow::update_restarts_dir(&agent_dir)
            .join("3c5dcf39f81bbe22-4e3a8d9e-1f2b-4c5d-9a0b-c1d2e3f4a5b6.json");
        std::fs::write(&ts_record, "{}").unwrap();
        let other_socket_dir =
            pa_types::daemon::update_flow::update_restarts_dir(&agent_dir).join("0f".repeat(32));
        std::fs::create_dir_all(other_socket_dir.join("prepared")).unwrap();
        boot_sweep(&agent_dir, &socket);
        assert!(!scratch.exists());
        assert!(!legacy_update_restarts_dir(&agent_dir).exists());
        assert!(!legacy_update_restart_status(&agent_dir).exists());
        assert_eq!(std::fs::read_to_string(&ts_record).unwrap(), "{}");
        assert!(other_socket_dir.exists());
    }

    #[test]
    fn boot_sweep_preserves_socket_bound_lost_reply_sources_until_attempts_finish() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        let socket = dir.path().join("daemon.sock");
        let other_socket = dir.path().join("other.sock");
        let sources = legacy_update_restarts_dir(&agent_dir);
        let recovery = agent_dir.join("legacy-update-recovery");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::create_dir_all(&recovery).unwrap();
        let mut attempts = Vec::new();
        let mut manifests = Vec::new();
        for pending_socket in [&socket, &other_socket] {
            let key = socket_hash(pending_socket);
            let attempt = recovery.join(format!("{key}.attempt.json"));
            let manifest = sources.join(format!("{key}.json"));
            std::fs::write(&attempt, serde_json::to_vec(&json!({
                "socket": pending_socket, "version": "older-target-version", "updateId": "pending",
                "startedAt": 123, "previousDigest": null, "origin": null,
                "hello": { "supervisorPid": 42, "supervisorProcessStartId": "start",
                    "supervisorGeneration": "generation" }
            })).unwrap()).unwrap();
            std::fs::write(&manifest, "prepared TS session and queue snapshot").unwrap();
            attempts.push(attempt);
            manifests.push(manifest);
        }
        let checkpoint = recovery.join("retained-checkpoint.json");
        std::fs::write(&checkpoint, "converted roster").unwrap();
        let stale = sources.join("unrelated.json");
        std::fs::write(&stale, "stale snapshot").unwrap();
        // A marker at the wrong key cannot preserve an unrelated source.
        std::fs::copy(&attempts[0], recovery.join("unrelated.attempt.json")).unwrap();
        let malformed = sources.join("malformed.json");
        std::fs::write(&malformed, "stale snapshot").unwrap();
        std::fs::write(recovery.join("malformed.attempt.json"), "not JSON").unwrap();
        boot_sweep(&agent_dir, &socket);
        assert_eq!(
            manifests
                .iter()
                .map(std::fs::read_to_string)
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap(),
            [
                "prepared TS session and queue snapshot",
                "prepared TS session and queue snapshot"
            ]
        );
        assert!(!stale.exists());
        assert!(!malformed.exists());
        assert!(attempts.iter().all(|path| path.exists()));
        assert_eq!(
            std::fs::read_to_string(&checkpoint).unwrap(),
            "converted roster"
        );

        // Successful coordinator adoption removes its attempt. A later boot can
        // sweep that source without destroying another socket's pending recovery.
        std::fs::remove_file(&attempts[0]).unwrap();
        boot_sweep(&agent_dir, &socket);
        assert!(!manifests[0].exists());
        assert_eq!(
            std::fs::read_to_string(&manifests[1]).unwrap(),
            "prepared TS session and queue snapshot"
        );
        std::fs::remove_file(&attempts[1]).unwrap();
        boot_sweep(&agent_dir, &socket);
        assert!(!sources.exists());
        assert_eq!(
            std::fs::read_to_string(checkpoint).unwrap(),
            "converted roster"
        );
    }

    #[test]
    fn action_snapshots_are_checked_in_order_with_the_daemons_own_refusal_last() {
        let snapshot = |actions: Value| json!({ "formatVersion": 1, "actions": actions });
        let refuse_custom = |id: &str, action: &Value| {
            if action["payload"].get("customMessage").is_some() {
                Err(format!("refused {id}"))
            } else {
                Ok(())
            }
        };
        let turn = |id: &str| json!({ "id": id, "payload": { "kind": "turn", "text": id } });
        assert_eq!(
            [
                validate_actions(None, refuse_custom).map(Vec::len),
                validate_actions(
                    Some(&json!({ "formatVersion": 2, "actions": [] })),
                    refuse_custom
                )
                .map(Vec::len),
                validate_actions(
                    Some(&snapshot(json!([turn("a"), turn("a")]))),
                    refuse_custom
                )
                .map(Vec::len),
                validate_actions(
                    Some(&snapshot(
                        json!([{ "id": "b", "payload": { "customMessage": {} } },
                                          { "id": "b" }])
                    )),
                    refuse_custom
                )
                .map(Vec::len),
                validate_actions(
                    Some(&snapshot(json!([turn("a"), turn("b")]))),
                    refuse_custom
                )
                .map(Vec::len),
            ],
            [
                Err("restore_actions requires a snapshot".to_string()),
                Err("Unsupported session action recovery format version: 2".to_string()),
                Err("Duplicate session action id: a".to_string()),
                Err("refused b".to_string()),
                Ok(2),
            ]
        );
    }

    #[test]
    fn consume_roster_env_none_without_the_env() {
        // NOTE: process env is global; this documents the None path only when no
        // other test set the var.
        if pa_types::session_scope::var(UPDATE_ROSTER_ENV).is_err() {
            assert!(consume_roster_env().is_none());
        }
    }
}
