//! The daemon's event bus: the sources sessions define, the subscriptions they
//! hold, and what a publication delivers (`event_bus` supervisor command; the
//! wire vocabulary is [`pa_types::events`]).
//!
//! A source is a named service with a JSON schema (an object schema) for its
//! payloads, owned by the session that defined it: only the owner publishes or
//! removes it. A subscription names a source, a filter (payload keys whose
//! values must be equal), and the prompt the subscriber's turn starts from.
//! Publishing validates the payload against the schema
//! ([`pa_agent::validation::validate_tool_arguments`]) and yields one delivery
//! per matching subscription of another session; the supervisor routes each as
//! an agent message (`supervisor.rs` in this module), which brings the
//! message path's lanes, idle wake, wake budget and queue cap with it.
//!
//! The bus is daemon-wide and survives daemon restarts: its state is one JSON
//! file in the supervisor's descriptor directory, rewritten atomically on
//! every change. A file that does not parse is set aside (`.corrupt`) and the
//! bus starts empty.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, bail, ensure, Context, Result};
use pa_types::events::{EventBusRequest, EventSourceRecord, EventSubscriptionRecord};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod link;
mod supervisor;

pub(crate) use link::LinkEventBusController;
#[cfg(test)]
mod tests;

/// The bus's file in the supervisor's descriptor directory.
pub(crate) const EVENT_BUS_FILE_NAME: &str = "event-bus.json";
/// Longest source name.
const MAX_SOURCE_NAME_CHARS: usize = 64;
/// Largest schema and largest payload, serialized.
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
/// A delivery is an agent message (at most 16,384 characters): an 8 KiB payload and a
/// 4,000-character prompt leave room for the header.
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
/// Longest subscription prompt and description.
const MAX_PROMPT_CHARS: usize = 4_000;
/// Most sources one session owns, and most subscriptions one session holds.
const MAX_SOURCES_PER_SESSION: usize = 32;
const MAX_SUBSCRIPTIONS_PER_SESSION: usize = 64;

/// The session a request comes from, as the supervisor resolved its worker token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requester {
    pub(crate) session_id: String,
    pub(crate) name: String,
    pub(crate) cwd: String,
}

/// One event to deliver: the subscription it matched and the message its
/// subscriber receives.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Delivery {
    pub(crate) subscription: EventSubscriptionRecord,
    pub(crate) message: String,
}

/// What an accepted publication delivers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Publication {
    pub(crate) event_id: String,
    pub(crate) deliveries: Vec<Delivery>,
}

/// What a request does: answered by the bus alone, or a publication the
/// supervisor still has to route.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Handled {
    Answer(Value),
    Publish(Publication),
}

/// The persisted state.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BusState {
    sources: BTreeMap<String, EventSourceRecord>,
    subscriptions: Vec<EventSubscriptionRecord>,
}

pub(crate) struct EventBus {
    path: PathBuf,
    state: Mutex<BusState>,
}

impl EventBus {
    /// The bus kept at `path`: its saved state, or an empty bus when there is
    /// none or it does not parse (that file is renamed aside first, and `log`
    /// says so).
    pub(crate) fn load(path: PathBuf, log: &crate::paths::RotatingLog) -> Self {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                let aside = path.with_extension("json.corrupt");
                let moved = match std::fs::rename(&path, &aside) {
                    Ok(()) => format!("set aside as {}", aside.display()),
                    Err(error) => format!("not set aside: {error}"),
                };
                log.append(&format!(
                    "event bus state {} does not parse ({error}); {moved}; starting empty",
                    path.display()
                ));
                BusState::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BusState::default(),
            Err(error) => {
                log.append(&format!(
                    "event bus state {} is unreadable ({error}); starting empty",
                    path.display()
                ));
                BusState::default()
            }
        };
        EventBus {
            path,
            state: Mutex::new(state),
        }
    }

    /// Apply one request from `requester`, at `now` (Unix milliseconds).
    ///
    /// # Errors
    ///
    /// The request is refused (its message says why) or the state could not
    /// be saved.
    pub(crate) fn handle(
        &self,
        requester: &Requester,
        request: EventBusRequest,
        now: u64,
    ) -> Result<Handled> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handled = match request {
            EventBusRequest::DefineSource {
                name,
                description,
                schema,
            } => {
                ensure!(
                    name.chars().count() <= MAX_SOURCE_NAME_CHARS
                        && name
                            .chars()
                            .next()
                            .is_some_and(|first| first.is_ascii_lowercase())
                        && name.chars().all(|letter| {
                            letter.is_ascii_lowercase()
                                || letter.is_ascii_digit()
                                || ".-_".contains(letter)
                        }),
                    "an event source name is 1 to {MAX_SOURCE_NAME_CHARS} lowercase letters, \
                     digits, `.`, `-` and `_`, starting with a letter: not {name:?}"
                );
                ensure!(
                    description.chars().count() <= MAX_PROMPT_CHARS,
                    "event source description is longer than {MAX_PROMPT_CHARS} characters"
                );
                ensure!(
                    schema.get("type").and_then(Value::as_str) == Some("object"),
                    "an event source schema is a JSON object schema with \"type\": \"object\""
                );
                let size = serde_json::to_string(&schema)?.len();
                ensure!(
                    size <= MAX_SCHEMA_BYTES,
                    "an event source schema is at most {MAX_SCHEMA_BYTES} bytes, not {size}"
                );
                let published_count = match state.sources.get(&name) {
                    Some(existing) if existing.owner_session_id != requester.session_id => bail!(
                        "event source {name:?} is defined by session {}; only it can redefine it",
                        existing.owner_name
                    ),
                    Some(existing) => existing.published_count,
                    None => {
                        let owned = state
                            .sources
                            .values()
                            .filter(|source| source.owner_session_id == requester.session_id)
                            .count();
                        ensure!(
                            owned < MAX_SOURCES_PER_SESSION,
                            "a session defines at most {MAX_SOURCES_PER_SESSION} event sources"
                        );
                        0
                    }
                };
                let record = EventSourceRecord {
                    name: name.clone(),
                    description,
                    schema,
                    owner_session_id: requester.session_id.clone(),
                    owner_name: requester.name.clone(),
                    defined_at: now,
                    published_count,
                };
                state.sources.insert(name, record.clone());
                self.save(&state)?;
                Handled::Answer(serde_json::to_value(record)?)
            }
            EventBusRequest::RemoveSource { name } => {
                owned_source(&state, &name, requester)?;
                state.sources.remove(&name);
                let before = state.subscriptions.len();
                state
                    .subscriptions
                    .retain(|subscription| subscription.source != name);
                let removed = before - state.subscriptions.len();
                self.save(&state)?;
                Handled::Answer(json!({ "removed": true, "subscriptionsRemoved": removed }))
            }
            EventBusRequest::Publish { source, payload } => {
                let record = owned_source(&state, &source, requester)?;
                let size = serde_json::to_string(&payload)?.len();
                ensure!(
                    size <= MAX_PAYLOAD_BYTES,
                    "event payload is {size} bytes; at most {MAX_PAYLOAD_BYTES}"
                );
                let payload = pa_agent::validation::validate_tool_arguments(
                    &format!("event {source}"),
                    &record.schema,
                    &payload,
                )
                .map_err(|error| anyhow!(error))?;
                let event_id = format!("evt-{}", uuid::Uuid::now_v7());
                let publisher = record.owner_name.clone();
                let rendered = serde_json::to_string(&payload)?;
                let deliveries: Vec<Delivery> = state
                    .subscriptions
                    .iter()
                    .filter(|subscription| {
                        subscription.source == source
                            && subscription.session_id != requester.session_id
                            && subscription
                                .filter
                                .iter()
                                .all(|(key, value)| payload.get(key) == Some(value))
                    })
                    .map(|subscription| {
                        let label = subscription
                            .label
                            .as_deref()
                            .map(|label| format!(" ({label})"))
                            .unwrap_or_default();
                        Delivery {
                            message: format!(
                                "Event `{source}` {event_id}, published by {publisher}, for \
                                 your subscription {}{label}.\n\n{}\n\nPayload:\n{rendered}",
                                subscription.id, subscription.prompt
                            ),
                            subscription: subscription.clone(),
                        }
                    })
                    .collect();
                if let Some(record) = state.sources.get_mut(&source) {
                    record.published_count += 1;
                }
                self.save(&state)?;
                Handled::Publish(Publication {
                    event_id,
                    deliveries,
                })
            }
            EventBusRequest::Subscribe {
                source,
                filter,
                prompt,
                delivery_mode,
                label,
            } => {
                let record = state
                    .sources
                    .get(&source)
                    .ok_or_else(|| anyhow!("no event source is named {source:?}"))?;
                // A filter names only properties the schema declares, when it declares any.
                if let Some(properties) = record.schema.get("properties").and_then(Value::as_object)
                {
                    let unknown: Vec<&str> = filter
                        .keys()
                        .filter(|key| !properties.contains_key(*key))
                        .map(String::as_str)
                        .collect();
                    ensure!(
                        unknown.is_empty(),
                        "the filter names properties the source's schema does not declare: {}",
                        unknown.join(", ")
                    );
                }
                let prompt = prompt.trim().to_string();
                ensure!(!prompt.is_empty(), "a subscription needs a prompt");
                ensure!(
                    prompt.chars().count() <= MAX_PROMPT_CHARS,
                    "a subscription prompt is at most {MAX_PROMPT_CHARS} characters"
                );
                let held = state
                    .subscriptions
                    .iter()
                    .filter(|subscription| subscription.session_id == requester.session_id)
                    .count();
                ensure!(
                    held < MAX_SUBSCRIPTIONS_PER_SESSION,
                    "a session holds at most {MAX_SUBSCRIPTIONS_PER_SESSION} subscriptions"
                );
                let record = EventSubscriptionRecord {
                    id: format!("sub-{}", uuid::Uuid::now_v7()),
                    source,
                    filter,
                    prompt,
                    delivery_mode,
                    label: label.filter(|label| !label.trim().is_empty()),
                    session_id: requester.session_id.clone(),
                    cwd: requester.cwd.clone(),
                    created_at: now,
                    delivered_count: 0,
                };
                state.subscriptions.push(record.clone());
                self.save(&state)?;
                Handled::Answer(serde_json::to_value(record)?)
            }
            EventBusRequest::Unsubscribe { subscription_id } => {
                let position = state
                    .subscriptions
                    .iter()
                    .position(|subscription| {
                        subscription.id == subscription_id
                            && subscription.session_id == requester.session_id
                    })
                    .ok_or_else(|| {
                        anyhow!("this session holds no subscription {subscription_id:?}")
                    })?;
                let removed = state.subscriptions.remove(position);
                self.save(&state)?;
                Handled::Answer(serde_json::to_value(removed)?)
            }
            EventBusRequest::List => Handled::Answer(json!({
                "sources": state.sources.values().collect::<Vec<_>>(),
                "subscriptions": state
                    .subscriptions
                    .iter()
                    .filter(|subscription| subscription.session_id == requester.session_id)
                    .collect::<Vec<_>>(),
            })),
        };
        Ok(handled)
    }

    /// Settle one routed delivery: count a delivered one, drop the
    /// subscription of a session that no longer exists.
    ///
    /// # Errors
    ///
    /// The state could not be saved.
    pub(crate) fn settle(&self, subscription_id: &str, settled: Settled) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match settled {
            Settled::Delivered => {
                if let Some(subscription) = state
                    .subscriptions
                    .iter_mut()
                    .find(|subscription| subscription.id == subscription_id)
                {
                    subscription.delivered_count += 1;
                }
            }
            Settled::SubscriberGone => {
                state
                    .subscriptions
                    .retain(|subscription| subscription.id != subscription_id);
            }
        }
        self.save(&state)
    }

    fn save(&self, state: &BusState) -> Result<()> {
        crate::descriptor::write_file_atomic(&self.path, &serde_json::to_string_pretty(state)?)
            .with_context(|| format!("save the event bus to {}", self.path.display()))
    }
}

/// How one routed delivery ended, for [`EventBus::settle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    Delivered,
    /// No live or saved session has the subscriber's id any more.
    SubscriberGone,
}

/// The source `name`, when `requester` owns it.
fn owned_source<'a>(
    state: &'a BusState,
    name: &str,
    requester: &Requester,
) -> Result<&'a EventSourceRecord> {
    let record = state
        .sources
        .get(name)
        .ok_or_else(|| anyhow!("no event source is named {name:?}"))?;
    ensure!(
        record.owner_session_id == requester.session_id,
        "event source {name:?} belongs to session {}; only it publishes or removes it",
        record.owner_name
    );
    Ok(record)
}
