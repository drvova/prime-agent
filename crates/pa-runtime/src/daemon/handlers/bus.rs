//! The bus and settings queries: `sources`, `subscriptions` and `settings`.
//!
//! - `sources`: the timer and watch sources the agent's database keeps (the
//!   scheduled jobs' timers among them), each with its kind, options, owner
//!   and whether it runs now, and the sources a run defined for itself that
//!   run now (`"kept": false`).
//! - `subscriptions`: the subscriptions on the bus, in id order, in the
//!   log's subscription shape.
//! - `settings`: the effective settings (the global ones under the
//!   project's) for session `args.session`'s working directory, or the
//!   daemon's without one, and what could not be read
//!   (`{"settings", "errors": [{"scope", "message"}]}`).

use anyhow::Context as _;
use serde_json::{json, Value};

use super::{optional_string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) async fn sources(&self) -> anyhow::Result<Value> {
        let log = std::sync::Arc::clone(self.runtime.log());
        let set =
            tokio::task::spawn_blocking(move || log.sources(crate::sources::SOURCES_PARTITION))
                .await
                .context("the sources' thread panicked")??;
        let mut running = self.runtime.running_sources();
        let mut sources = Vec::with_capacity(set.sources.len());
        for row in set.sources {
            let spec: Value = serde_json::from_str(&row.spec)
                .with_context(|| format!("source {:?} is not readable", row.id))?;
            sources.push(json!({
                "name": row.id,
                "kind": match row.kind {
                    pa_log::SourceKind::Timer => "timer",
                    pa_log::SourceKind::Watch => "watch",
                    pa_log::SourceKind::Webhook => "webhook",
                    pa_log::SourceKind::Bus => "bus",
                },
                "options": spec.get("options"),
                "owner": spec.get("owner"),
                "running": running.remove(&row.id),
                "kept": true,
            }));
        }
        // What runs now and is not kept: defined for a run.
        sources.extend(
            running
                .into_iter()
                .map(|name| json!({ "name": name, "running": true, "kept": false })),
        );
        Ok(json!({ "sources": sources }))
    }

    pub(super) fn subscriptions(&self) -> Value {
        json!({ "subscriptions": self.runtime.subscriptions() })
    }

    pub(super) async fn settings(&self, args: &Value) -> anyhow::Result<Value> {
        let cwd = match optional_string_arg(args, "session")? {
            Some(id) => self
                .open
                .cwd(&id)
                .with_context(|| format!("session {id} is not open in the runtime daemon"))?,
            None => self.cwd.clone(),
        };
        let agent_dir = self.agent_dir.clone();
        tokio::task::spawn_blocking(move || {
            let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
            let errors: Vec<Value> = settings
                .errors()
                .iter()
                .map(|error| {
                    let scope = match error.scope {
                        pa_core::settings::SettingsScope::Global => "global",
                        pa_core::settings::SettingsScope::Project => "project",
                    };
                    json!({ "scope": scope, "message": error.message })
                })
                .collect();
            Ok(json!({
                "cwd": cwd,
                "settings": serde_json::to_value(settings.settings())?,
                "errors": errors,
            }))
        })
        .await
        .context("the settings' thread panicked")?
    }
}
