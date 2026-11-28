//! The agents view's lanes into a session's kernel, as both daemons answer them: the kernel's
//! own shell commands (`list_kernel_bash`, `tail_kernel_bash`, `kill_kernel_bash`, over
//! [`SessionEngine::bash_activity`]) and the `/factory` view's lane (`factory_activity`, over
//! [`SessionEngine::factory_activity`]). The argument checks and their refusal texts, the reply
//! shapes, and the unscoped graph's stored specs live here, moved from the old worker's arms
//! (pa-daemon `user_bash.rs` and `factory_activity.rs`); the old worker and the runtime daemon
//! (`pa-runtime` `daemon/handlers/kernel_activity.rs`) call them.
//!
//! [`SessionEngine::bash_activity`]: super::engine::SessionEngine::bash_activity
//! [`SessionEngine::factory_activity`]: super::engine::SessionEngine::factory_activity

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

/// How many lines a `tail_kernel_bash` without `lines` reads.
const DEFAULT_TAIL_LINES: usize = 50;

/// What one kernel shell activity command asks the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelBashAction {
    /// `list_kernel_bash`: `{activities}`.
    List,
    /// `tail_kernel_bash`: `{id, tail}`.
    Tail,
    /// `kill_kernel_bash`: `{id, killed}`.
    Kill,
}

impl KernelBashAction {
    /// The kernel's name for the action ([`SessionEngine::bash_activity`]'s `action`).
    ///
    /// [`SessionEngine::bash_activity`]: super::engine::SessionEngine::bash_activity
    #[must_use]
    pub fn kernel_action(self) -> &'static str {
        match self {
            KernelBashAction::List => "list",
            KernelBashAction::Tail => "tail",
            KernelBashAction::Kill => "kill",
        }
    }
}

/// One kernel shell activity command, checked as the old worker checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelBashRequest {
    pub action: KernelBashAction,
    /// The command a `tail` or `kill` names (`activityId`).
    pub activity_id: Option<String>,
    /// How many lines a `tail` reads (1 to 200; 50 when the command names none).
    pub lines: usize,
}

impl KernelBashRequest {
    /// The command's `action` with its payload's `activityId` and `lines`.
    ///
    /// # Errors
    ///
    /// The old refusal text: a `tail` or `kill` without an `activityId`, or a `tail` whose
    /// `lines` is not a count from 1 to 200.
    pub fn parse(action: KernelBashAction, payload: &Value) -> Result<Self, &'static str> {
        let activity_id = payload.get("activityId").and_then(Value::as_str);
        if action != KernelBashAction::List && activity_id.is_none_or(str::is_empty) {
            return Err("activityId is required");
        }
        let lines = match (action, payload.get("lines")) {
            (KernelBashAction::Tail, Some(value)) => value
                .as_u64()
                .filter(|lines| (1..=200).contains(lines))
                .and_then(|lines| usize::try_from(lines).ok())
                .ok_or("lines must be between 1 and 200")?,
            (KernelBashAction::List | KernelBashAction::Tail | KernelBashAction::Kill, _) => {
                DEFAULT_TAIL_LINES
            }
        };
        Ok(KernelBashRequest {
            action,
            activity_id: activity_id.map(str::to_string),
            lines,
        })
    }

    /// The command's reply from the kernel's `fields`: without the kernel frame's own `event`,
    /// `status`, `reason` and request `id`, and with the command's `activityId` as `id`.
    #[must_use]
    pub fn reply(&self, mut fields: Value) -> Value {
        if let Some(object) = fields.as_object_mut() {
            for key in ["event", "status", "reason", "id", "activityId"] {
                object.remove(key);
            }
            if let Some(activity_id) = &self.activity_id {
                object.insert("id".to_string(), json!(activity_id));
            }
        }
        fields
    }
}

/// One `factory_activity` command: the action and its targets, as the kernel frame takes them
/// ([`SessionEngine::factory_activity`] validates them).
///
/// [`SessionEngine::factory_activity`]: super::engine::SessionEngine::factory_activity
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryLaneRequest {
    pub action: String,
    pub run_id: Option<String>,
    pub spec_id: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl FactoryLaneRequest {
    /// The command's `action`, `runId`, `specId` and `timeoutMs`.
    ///
    /// # Errors
    ///
    /// The old refusal text when the command names no action.
    pub fn parse(payload: &Value) -> Result<Self, &'static str> {
        let string = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        let action = string("action").unwrap_or_default();
        if action.is_empty() {
            return Err("action is required");
        }
        Ok(FactoryLaneRequest {
            action,
            run_id: string("runId"),
            spec_id: string("specId"),
            timeout_ms: payload.get("timeoutMs").and_then(Value::as_u64),
        })
    }

    /// Whether this is the unscoped graph (the dock's and the agents view's poll), whose reply
    /// also carries the session's stored specs as `factorySpecs`.
    #[must_use]
    pub fn lists_runs(&self) -> bool {
        self.action == "graph" && self.run_id.is_none() && self.spec_id.is_none()
    }

    /// The kernel's `reply` as the lane answers it: the unscoped graph of a session whose kernel
    /// is not running (not booted yet, or passivated) is `{runs: []}`, since its stored specs are
    /// durable harness entries; every other reply is the kernel's.
    ///
    /// # Errors
    ///
    /// The kernel's error, for every other request.
    pub fn settle(&self, reply: anyhow::Result<Value>) -> anyhow::Result<Value> {
        match reply {
            Err(error)
                if self.lists_runs()
                    && error.to_string() == pa_types::daemon::KERNEL_NOT_RUNNING_MESSAGE =>
            {
                Ok(json!({ "runs": [] }))
            }
            reply => reply,
        }
    }
}

/// Whether the factory lane is on: the factory is opt-in (`factory.enabled` in the shared
/// settings file, default off), so a disabled factory surfaces no lane and its sessions' rows
/// count no stored specs. The read is fresh on every call.
#[must_use]
pub fn factory_lane_enabled(agent_dir: &Path) -> bool {
    crate::settings::SettingsManager::create(
        pa_types::session_scope::current_dir().unwrap_or_default(),
        agent_dir,
    )
    .get_factory_enabled()
}

/// A session's own stored factory specs: the factory entries of its local harness store (under
/// its `session_artifact_dir`), read from disk without a kernel. The global store and the
/// runtime's shared library machines are every session's, so they never count. Blocks on file
/// reads: call it off an async runtime.
#[must_use]
pub fn stored_factory_spec_count(session_artifact_dir: &Path) -> usize {
    crate::refinement::get_local_harness_state_dir(Some(session_artifact_dir)).map_or(0, |dir| {
        crate::refinement::load_harness_state(&dir, crate::refinement::HarnessScope::Local).entries
            [&crate::refinement::RefinementKind::Factory]
            .len()
    })
}

/// Reads one saved session's stored factory specs ([`stored_factory_spec_count`], its artifact
/// dir beside its sessions dir) into a listing's map: a listing reads each store once, and only
/// sessions holding specs get an entry. Blocks on file reads: call it off an async runtime.
pub fn count_factory_specs<S: std::hash::BuildHasher>(
    info: &crate::session::catalog::SessionInfo,
    factory_specs: &mut HashMap<String, usize, S>,
) {
    let specs = info.path.parent().and_then(Path::parent).map_or(0, |root| {
        stored_factory_spec_count(&root.join("session-artifacts").join(&info.id))
    });
    if specs > 0 {
        factory_specs.insert(info.id.clone(), specs);
    }
}

#[cfg(test)]
mod tests;
