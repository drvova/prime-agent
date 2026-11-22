//! Where a session's factory reads from: the agent directory (the opt-in
//! setting and the personal machine library), the harness stores holding
//! factory and subagent entries, and the shared machine library.

use std::path::PathBuf;

use anyhow::anyhow;

use crate::refinement::{
    factory_enabled, get_global_harness_state_dir, load_harness_state, HarnessEntry, HarnessScope,
    RefinementKind, FACTORY_DISABLED_MESSAGE,
};

/// The directories one session's factory resolves against.
#[derive(Debug, Clone, Default)]
pub struct FactoryEnvironment {
    /// The session's agent directory. Without one the factory reads as
    /// disabled: the opt-in fails closed.
    pub agent_dir: Option<PathBuf>,
    /// The session's local harness state directory, where unprefixed
    /// factory and subagent ids resolve. `None` for a session without
    /// artifacts: it has no stored entries, only library machines.
    pub local_harness_dir: Option<PathBuf>,
    /// A directory replacing the machines shipped with the product as the
    /// library's first level (`PRIME_AGENT_MACHINES_DIR`).
    pub machines_dir: Option<PathBuf>,
}

impl FactoryEnvironment {
    /// The environment of a session with this agent directory and local
    /// harness state directory; the shared machine level follows
    /// `PRIME_AGENT_MACHINES_DIR` when it is set and not empty.
    #[must_use]
    pub fn for_session(agent_dir: PathBuf, local_harness_dir: Option<PathBuf>) -> Self {
        FactoryEnvironment {
            agent_dir: Some(agent_dir),
            local_harness_dir,
            machines_dir: pa_types::session_scope::var("PRIME_AGENT_MACHINES_DIR")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Refuse while the `factory.enabled` setting is off (the default; the
    /// user turns it on with `/factory on`).
    ///
    /// # Errors
    ///
    /// The one refusal every gated factory call raises.
    pub(super) fn require_enabled(&self) -> anyhow::Result<()> {
        if self.agent_dir.as_deref().is_some_and(factory_enabled) {
            Ok(())
        } else {
            Err(anyhow!(FACTORY_DISABLED_MESSAGE))
        }
    }

    /// The personal machines directory (`<agent dir>/machines`).
    pub(super) fn user_machines_dir(&self) -> Option<PathBuf> {
        self.agent_dir.as_ref().map(|dir| dir.join("machines"))
    }

    /// Every entry of one kind in the store an id routes to: `global:x`
    /// reads the global store, anything else (`local:x` included) the
    /// session's local one. Returns the entries and the id without its
    /// scope prefix.
    fn entries<'a>(&self, kind: RefinementKind, id: &'a str) -> (Vec<HarnessEntry>, &'a str) {
        let (global, id) = match id.split_once(':') {
            Some(("global", rest)) if !rest.is_empty() => (true, rest),
            Some(("local", rest)) if !rest.is_empty() => (false, rest),
            Some(_) | None => (false, id),
        };
        let state = if global {
            self.agent_dir.as_ref().map(|agent_dir| {
                load_harness_state(
                    &get_global_harness_state_dir(agent_dir),
                    HarnessScope::Global,
                )
            })
        } else {
            self.local_harness_dir
                .as_ref()
                .map(|dir| load_harness_state(dir, HarnessScope::Local))
        };
        let entries = state
            .and_then(|mut state| state.entries.remove(&kind))
            .map(|entries| entries.into_values().collect())
            .unwrap_or_default();
        (entries, id)
    }

    /// The stored factory entry with this id.
    pub(super) fn factory_entry(&self, id: &str) -> Option<HarnessEntry> {
        let (entries, id) = self.entries(RefinementKind::Factory, id);
        entries.into_iter().find(|entry| entry.id == id)
    }

    /// The harness subagent entry a state references: by id, else by title
    /// (the first in path, then id, order among entries sharing a title).
    pub(super) fn subagent_entry(&self, reference: &str) -> Option<HarnessEntry> {
        let (mut entries, id) = self.entries(RefinementKind::Subagent, reference);
        if let Some(position) = entries.iter().position(|entry| entry.id == id) {
            return Some(entries.swap_remove(position));
        }
        let (mut local, _) = self.entries(RefinementKind::Subagent, "");
        local.sort_by(|left, right| (&left.path, &left.id).cmp(&(&right.path, &right.id)));
        local.into_iter().find(|entry| entry.title == reference)
    }
}

/// The spec a stored factory entry carries: its `machine`, else its `dag`
/// (a JSON null reads as absent).
pub(super) fn entry_spec(entry: &HarnessEntry) -> serde_json::Value {
    ["machine", "dag"]
        .into_iter()
        .filter_map(|key| entry.arguments.get(key))
        .find(|spec| !spec.is_null())
        .cloned()
        .unwrap_or_default()
}
