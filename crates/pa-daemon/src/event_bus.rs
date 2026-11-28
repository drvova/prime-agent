//! The daemon's event bus: the sources sessions define, the subscriptions they
//! hold, and what a publication delivers (`event_bus` supervisor command; the
//! wire vocabulary is [`pa_types::events`]).
//!
//! A source is a named service with a JSON schema (an object schema) for its
//! payloads, owned by the session that defined it: only the owner publishes or
//! removes it. A subscription names a source, a filter (payload keys whose
//! values must be equal), and the prompt the subscriber's turn starts from.
//! The rules are pa-core's (`pa_core::agent_state::EventBusState::apply`,
//! which a headless runtime's bus applies too). Publishing validates the
//! payload against the schema and yields one delivery per matching
//! subscription of another session; the supervisor routes each as
//! an agent message (`supervisor.rs` in this module), which brings the
//! message path's lanes, idle wake, wake budget and queue cap with it.
//!
//! The bus is daemon-wide and survives daemon restarts: its state is kept in
//! the agent database (`pa_core::agent_state`, its sources and subscriptions
//! as rows), under the path of its file in the supervisor's descriptor
//! directory, and stored whole on every change. Until the database holds it
//! (and with the file transcript store, always) the file is read, and with
//! the file store rewritten atomically. A file that does not parse is set
//! aside (`.corrupt`) and the bus starts empty.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use pa_core::agent_state::{EventBusChange, SourceKinds, TopicSubscriptions};
use pa_types::events::{EventBusRequest, EventSubscriptionRecord};
use serde_json::Value;

mod link;
mod supervisor;

pub(crate) use link::LinkEventBusController;
#[cfg(test)]
mod tests;

/// The bus's file in the supervisor's descriptor directory.
pub(crate) const EVENT_BUS_FILE_NAME: &str = "event-bus.json";
/// The session a request comes from, as the supervisor resolved its worker token.
pub(crate) type Requester = pa_core::agent_state::EventRequester;

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
type BusState = pa_core::agent_state::EventBusState;

pub(crate) struct EventBus {
    path: PathBuf,
    state: Mutex<BusState>,
}

impl EventBus {
    /// The bus kept at `path`: its saved state, or an empty bus when there is
    /// none or it does not parse (that file is renamed aside first, and `log`
    /// says so).
    pub(crate) fn load(
        path: PathBuf,
        log: &pa_core::platform::daemon_endpoint::RotatingLog,
    ) -> Self {
        let held = pa_core::agent_state::held_event_bus(&path).unwrap_or_else(|error| {
            log.append(&format!(
                "event bus state for {} is unreadable in the database ({error:#}); starting empty",
                path.display()
            ));
            Some(BusState::default())
        });
        let state = match held {
            Some(state) => state,
            None => match std::fs::read_to_string(&path) {
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
            },
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
        let saves = !matches!(request, EventBusRequest::List);
        let handled = match state.apply(
            requester,
            request,
            now,
            TopicSubscriptions::Refused,
            SourceKinds::BusOnly,
        )? {
            EventBusChange::Answer(answer) => Handled::Answer(answer),
            EventBusChange::Publish { source, payload } => {
                let event_id = format!("evt-{}", uuid::Uuid::now_v7());
                let deliveries: Vec<Delivery> = state
                    .subscriptions
                    .iter()
                    .filter(|subscription| {
                        subscription.source == source.name
                            && subscription.session_id != requester.session_id
                            && subscription
                                .filter
                                .iter()
                                .all(|(key, value)| payload.get(key) == Some(value))
                    })
                    .map(|subscription| Delivery {
                        message: pa_core::agent_state::event_delivery_message(
                            &source.name,
                            &event_id,
                            &source.owner_name,
                            subscription,
                            &payload,
                        ),
                        subscription: subscription.clone(),
                    })
                    .collect();
                Handled::Publish(Publication {
                    event_id,
                    deliveries,
                })
            }
        };
        if saves {
            self.save(&state)?;
        }
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
        if pa_core::agent_state::store_event_bus(&self.path, state)
            .with_context(|| format!("save the event bus for {}", self.path.display()))?
        {
            return Ok(());
        }
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
