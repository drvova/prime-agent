//! The daemon's event bus (`event-bus.json`): its sources are bus rows of
//! `sources`, its subscriptions rows of `subscriptions`.

use std::collections::BTreeMap;
use std::path::Path;

use pa_log::{Log, Origin, SourceKind, SourceRow, SourceSet, SubscriptionRow};
use pa_types::events::{EventSourceRecord, EventSubscriptionRecord};
use serde::{Deserialize, Serialize};

use super::{process_log, StateFile};

/// What the bus keeps: its file's shape.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventBusState {
    /// By name.
    pub sources: BTreeMap<String, EventSourceRecord>,
    pub subscriptions: Vec<EventSubscriptionRecord>,
}

impl EventBusState {
    /// Its rows.
    pub(super) fn rows(&self) -> anyhow::Result<SourceSet> {
        Ok(SourceSet {
            sources: self
                .sources
                .iter()
                .map(|(name, record)| {
                    Ok(SourceRow {
                        id: name.clone(),
                        kind: SourceKind::Bus,
                        spec: serde_json::to_string(record)?,
                        claim: None,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
            subscriptions: self
                .subscriptions
                .iter()
                .map(|subscription| {
                    Ok(SubscriptionRow {
                        id: subscription.id.clone(),
                        source: subscription.source.clone(),
                        spec: serde_json::to_string(subscription)?,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
        })
    }
}

/// The bus kept for `path` in `log`, when the database holds it.
pub(super) fn held_in(log: &Log, path: &Path) -> anyhow::Result<Option<EventBusState>> {
    let scope = StateFile::EventBus.scope(path);
    if log.partition(&scope)?.is_none() {
        return Ok(None);
    }
    let set = log.sources(&scope)?;
    Ok(Some(EventBusState {
        sources: set
            .sources
            .into_iter()
            .map(|row| Ok((row.id, serde_json::from_str(&row.spec)?)))
            .collect::<anyhow::Result<_>>()?,
        subscriptions: set
            .subscriptions
            .into_iter()
            .map(|row| Ok(serde_json::from_str(&row.spec)?))
            .collect::<anyhow::Result<_>>()?,
    }))
}

/// The bus whose file is `path`, when the process's database holds it;
/// `None` when there is no database or it does not yet (the caller reads
/// the file, as before the move).
///
/// # Errors
///
/// The database could not be read, or holds rows that do not parse.
pub fn held_event_bus(path: &Path) -> anyhow::Result<Option<EventBusState>> {
    match process_log() {
        Some(log) => held_in(log, path),
        None => Ok(None),
    }
}

/// Store the bus whose file is `path` in the process's database; whether it
/// was (false: there is no database, and the caller keeps its file).
///
/// # Errors
///
/// The write did not commit.
pub fn store_event_bus(path: &Path, state: &EventBusState) -> anyhow::Result<bool> {
    let Some(log) = process_log() else {
        return Ok(false);
    };
    log.replace_sources(
        &StateFile::EventBus.scope(path),
        state.rows()?,
        Origin::Written,
    )
    .wait_blocking()
}
