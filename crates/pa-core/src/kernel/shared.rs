//! Shared constants, result shapes, and host-bridge types for the kernel layer.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

/// Per-cell stdout/stderr cap. `HeadTailBuffer` splits it between head and
/// tail, so a long failing run keeps its printed verdict.
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 65_536;

/// The `factory_activity` out-of-band frame's action vocabulary, mirroring
/// the kernel executor's `ACTIVITY_ACTIONS` (the `/factory` view's bridge).
pub const FACTORY_ACTIVITY_ACTIONS: [&str; 6] =
    ["graph", "status", "watch", "run", "stop", "resume"];

/// Upper bound on one `factory_activity` watch's `timeoutMs` (the kernel
/// caps its own at 60s; the host bridge pins the view's polling cadence
/// lower). Mirrors the kernel's `ACTIVITY_TIMEOUT_MS_CAP` for the preflight.
pub const FACTIVITY_WATCH_TIMEOUT_MS_CAP: u64 = 60_000;

/// Hard ceiling on a single attachment's base64 payload, a defensive guard against a runaway direct
/// display emit. The `attach-image` skill caps its own images well under this.
pub const MAX_ATTACHMENT_DATA_CHARS: usize = 10_000_000;

/// The kernel host request that reports a background bash completion
/// (the `[bash-done]` lane). The daemon registers the handler
/// (`bash_notices`), and the provisioner keys the bounded-await bootstrap
/// default on its presence.
pub const BASH_COMPLETED_REQUEST_TYPE: &str = "bash.completed";

/// The kernel host request that reports background commands that have
/// written nothing for a while (the `[bash-quiet]` notice). Without a
/// handler for it the kernel does not watch for quiet commands.
pub(crate) const BASH_QUIET_REQUEST_TYPE: &str = "bash.quiet";

/// The kernel host request that ends the turn from inside a cell
/// (`rlm.end_turn()`). The host-request dispatch answers it by marking
/// the executing cell's result, so the turn ends once the batch settles.
pub const END_TURN_REQUEST_TYPE: &str = "rlm.end_turn";

/// Fires when the kernel's last live background `bash()` handle settles, so owed continuations can
/// resume.
pub type BackgroundWorkSettledCallback = Arc<dyn Fn() + Send + Sync>;

/// One step in a backgrounded cell's life, for adoption counts and the learned
/// cell window (never code, output, or an id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundCellFact {
    /// A cell outlived its yield window and continued in the background.
    Backgrounded,
    /// A backgrounded cell ended with a value, this long after it started
    /// (`None` when its start was not seen).
    EndedOk {
        ran_for: Option<std::time::Duration>,
    },
    /// A backgrounded cell ended with an exception, this long after it started.
    EndedWithError {
        ran_for: Option<std::time::Duration>,
    },
    /// A backgrounded cell was interrupted.
    Interrupted,
}

/// Receives each [`BackgroundCellFact`] the kernel reports.
pub type BackgroundCellObserver = Arc<dyn Fn(BackgroundCellFact) + Send + Sync>;

/// One `wait-until` wait inside a cell, for adoption counts (never a
/// condition, a value, or a duration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWaitFact {
    /// A wait started.
    Started,
    /// A wait ended with its condition met.
    Met,
    /// A wait ended at its timeout.
    TimedOut,
}

/// Receives each [`CellWaitFact`] the kernel reports.
pub type CellWaitObserver = Arc<dyn Fn(CellWaitFact) + Send + Sync>;

/// Callback receiving an agent message sent late by the kernel.
pub type LateSentAgentMessageCallback = Arc<dyn Fn(KernelSentAgentMessage) + Send + Sync>;

/// One file edit, captured from a [`DIFF_DISPLAY_MIME`] display payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelDiffDisplay {
    pub path: String,
    pub old_str: String,
    pub new_str: String,
    /// 1-based line where `old_str` begins in the file, for absolute line numbers.
    pub start_line: Option<u64>,
}

/// One media attachment, captured from an [`ATTACHMENT_DISPLAY_MIME`] display payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelAttachment {
    pub mime_type: String,
    /// base64-encoded bytes.
    pub data: String,
    /// Source path, surfaced to the TUI renderer.
    pub path: Option<String>,
}

/// A delivery receipt for one agent message sent from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSentAgentMessage {
    pub id: String,
    pub message: String,
    pub delivery_status: SentDeliveryStatus,
    pub receiver_role: Option<ReceiverRole>,
    pub target: SentAgentMessageTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentDeliveryStatus {
    Delivered,
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverRole {
    Parent,
    Sibling,
    Child,
}

impl ReceiverRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiverRole::Parent => "parent",
            ReceiverRole::Sibling => "sibling",
            ReceiverRole::Child => "child",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentAgentMessageTarget {
    pub active_session_id: String,
    pub session_id: String,
    pub session_name: Option<String>,
}

/// The `bash()` commands one cell started, summarized for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelBashCommands {
    pub first: String,
    pub count: usize,
    pub lines: usize,
    /// Whether any of them ran elevated (`'elevated #t`).
    pub elevated: bool,
}

/// The bounded `bash()` awaits one cell ran: how many returned the finished
/// result inside their wait window, and how many yielded a still-running
/// snapshot (each yield usually costs the session another model round trip).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KernelBashWaits {
    pub completed: u64,
    pub yielded: u64,
}

/// One failed `bash()` command a cell awaited, captured from a
/// [`COMMAND_FAILURE_DISPLAY_MIME`] display payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelCommandFailure {
    pub command: String,
    pub exit_code: i64,
    /// The command's whole output, as its result held it.
    pub output: String,
}

impl KernelCommandFailure {
    /// Parse one [`COMMAND_FAILURE_DISPLAY_MIME`] payload (`None` when malformed).
    #[must_use]
    pub fn from_display(payload: &Value) -> Option<Self> {
        Some(Self {
            command: payload.get("command")?.as_str()?.to_string(),
            exit_code: payload.get("exitCode")?.as_i64()?,
            output: payload.get("output")?.as_str()?.to_string(),
        })
    }
}

/// A typed request from a cell running in the kernel to the host.
#[derive(Debug, Clone)]
pub struct HostRequestPayload {
    /// The request payload.
    pub data: Value,
}

pub type HostHandlerFuture = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;
pub type HostHandlerFn = Arc<dyn Fn(HostRequestPayload) -> HostHandlerFuture + Send + Sync>;

/// Handles one typed request from a cell running in the kernel.
/// The returned value is delivered verbatim to the caller.
pub trait HostRequestHandler: Send + Sync {
    fn handle(&self, payload: HostRequestPayload) -> HostHandlerFuture;
}

impl<F> HostRequestHandler for F
where
    F: Fn(HostRequestPayload) -> HostHandlerFuture + Send + Sync,
{
    fn handle(&self, payload: HostRequestPayload) -> HostHandlerFuture {
        self(payload)
    }
}

/// Build a [`HostRequestHandler`] from a closure returning a future.
pub fn host_handler<F, Fut>(f: F) -> HostHandlerFn
where
    F: Fn(HostRequestPayload) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<Value>> + Send + 'static,
{
    Arc::new(move |payload| Box::pin(f(payload)) as HostHandlerFuture)
}

/// Host request handlers keyed by request type (e.g. `rlm.run`, `goal.complete`).
#[derive(Clone, Default)]
pub struct HostRequestHandlers {
    handlers: Arc<HashMap<String, HostHandlerFn>>,
}

impl HostRequestHandlers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, request_type: impl Into<String>, handler: HostHandlerFn) {
        let mut map = Arc::try_unwrap(std::mem::take(&mut self.handlers))
            .unwrap_or_else(|shared| (*shared).clone());
        map.insert(request_type.into(), handler);
        self.handlers = Arc::new(map);
    }

    /// Merge another registry into this one; the other registry's entries
    /// win on key collisions (later registrations override).
    pub fn merge(&mut self, other: Self) {
        let other_map = Arc::try_unwrap(other.handlers).unwrap_or_else(|shared| (*shared).clone());
        let mut map = Arc::try_unwrap(std::mem::take(&mut self.handlers))
            .unwrap_or_else(|shared| (*shared).clone());
        for (key, handler) in other_map {
            map.insert(key, handler);
        }
        self.handlers = Arc::new(map);
    }

    #[must_use]
    pub fn get(&self, request_type: &str) -> Option<&HostHandlerFn> {
        self.handlers.get(request_type)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }
}

/// Parse an [`AGENT_MESSAGE_DISPLAY_MIME`] payload, tolerating malformed input.
pub fn parse_sent_agent_message(payload: &Value) -> Option<KernelSentAgentMessage> {
    let obj = payload.as_object()?;
    let target = obj.get("target")?.as_object()?;
    let id = obj.get("id")?.as_str()?;
    let message = obj.get("message")?.as_str()?;
    let delivery_status = match obj.get("deliveryStatus")?.as_str()? {
        "delivered" => SentDeliveryStatus::Delivered,
        "queued" => SentDeliveryStatus::Queued,
        _ => return None,
    };
    let active_session_id = target.get("activeSessionId")?.as_str()?;
    let session_id = target.get("sessionId")?.as_str()?;
    let receiver_role = match obj.get("receiverRole").and_then(Value::as_str) {
        Some("parent") => Some(ReceiverRole::Parent),
        Some("sibling") => Some(ReceiverRole::Sibling),
        Some("child") => Some(ReceiverRole::Child),
        _ => None,
    };
    Some(KernelSentAgentMessage {
        id: id.to_string(),
        message: message.to_string(),
        delivery_status,
        receiver_role,
        target: SentAgentMessageTarget {
            active_session_id: active_session_id.to_string(),
            session_id: session_id.to_string(),
            session_name: target
                .get("sessionName")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
    })
}
