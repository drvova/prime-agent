//! An event bus's state (`event-bus.json`): its sources are bus rows of
//! `sources`, its subscriptions rows of `subscriptions`; and the bus's rules
//! ([`EventBusState::apply`]), which the daemon's bus and a headless runtime's
//! both apply, so a request is refused for the same reason in either.
//!
//! A source is a named service with a JSON object schema for its payloads,
//! owned by the session that defined it: only the owner publishes, redefines
//! or removes it. A subscription names a source (or, where the bus takes
//! them, a topic filter), a filter (payload keys whose values must be
//! equal), and the prompt its session's turn starts from. A publication's
//! payload is checked against the source's schema
//! ([`pa_agent::validation::validate_tool_arguments`]); who it reaches is the
//! bus's to decide.
//!
//! Where the bus runs them ([`SourceKinds::Accepted`]: a headless run's and
//! the runtime daemon's), a source may instead be a timer, a folder watch or
//! a webhook, which publishes its own events under its kind's payload
//! schema. Names, limits and ownership are one namespace across kinds. A
//! webhook's record says where a sender posts and how it authenticates,
//! never with what: its runtime keeps the secret, and fills in the port and
//! the secret before the request is applied. Timer, watch and webhook
//! sources live only for the run unless the bus keeps them
//! ([`StoredSources::Every`]): otherwise they, and the subscriptions naming
//! them, are left out of the rows the bus stores.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, ensure};
use pa_log::{Log, Origin, Receipt, SourceKind, SourceRow, SourceSet, SubscriptionRow};
use pa_types::event_log::TopicFilter;
use pa_types::events::{
    EventBusRequest, EventSourceKind, EventSourceRecord, EventSubscriptionRecord, WebhookRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{process_log, StateFile};

/// Longest source name.
const MAX_SOURCE_NAME_CHARS: usize = 64;
/// Largest schema, serialized.
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
/// Largest payload, serialized. A delivery is an agent message (at most 16,384 characters): an
/// 8 KiB payload and a 4,000-character prompt leave room for the header.
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
/// Longest subscription prompt and source description.
const MAX_PROMPT_CHARS: usize = 4_000;
/// Most sources one session owns, and most subscriptions one session holds.
const MAX_SOURCES_PER_SESSION: usize = 32;
const MAX_SUBSCRIPTIONS_PER_SESSION: usize = 64;
/// A webhook's body limit when its definer gives none, and the most it may
/// give: a delivery carries the body to the model.
const DEFAULT_WEBHOOK_BODY_BYTES: u64 = 64 * 1024;
const MAX_WEBHOOK_BODY_BYTES: u64 = 1024 * 1024;
/// Longest webhook path.
const MAX_WEBHOOK_PATH_CHARS: usize = 256;

/// The session a bus request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRequester {
    /// Its durable id: what ownership is checked against.
    pub session_id: String,
    /// Its name when it has one, else its id.
    pub name: String,
    pub cwd: String,
}

/// Whether a subscription may name a topic filter (any text holding `/`,
/// which no source name does) in place of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicSubscriptions {
    /// Only sources: a `/` names no source and is refused as one.
    Refused,
    /// A topic filter subscribes to every event on the topics it matches.
    Accepted,
}

/// Whether a bus defines timer, watch and webhook sources besides its bus
/// sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKinds {
    /// Only bus sources (the daemon's bus): a timer, a watch or a webhook is
    /// refused.
    BusOnly,
    /// Timers, watches and webhooks too, which its runtime runs (and refuses
    /// what it cannot: a headless run, a webhook).
    Accepted,
}

/// Which of a bus's sources, and the subscriptions to them, its stored state
/// keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredSources {
    /// Its bus sources only: a timer or a watch lasts as long as the run
    /// that runs it.
    BusOnly,
    /// Its timers, watches and webhooks too (the runtime daemon's, whose
    /// runtime runs them again after a restart).
    Every,
}

/// What an accepted request asks of the bus that keeps the state.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBusChange {
    /// The answer, from the state alone.
    Answer(Value),
    /// A publication the bus still delivers: its source (whose count is
    /// already raised) and the payload as its schema checked it.
    Publish {
        /// Boxed: a record is many times an answer's size.
        source: Box<EventSourceRecord>,
        payload: Value,
    },
}

/// What the bus keeps: its file's shape.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventBusState {
    /// By name.
    pub sources: BTreeMap<String, EventSourceRecord>,
    pub subscriptions: Vec<EventSubscriptionRecord>,
}

impl EventBusState {
    /// Apply one request from `requester` at `now` (Unix milliseconds); the
    /// caller stores the state after any request but a listing.
    ///
    /// # Errors
    ///
    /// The request is refused; the message says why, for the model to read.
    pub fn apply(
        &mut self,
        requester: &EventRequester,
        request: EventBusRequest,
        now: u64,
        topics: TopicSubscriptions,
        kinds: SourceKinds,
    ) -> anyhow::Result<EventBusChange> {
        let answer = match request {
            EventBusRequest::DefineSource {
                name,
                description,
                schema,
                timer,
                watch,
                webhook,
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
                    [timer.is_some(), watch.is_some(), webhook.is_some()]
                        .into_iter()
                        .filter(|given| *given)
                        .count()
                        <= 1,
                    "an event source is a timer, a watch or a webhook, not several: give one of \
                     #:timer, #:watch and #:webhook"
                );
                let kind = match (&timer, &watch, &webhook) {
                    (Some(_), _, _) => EventSourceKind::Timer,
                    (None, Some(_), _) => EventSourceKind::Watch,
                    (None, None, Some(_)) => EventSourceKind::Webhook,
                    (None, None, None) => EventSourceKind::Bus,
                };
                let schema = match kind {
                    EventSourceKind::Bus => {
                        ensure!(
                            schema.get("type").and_then(Value::as_str) == Some("object"),
                            "an event source schema is a JSON object schema with \"type\": \"object\""
                        );
                        let size = serde_json::to_string(&schema)?.len();
                        ensure!(
                            size <= MAX_SCHEMA_BYTES,
                            "an event source schema is at most {MAX_SCHEMA_BYTES} bytes, not {size}"
                        );
                        schema
                    }
                    EventSourceKind::Timer | EventSourceKind::Watch | EventSourceKind::Webhook => {
                        let kind_name = kind.as_str();
                        ensure!(
                            kinds == SourceKinds::Accepted,
                            "this event bus (the daemon's) defines bus sources only: {kind_name} \
                             sources run in headless runs (`prime-agent -p`) for now"
                        );
                        ensure!(
                            matches!(schema, Value::Null | Value::Bool(false)),
                            "a {kind_name} source's payload schema is its kind's: give #f for the \
                             schema"
                        );
                        match kind {
                            EventSourceKind::Webhook => json!({
                                "type": "object",
                                "properties": {
                                    "body": {},
                                    "headers": { "type": "object" },
                                },
                                "required": ["body"],
                            }),
                            EventSourceKind::Timer => json!({
                                "type": "object",
                                "properties": {
                                    "scheduledFor": { "type": "integer" },
                                    "firedAt": { "type": "integer" },
                                },
                                "required": ["scheduledFor", "firedAt"],
                            }),
                            EventSourceKind::Watch | EventSourceKind::Bus => json!({
                                "type": "object",
                                "properties": {
                                    "paths": { "type": "array", "items": { "type": "string" } },
                                    "kinds": { "type": "array", "items": { "type": "string" } },
                                    "error": { "type": "string" },
                                },
                            }),
                        }
                    }
                };
                let webhook = webhook
                    .map(|spec| {
                        let path = spec.path.unwrap_or_else(|| "/".to_string());
                        ensure!(
                            path.starts_with('/')
                                && path.chars().count() <= MAX_WEBHOOK_PATH_CHARS
                                && !path.contains(['?', '#'])
                                && !path.chars().any(char::is_whitespace),
                            "a webhook's 'path starts with `/`, is at most \
                             {MAX_WEBHOOK_PATH_CHARS} characters, and has no spaces, `?` or `#`: \
                             not {path:?}"
                        );
                        let port = spec.port.filter(|port| *port != 0).ok_or_else(|| {
                            anyhow!("a webhook's port is picked by the runtime that runs it")
                        })?;
                        let auth: Vec<String> = [
                            spec.token.as_ref().map(|_| "token"),
                            spec.hmac_secret.as_ref().map(|_| "hmac-secret"),
                        ]
                        .into_iter()
                        .flatten()
                        .map(str::to_string)
                        .collect();
                        ensure!(
                            !auth.is_empty(),
                            "a webhook needs a 'token, an 'hmac-secret, or both"
                        );
                        let max_body_bytes =
                            spec.max_body_bytes.unwrap_or(DEFAULT_WEBHOOK_BODY_BYTES);
                        ensure!(
                            max_body_bytes <= MAX_WEBHOOK_BODY_BYTES,
                            "a webhook's 'max-body-bytes is at most {MAX_WEBHOOK_BODY_BYTES}"
                        );
                        Ok(WebhookRecord {
                            url: format!("http://127.0.0.1:{port}{path}"),
                            path,
                            port,
                            auth,
                            max_body_bytes,
                        })
                    })
                    .transpose()?;
                let published_count = match self.sources.get(&name) {
                    Some(existing) if existing.owner_session_id != requester.session_id => bail!(
                        "event source {name:?} is defined by session {}; only it can redefine it",
                        existing.owner_name
                    ),
                    // A webhook's secret is not the bus's to compare: one is
                    // only replaced by removing it first.
                    Some(existing)
                        if existing.kind() == EventSourceKind::Webhook
                            || kind == EventSourceKind::Webhook =>
                    {
                        bail!(
                            "event source {name:?} is defined already as a {} source; remove it \
                             (event-remove) before defining it again",
                            existing.kind().as_str()
                        )
                    }
                    // A bus source's schema may change; what a timer or a
                    // watch runs is only replaced by removing it first.
                    Some(existing)
                        if (existing.kind() != EventSourceKind::Bus
                            || kind != EventSourceKind::Bus)
                            && (existing.timer != timer || existing.watch != watch) =>
                    {
                        bail!(
                            "event source {name:?} is defined already as a {} source; remove it \
                             (event-remove) before defining it again another way",
                            existing.kind().as_str()
                        )
                    }
                    Some(existing) => existing.published_count,
                    None => {
                        let owned = self
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
                    timer,
                    watch,
                    webhook,
                };
                self.sources.insert(name, record.clone());
                serde_json::to_value(record)?
            }
            EventBusRequest::RemoveSource { name } => {
                self.owned_source(&name, requester)?;
                self.sources.remove(&name);
                let before = self.subscriptions.len();
                self.subscriptions
                    .retain(|subscription| subscription.source != name);
                let removed = before - self.subscriptions.len();
                json!({ "removed": true, "subscriptionsRemoved": removed })
            }
            EventBusRequest::Publish { source, payload } => {
                let record = self.owned_source(&source, requester)?;
                ensure!(
                    record.kind() == EventSourceKind::Bus,
                    "event source {source:?} is a {} source: it publishes its own events",
                    record.kind().as_str()
                );
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
                let record = self
                    .sources
                    .get_mut(&source)
                    .ok_or_else(|| anyhow!("no event source is named {source:?}"))?;
                record.published_count += 1;
                return Ok(EventBusChange::Publish {
                    source: Box::new(record.clone()),
                    payload,
                });
            }
            EventBusRequest::Subscribe {
                source,
                filter,
                prompt,
                delivery_mode,
                label,
            } => {
                match topics {
                    TopicSubscriptions::Accepted if source.contains('/') => {
                        TopicFilter::new(source.as_str()).map_err(|error| {
                            anyhow!("{source:?} is not a topic filter: {error}")
                        })?;
                    }
                    TopicSubscriptions::Accepted | TopicSubscriptions::Refused => {
                        let record = self
                            .sources
                            .get(&source)
                            .ok_or_else(|| anyhow!("no event source is named {source:?}"))?;
                        // A filter names only properties the schema declares, when it declares any.
                        if let Some(properties) =
                            record.schema.get("properties").and_then(Value::as_object)
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
                    }
                }
                let prompt = prompt.trim().to_string();
                ensure!(!prompt.is_empty(), "a subscription needs a prompt");
                ensure!(
                    prompt.chars().count() <= MAX_PROMPT_CHARS,
                    "a subscription prompt is at most {MAX_PROMPT_CHARS} characters"
                );
                let held = self
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
                self.subscriptions.push(record.clone());
                serde_json::to_value(record)?
            }
            EventBusRequest::Unsubscribe { subscription_id } => {
                let position = self
                    .subscriptions
                    .iter()
                    .position(|subscription| {
                        subscription.id == subscription_id
                            && subscription.session_id == requester.session_id
                    })
                    .ok_or_else(|| {
                        anyhow!("this session holds no subscription {subscription_id:?}")
                    })?;
                serde_json::to_value(self.subscriptions.remove(position))?
            }
            EventBusRequest::List => json!({
                "sources": self.sources.values().collect::<Vec<_>>(),
                "subscriptions": self
                    .subscriptions
                    .iter()
                    .filter(|subscription| subscription.session_id == requester.session_id)
                    .collect::<Vec<_>>(),
            }),
        };
        Ok(EventBusChange::Answer(answer))
    }

    /// The source `name`, when `requester` owns it.
    fn owned_source(
        &self,
        name: &str,
        requester: &EventRequester,
    ) -> anyhow::Result<&EventSourceRecord> {
        let record = self
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

    /// Its rows: its bus sources and the subscriptions not naming a timer,
    /// a watch or a webhook, which live only for the run, unless `stored`
    /// keeps every source.
    pub(super) fn rows(&self, stored: StoredSources) -> anyhow::Result<SourceSet> {
        let kept = |name: &str| match stored {
            StoredSources::Every => true,
            StoredSources::BusOnly => self
                .sources
                .get(name)
                .is_none_or(|record| record.kind() == EventSourceKind::Bus),
        };
        Ok(SourceSet {
            sources: self
                .sources
                .iter()
                .filter(|(name, _)| kept(name))
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
                .filter(|subscription| kept(&subscription.source))
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

/// The message a delivered event carries to `subscription`'s session: the
/// event `source` names (`event_id`, published by `publisher`), the
/// subscription's prompt, and the payload as JSON. The receiving session
/// reads it as an agent message from `event:<source>` (whose header shows
/// `event <source>`).
#[must_use]
pub fn event_delivery_message(
    source: &str,
    event_id: &str,
    publisher: &str,
    subscription: &EventSubscriptionRecord,
    payload: &Value,
) -> String {
    let label = subscription
        .label
        .as_deref()
        .map_or_default(|label| format!(" ({label})"));
    format!(
        "Event `{source}` {event_id}, published by {publisher}, for your subscription {}{label}.\
         \n\n{}\n\nPayload:\n{payload}",
        subscription.id, subscription.prompt
    )
}

/// The bus kept for `path` in `log`, when the database holds it.
///
/// # Errors
///
/// The database could not be read, or holds rows that do not parse.
pub fn held_event_bus_in(log: &Log, path: &Path) -> anyhow::Result<Option<EventBusState>> {
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
        Some(log) => held_event_bus_in(log, path),
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
    store_event_bus_in(log, path, state, StoredSources::BusOnly)?.wait_blocking()
}

/// Store the bus whose file is `path` in `log`, the sources `stored` keeps:
/// the receipt says whether it was committed.
///
/// # Errors
///
/// A record does not serialize.
pub fn store_event_bus_in(
    log: &Log,
    path: &Path,
    state: &EventBusState,
    stored: StoredSources,
) -> anyhow::Result<Receipt<bool>> {
    Ok(log.replace_sources(
        &StateFile::EventBus.scope(path),
        state.rows(stored)?,
        Origin::Written,
    ))
}
