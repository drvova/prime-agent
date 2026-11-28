//! The event bus's wire vocabulary: what a session asks the daemon's event bus
//! (`event_bus` supervisor command) and the records the bus keeps.
//!
//! An event source is a named service with a JSON schema for its payloads,
//! owned by the session that defined it; only that session publishes it. A
//! subscription names a source, an optional filter (payload keys whose values
//! must be equal), and the prompt its session's turn starts from. Publishing
//! validates the payload against the source's schema and delivers one turn to
//! each matching subscription's session.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How an event reaches a subscribed session: as a follow-up after its
/// running turn (the default), or steering into it at the next boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDeliveryMode {
    #[default]
    FollowUp,
    Steer,
}

impl EventDeliveryMode {
    /// The `deliveryMode` value a message delivery carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp => "follow_up",
            Self::Steer => "steer",
        }
    }
}

/// One request to the event bus, tagged by `op`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum EventBusRequest {
    /// Define (or, by its owner, redefine) a source.
    DefineSource {
        name: String,
        #[serde(default)]
        description: String,
        /// A bus source's payload schema; a timer's or a watch's is its
        /// kind's, and this is `false` or `null`.
        schema: Value,
        /// Makes it a timer source, which fires on its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer: Option<TimerSpec>,
        /// Makes it a folder-watch source, which reports changes on its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        watch: Option<WatchSpec>,
        /// Makes it a webhook source, which publishes each authenticated
        /// `POST` on its own. Boxed: it is many times the other kinds'
        /// size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        webhook: Option<Box<WebhookSpec>>,
    },
    /// Remove one of the requester's own sources and the subscriptions to it.
    RemoveSource { name: String },
    /// Publish one event of a source the requester owns.
    Publish { source: String, payload: Value },
    /// Subscribe the requester to a source.
    Subscribe {
        source: String,
        #[serde(default)]
        filter: Map<String, Value>,
        prompt: String,
        #[serde(default)]
        delivery_mode: EventDeliveryMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Cancel one of the requester's subscriptions.
    Unsubscribe { subscription_id: String },
    /// Every source, and the requester's subscriptions.
    List,
}

/// A defined source, as the bus keeps and lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSourceRecord {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// The owning session's durable id.
    pub owner_session_id: String,
    /// The owner's name when it had one at definition, else its id.
    pub owner_name: String,
    pub defined_at: u64,
    pub published_count: u64,
    /// When it is a timer source: when it fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<TimerSpec>,
    /// When it is a folder-watch source: what it watches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchSpec>,
    /// When it is a webhook source: where a sender posts, never its secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookRecord>,
}

impl EventSourceRecord {
    /// Its kind: a timer, a watch or a webhook by the spec it carries, else
    /// a bus source its owner publishes.
    #[must_use]
    pub fn kind(&self) -> EventSourceKind {
        match (&self.timer, &self.watch, &self.webhook) {
            (Some(_), _, _) => EventSourceKind::Timer,
            (None, Some(_), _) => EventSourceKind::Watch,
            (None, None, Some(_)) => EventSourceKind::Webhook,
            (None, None, None) => EventSourceKind::Bus,
        }
    }
}

/// What publishes a source's events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSourceKind {
    /// Its owner, with `event-publish`.
    Bus,
    /// A timer, on `timer/<name>`.
    Timer,
    /// A folder watch, on `source/<name>/changed` and `source/<name>/error`.
    Watch,
    /// An HTTP listener, on `source/<name>/received`, always Untrusted.
    Webhook,
}

impl EventSourceKind {
    /// Its name, as the kernel and telemetry spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bus => "bus",
            Self::Timer => "timer",
            Self::Watch => "watch",
            Self::Webhook => "webhook",
        }
    }
}

/// When a timer source fires: exactly one of `every` (`5m`), `cron` (five
/// fields in UTC) and `at` (an ISO time), in pa-sources' schedule grammar.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// What a folder-watch source watches: `path` (under the defining session's
/// working directory), optionally only names matching `glob`, in subfolders
/// unless `recursive` is false, coalescing a burst of changes over
/// `debounce-ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WatchSpec {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<u64>,
}

/// What a webhook source listens for, as its definer gives it: `path` (`/`
/// by default) on a loopback port (`port`, or one picked when it is absent
/// or 0), authenticated by `token` (`Authorization: Bearer`), `hmac-secret`
/// (`X-Signature-256`), or both; a token is generated when neither is
/// given. Bodies over `max-body-bytes` (1 MiB by default) are refused.
/// `Debug` never prints a secret.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WebhookSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<u64>,
}

impl std::fmt::Debug for WebhookSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookSpec")
            .field("path", &self.path)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| ".."))
            .field("hmac_secret", &self.hmac_secret.as_ref().map(|_| ".."))
            .field("max_body_bytes", &self.max_body_bytes)
            .finish()
    }
}

/// A webhook source as the bus keeps and lists it: where a sender posts
/// (`url`: `http://127.0.0.1:<port><path>`) and how it authenticates
/// (`auth`: `"token"`, `"hmac-secret"`, or both). The secrets themselves are
/// kept apart (the agent dir's `auth.json`), never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WebhookRecord {
    pub path: String,
    pub port: u16,
    pub url: String,
    pub auth: Vec<String>,
    pub max_body_bytes: u64,
}

/// A subscription, as the bus keeps and lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscriptionRecord {
    pub id: String,
    pub source: String,
    pub filter: Map<String, Value>,
    pub prompt: String,
    pub delivery_mode: EventDeliveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The subscribed session's durable id.
    pub session_id: String,
    /// The subscribed session's working directory (where a saved session
    /// woken for an event is looked up).
    pub cwd: String,
    pub created_at: u64,
    pub delivered_count: u64,
}
