//! Agent messaging and observation host requests: validation helpers, message
//! ids and prompts, controller traits, and kernel host-handler registration.

use std::future::Future;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

pub const AGENT_MESSAGE_CUSTOM_TYPE: &str = "agent_message";
/// The queue-strip preview label for a delivered agent message
/// (`queuedAgentMessagePreview` renders "<label>: <details.message>").
pub const AGENT_MESSAGE_RECEIVED_PREVIEW_LABEL: &str = "Agent message received";
pub const AGENT_MESSAGE_SOURCE: &str = "agent_message";
pub const AGENT_MESSAGE_ID_PREFIX: &str = "agentmsg_";
pub const DEFAULT_AGENT_MESSAGE_MAX_CHARS: usize = 16_384;
pub const DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION: usize = 20;
/// The per-sender token bucket capacity: three deliveries burst before
/// the refill paces them.
pub const DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY: usize = 3;
/// One rate-limit token per sender per this window.
pub const DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS: u64 = 1_000;
/// The wakes one sender may spend on one idle receiver per
/// [`DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS`]: the receiving worker refuses the
/// next idle-waking delivery (deliveries to a busy receiver queue, unbudgeted).
pub const DEFAULT_AGENT_MESSAGE_WAKE_BUDGET: usize = 20;
/// The trailing window the wake budget counts over (10 minutes).
pub const DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS: u64 = 600_000;
/// The receipt `deliveryStatus` of a delivery the receiver's wake budget refused.
pub const AGENT_MESSAGE_WAKE_REFUSED_STATUS: &str = "wake_refused";

pub const AGENT_OBSERVE_PREVIEW_MAX_CHARS: usize = 240;
pub const AGENT_OBSERVE_IMPORT_NAME: &str = "agent_observe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFamilyRelationship {
    Parent,
    Sibling,
    Child,
}

impl std::fmt::Display for AgentFamilyRelationship {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AgentFamilyRelationship {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentFamilyRelationship::Parent => "parent",
            AgentFamilyRelationship::Sibling => "sibling",
            AgentFamilyRelationship::Child => "child",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "parent" => Some(AgentFamilyRelationship::Parent),
            "sibling" => Some(AgentFamilyRelationship::Sibling),
            "child" => Some(AgentFamilyRelationship::Child),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageDeliveryStatus {
    Delivered,
    Queued,
    /// The idle receiver refused to wake: this sender spent its wake budget
    /// there; the message was not delivered. `retry_after_ms` is when the
    /// oldest counted wake leaves the window.
    WakeRefused {
        retry_after_ms: u64,
    },
}

impl AgentMessageDeliveryStatus {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentMessageDeliveryStatus::Delivered => "delivered",
            AgentMessageDeliveryStatus::Queued => "queued",
            AgentMessageDeliveryStatus::WakeRefused { .. } => AGENT_MESSAGE_WAKE_REFUSED_STATUS,
        }
    }
}

/// How one delivery was addressed: a broadcast (`send("all", ...)`) never ends an
/// `agent_message.wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageKind {
    Direct,
    Broadcast,
}

impl AgentMessageKind {
    /// The delivery payload's `messageKind` value; a direct delivery omits the field.
    pub const BROADCAST_WIRE: &'static str = "broadcast";

    /// The `messageKind` a delivery payload carries (`None`: a direct delivery).
    #[must_use]
    pub fn from_wire(value: Option<&str>) -> Self {
        match value {
            Some(Self::BROADCAST_WIRE) => AgentMessageKind::Broadcast,
            Some(_) | None => AgentMessageKind::Direct,
        }
    }

    /// The `messageKind` a delivery payload carries (`None` for a direct delivery).
    #[must_use]
    pub fn to_wire(self) -> Option<&'static str> {
        match self {
            AgentMessageKind::Direct => None,
            AgentMessageKind::Broadcast => Some(Self::BROADCAST_WIRE),
        }
    }
}

/// `agent_message.send` input.
#[derive(Debug, Clone)]
pub struct AgentMessageSendInput {
    pub target: String,
    pub message: String,
    pub receiver_role: Option<AgentFamilyRelationship>,
    pub kind: AgentMessageKind,
}

#[derive(Debug, Clone)]
pub struct AgentMessageReceipt {
    pub id: String,
    /// The target's active session id (TS `target.activeSessionId`).
    pub target: String,
    /// The target endpoint's session id (TS `target.sessionId`).
    pub target_session_id: Option<String>,
    pub target_session_name: Option<String>,
    pub target_runtime_kind: Option<String>,
    pub message: String,
    pub delivery_status: AgentMessageDeliveryStatus,
    pub delivery_mode: Option<&'static str>,
    pub receiver_role: Option<AgentFamilyRelationship>,
    pub delivered_at: Option<String>,
    pub queued_at: Option<String>,
}

/// One addressable family member: a parent, sibling, or child of this
/// session, keyed by a routable target selector.
#[derive(Debug, Clone)]
pub struct AgentFamilyMember {
    pub relationship: AgentFamilyRelationship,
    /// The target selector the controller can deliver to: the session id for
    /// local family members, the active session id for peers on another worker.
    pub id: String,
    pub name: Option<String>,
    /// Extra selector forms that resolve to this same member: the
    /// supervisor-backed controller lists a child's RLM child id and
    /// persisted session id here, so every roster identifier addresses the
    /// child while broadcast sends stay one-per-member.
    pub aliases: Vec<String>,
}

impl AgentFamilyMember {
    /// TS `agentFamilyMemberName`: the name, else the id.
    #[must_use]
    pub fn member_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub(crate) fn matches_selector(&self, selector: &str) -> bool {
        self.member_name() == selector
            || self.id == selector
            || self.aliases.iter().any(|alias| alias == selector)
    }
}

/// The controller the daemon supplies for `agent_message.*` requests.
pub trait AgentMessageController: Send + Sync {
    /// The addressable family: the parent, siblings, and children of this
    /// session, excluding the session itself.
    fn family(&self) -> impl Future<Output = anyhow::Result<Vec<AgentFamilyMember>>> + Send;
    fn send_agent_message(
        &self,
        input: AgentMessageSendInput,
    ) -> impl Future<Output = anyhow::Result<AgentMessageReceipt>> + Send;
    /// Park until a direct message matching `request` arrives at THIS session (a broadcast never
    /// ends it), `request.timeout` passes, or the awaited session waits (transitively) on this
    /// one, answered at once as a cycle. While parked, the session reports `waitingOn` (the one
    /// awaited member, when the request names one) and holds no admission slot; the message body
    /// still arrives once, as the ordinary agent-message turn. Controllers without a receiving
    /// side keep this default refusal.
    fn wait_for_agent_message(
        &self,
        request: AgentMessageWaitRequest,
    ) -> impl Future<Output = anyhow::Result<AgentMessageWaitOutcome>> + Send {
        let _ = request;
        std::future::ready(Err(anyhow::anyhow!(
            "agent_message.wait needs a daemon session"
        )))
    }
}

/// Message payload for the rendered `[agent-message from ...]` prompt.
#[derive(Debug, Clone, Default)]
pub struct AgentMessagePromptPayload {
    pub message: String,
    pub sender_name: String,
    pub from_relationship: Option<AgentFamilyRelationship>,
}

#[must_use]
pub fn create_agent_session_message_id() -> String {
    format!("{AGENT_MESSAGE_ID_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Distinguishes agent-to-agent ids from synthetic prompt ids.
#[must_use]
pub fn is_agent_session_message_id(id: Option<&str>) -> bool {
    id.is_some_and(|id| id.starts_with(AGENT_MESSAGE_ID_PREFIX))
}

/// Normalize and validate an outgoing message body.
///
/// # Errors
///
/// Error when the message is empty after trimming or longer than the default limit.
pub fn normalize_agent_session_message(message: &str) -> anyhow::Result<String> {
    normalize_agent_session_message_limited(message, DEFAULT_AGENT_MESSAGE_MAX_CHARS)
}

/// Normalize and validate an outgoing message body with an explicit
/// character limit.
///
/// # Errors
///
/// Error when the message is empty after trimming or longer than `max_chars`.
pub fn normalize_agent_session_message_limited(
    message: &str,
    max_chars: usize,
) -> anyhow::Result<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Agent session message cannot be empty");
    }
    if trimmed.chars().count() > max_chars {
        anyhow::bail!(
            "Agent session message is too long: {} chars exceeds {max_chars}",
            trimmed.chars().count()
        );
    }
    Ok(trimmed.to_string())
}

/// Reject broadcast targets: only direct messaging is supported.
/// Normalize a direct-messaging target and reject broadcast wildcards.
///
/// # Errors
///
/// Error when the target is empty after trimming or names the broadcast wildcard.
pub fn assert_direct_agent_message_target(target: &str) -> anyhow::Result<String> {
    let normalized = target.trim();
    if normalized.is_empty() {
        anyhow::bail!("Agent message target cannot be empty");
    }
    if normalized == "*"
        || normalized.eq_ignore_ascii_case("all")
        || normalized.eq_ignore_ascii_case("broadcast")
    {
        anyhow::bail!("Broadcast agent messaging is not supported");
    }
    Ok(normalized.to_string())
}

/// Guard the target session's pending-work capacity.
///
/// # Errors
///
/// Error when the target's unfinished action count has reached the pending-work limit.
pub fn assert_agent_message_queue_capacity(
    unfinished_action_count: usize,
    max_pending: usize,
) -> anyhow::Result<()> {
    if unfinished_action_count >= max_pending {
        anyhow::bail!(
            "Target session has too many pending messages: {unfinished_action_count} unfinished, limit is {max_pending}"
        );
    }
    Ok(())
}

/// Names interpolated into a `[<kind> ...]` header line must not carry the
/// characters that delimit the header itself (brackets, newlines, commas,
/// or ":"): runs of those collapse into one space, then the value trims.
pub(crate) fn sanitize_message_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for char in value.chars() {
        let delimiter = char.is_whitespace() || matches!(char, ',' | ':' | '[' | ']');
        if delimiter {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(char);
        }
    }
    out
}

/// The heads of the bracket notices the harness renders into a model context (`[agent-message
/// from ...]`, `[child-exited: ...]`, `[cell-done ...]`, `[task from parent]`, `[loop guard]`,
/// ...), lowercase. A delivered body line that opens with one is fenced, so a body can never
/// impersonate the harness or another sender. Prefix matches: `kernel-state` also covers
/// `[kernel-state-reset]`, and `python-state` the Python kernel's notices in saved sessions.
const HARNESS_NOTICE_HEADS: &[&str] = &[
    "agent-message",
    "autonomous-continuation",
    "autonomous-status",
    "bash-done",
    "bash-lost",
    "branch-summary",
    "cell-done",
    "child-exited",
    "child-failed",
    "compaction-summary",
    "factory-progress",
    "from ",
    "goal",
    "harness-digest",
    "heartbeat",
    "kernel-state",
    "loop guard",
    "model-prompt-error",
    "python-skills-unavailable",
    "python-state",
    "self-refinement",
    "task from",
    "update-complete",
    "user-refinement",
];

/// Quote (`> `) every body line that opens, after any indentation and in any letter case, with a
/// harness notice head; every other line stays verbatim.
fn fence_agent_message_body(body: &str) -> String {
    body.split('\n')
        .map(|line| {
            let head = line
                .trim_start()
                .strip_prefix('[')
                .map(str::to_ascii_lowercase);
            if head.is_some_and(|head| {
                HARNESS_NOTICE_HEADS
                    .iter()
                    .any(|notice| head.starts_with(notice))
            }) {
                format!("> {line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rendered prompt a receiving context sees: the daemon-derived header, then the body with
/// any line that could pass for a harness notice fenced.
#[must_use]
pub fn create_agent_session_message_prompt(payload: &AgentMessagePromptPayload) -> String {
    let sender = sanitize_message_header_value(&payload.sender_name);
    let sender = if sender.is_empty() {
        "unknown".to_string()
    } else {
        sender
    };
    let sender = match payload.from_relationship {
        Some(relationship) => format!("{}:{sender}", relationship.as_str()),
        None => sender,
    };
    let body = fence_agent_message_body(&payload.message);
    format!("[agent-message from {sender}]\n\n{body}")
}

/// The receiving side's custom-row inputs.
#[derive(Debug, Clone)]
pub struct AgentSessionMessageRowPayload<'a> {
    pub id: &'a str,
    /// The rendered prompt (stored as the row `content`; the model context
    /// reads it).
    pub prompt: &'a str,
    /// The raw delivered body.
    pub message: &'a str,
    pub from: &'a Value,
    pub from_relationship: Option<AgentFamilyRelationship>,
    pub target: Option<&'a Value>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// The `role: "custom"` agent-message row the receiving session's transcript
/// holds (TS `createAgentSessionMessage`): `content` is the rendered prompt,
/// so the turn's model context matches the plain-prompt delivery.
#[must_use]
pub fn create_agent_session_message_row(payload: &AgentSessionMessageRowPayload<'_>) -> Value {
    let mut details = serde_json::Map::new();
    details.insert("id".to_string(), json!(payload.id));
    details.insert("message".to_string(), json!(payload.message));
    details.insert("from".to_string(), payload.from.clone());
    if let Some(relationship) = payload.from_relationship {
        details.insert("fromRelationship".to_string(), json!(relationship.as_str()));
    }
    if let Some(target) = payload.target {
        details.insert("target".to_string(), target.clone());
    }
    json!({
        "role": "custom",
        "customType": AGENT_MESSAGE_CUSTOM_TYPE,
        "content": payload.prompt,
        "display": true,
        "details": Value::Object(details),
        "timestamp": payload.timestamp,
    })
}

/// Parse the message id out of the pre-bracket-grammar transcript header.
#[must_use]
pub fn parse_agent_session_message_prompt_id(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let offset = usize::from(lines.first().is_some_and(|line| line.starts_with("[from ")));
    if lines.get(offset).copied() != Some("Agent-to-agent message received.")
        || lines.get(offset + 1).copied()
            != Some(format!("Source: {AGENT_MESSAGE_SOURCE}").as_str())
    {
        return None;
    }
    let to_line_index = if lines
        .get(offset + 2)
        .is_some_and(|line| line.starts_with("From: "))
    {
        offset + 3
    } else {
        offset + 2
    };
    if !lines
        .get(to_line_index)
        .is_some_and(|line| line.starts_with("To: "))
    {
        return None;
    }
    let id_line = lines.get(to_line_index + 1)?;
    let id = id_line.strip_prefix("Message id: ")?;
    (!id.is_empty() && id.starts_with(AGENT_MESSAGE_ID_PREFIX)).then(|| id.to_string())
}

#[must_use]
pub fn is_agent_session_message_prompt(text: &str) -> bool {
    parse_agent_session_message_prompt_id(text).is_some()
}

/// The TS `AgentSessionMessageEndpoint` the receipt carries as `target`.
fn receipt_target_value(receipt: &AgentMessageReceipt) -> Value {
    let mut target = json!({
        "activeSessionId": receipt.target,
        "sessionId": receipt.target_session_id.clone().unwrap_or_default(),
    });
    if let Some(name) = receipt
        .target_session_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        target["sessionName"] = json!(name);
    }
    if let Some(kind) = receipt
        .target_runtime_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
    {
        target["runtimeKind"] = json!(kind);
    }
    target
}

fn receipt_value(receipt: &AgentMessageReceipt) -> Value {
    let mut value = json!({
        "id": receipt.id,
        "source": AGENT_MESSAGE_SOURCE,
        "target": receipt_target_value(receipt),
        "message": receipt.message,
        "deliveryStatus": receipt.delivery_status.as_str(),
        "deliveredAt": receipt.delivered_at,
        "queuedAt": receipt.queued_at,
        "deliveryMode": receipt.delivery_mode,
        "receiverRole": receipt.receiver_role.map(|role| role.as_str()),
    });
    match receipt.delivery_status {
        AgentMessageDeliveryStatus::WakeRefused { retry_after_ms } => {
            value[RETRY_AFTER_MS_FIELD] = json!(retry_after_ms);
            value["error"] = json!(format!(
                "Agent message not delivered: the idle receiver refused to wake; you already woke it {DEFAULT_AGENT_MESSAGE_WAKE_BUDGET} times in the last {} minutes. Retry after {retry_after_ms} ms",
                DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS / 60_000
            ));
        }
        AgentMessageDeliveryStatus::Delivered | AgentMessageDeliveryStatus::Queued => {}
    }
    value
}

/// Register `agent_message.*` handlers onto a handler map. The `send`
/// contract matches the kernel skill: role/addressed sends carry
/// `receiver_role`/`receiver_name`, and `target: "all"` is the broadcast
/// form; other positional targets are rejected.
pub fn register_agent_message_host_handlers<C: AgentMessageController + 'static>(
    controller: std::sync::Arc<C>,
    handlers: &mut HostRequestHandlers,
) {
    register_agent_message_host_handlers_with_clock(
        controller,
        handlers,
        std::sync::Arc::new(std::time::Instant::now),
    );
}

/// [`register_agent_message_host_handlers`] over an explicit rate-bucket clock. The bucket is
/// this registration's: one per sending session, every delivery attempt takes one token.
pub(crate) fn register_agent_message_host_handlers_with_clock<
    C: AgentMessageController + 'static,
>(
    controller: std::sync::Arc<C>,
    handlers: &mut HostRequestHandlers,
    clock: AgentMessageClock,
) {
    let limiter = std::sync::Arc::new(AgentMessageRateLimiter::new(clock));
    handlers.register(
        "agent_message.list_agents",
        host_handler(|_payload| {
            Box::pin(async {
                Err(anyhow::anyhow!(
                    "agent_message.list_agents was removed; the family roster is `(agent-roster)`."
                ))
            })
        }),
    );
    let waiter = controller.clone();
    handlers.register(
        "agent_message.wait",
        host_handler(move |payload| {
            let controller = waiter.clone();
            Box::pin(async move {
                wait::handle_agent_message_wait(controller.as_ref(), &payload.data).await
            })
        }),
    );
    handlers.register(
        "agent_message.send",
        host_handler(move |payload| {
            let controller = controller.clone();
            let limiter = limiter.clone();
            Box::pin(async move {
                let data = payload.data;
                let Some(message) = data.get("message").and_then(Value::as_str) else {
                    return Err(anyhow::anyhow!(
                        "agent_message.send message must be a string"
                    ));
                };
                // TS normalizes inside every send (broadcast included), so
                // normalizing up front is behaviorally identical.
                let message = normalize_agent_session_message(message)?;
                // Broadcast form (`target: "all"`): one send per family
                // member, all-settled into a receipts array.
                if let Some(target) = data.get("target").and_then(Value::as_str) {
                    if target != "all" {
                        return Err(anyhow::anyhow!(
                            "positional agent_message.send targets are not supported; use receiver_role and receiver_name"
                        ));
                    }
                    if data.get("receiver_role").is_some() || data.get("receiver_name").is_some() {
                        return Err(anyhow::anyhow!(
                            "agent_message.send broadcast cannot be combined with receiver_role/receiver_name"
                        ));
                    }
                    let family = controller.family().await?;
                    let mut receipts = Vec::with_capacity(family.len());
                    for member in family {
                        // One token per recipient: a throttled recipient settles with the
                        // retry hint instead of a delivery.
                        if let Err(throttled) = limiter.acquire() {
                            receipts.push(json!({
                                "target": member.id,
                                "error": throttled.to_string(),
                                RETRY_AFTER_MS_FIELD: throttled.retry_after_ms,
                            }));
                            continue;
                        }
                        let result = controller
                            .send_agent_message(AgentMessageSendInput {
                                target: member.id.clone(),
                                message: message.clone(),
                                receiver_role: Some(member.relationship),
                                kind: AgentMessageKind::Broadcast,
                            })
                            .await;
                        receipts.push(match result {
                            Ok(receipt) => receipt_value(&receipt),
                            Err(error) => json!({
                                "target": member.id,
                                "error": error.to_string(),
                            }),
                        });
                    }
                    return Ok(json!({ "receipts": receipts }));
                }
                // Role-addressed form: resolve the receiver through the
                // family roster, then send once. A name without a role is
                // a sibling or a child, whichever it names.
                let role = match data.get("receiver_role").filter(|value| !value.is_null()) {
                    None => None,
                    Some(Value::String(role)) if role == "parent" => {
                        Some(AgentFamilyRelationship::Parent)
                    }
                    Some(Value::String(role)) if role == "sibling" => {
                        Some(AgentFamilyRelationship::Sibling)
                    }
                    Some(Value::String(role)) if role == "child" => {
                        Some(AgentFamilyRelationship::Child)
                    }
                    Some(_) => {
                        return Err(anyhow::anyhow!(
                            "agent_message.send receiver_role must be \"parent\", \"sibling\", or \"child\""
                        ))
                    }
                };
                let receiver_name = data.get("receiver_name").filter(|value| !value.is_null());
                let parent = role == Some(AgentFamilyRelationship::Parent);
                if parent && receiver_name.is_some() {
                    return Err(anyhow::anyhow!(
                        "agent_message.send receiver_name must be omitted for parent messages"
                    ));
                }
                let selector: Option<&str> = if parent {
                    None
                } else {
                    match receiver_name
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                    {
                        Some(name) => Some(name),
                        None if role.is_none() => {
                            return Err(anyhow::anyhow!(
                                "agent_message.send needs receiver_role \"parent\" or a receiver_name"
                            ))
                        }
                        None => {
                            return Err(anyhow::anyhow!(
                                "agent_message.send receiver_name is required for sibling and child messages"
                            ))
                        }
                    }
                };
                // TS renders the receiver in errors via JSON.stringify.
                let rendered_receiver = match receiver_name {
                    Some(Value::String(name)) => format!("\"{name}\""),
                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                    None => "null".to_string(),
                };
                let family = controller.family().await?;
                let matches: Vec<AgentFamilyMember> = family
                    .into_iter()
                    .filter(|member| match role {
                        Some(AgentFamilyRelationship::Parent) => {
                            member.relationship == AgentFamilyRelationship::Parent
                        }
                        Some(role) => {
                            member.relationship == role
                                && selector.is_some_and(|selector| member.matches_selector(selector))
                        }
                        None => {
                            member.relationship != AgentFamilyRelationship::Parent
                                && selector.is_some_and(|selector| member.matches_selector(selector))
                        }
                    })
                    .collect();
                let named = role.map_or("sibling or child", |role| role.as_str());
                // Exactly one match resolves; zero or many keep the TS
                // error strings.
                let member = match matches.as_slice() {
                    [only] => only,
                    [] => {
                        return Err(anyhow::anyhow!(if parent {
                            "No parent matches the current agent".to_string()
                        } else {
                            format!("No {named} matches {rendered_receiver}")
                        }))
                    }
                    _ => {
                        return Err(anyhow::anyhow!(
                            format!("{named} selector {rendered_receiver} is ambiguous")
                        ))
                    }
                };
                limiter.acquire()?;
                let receipt = controller
                    .send_agent_message(AgentMessageSendInput {
                        target: member.id.clone(),
                        message: message.clone(),
                        receiver_role: Some(member.relationship),
                        kind: AgentMessageKind::Direct,
                    })
                    .await?;
                Ok(receipt_value(&receipt))
            })
        }),
    );
}

mod wait;
pub(crate) use wait::observe_agent_message_waits;
pub use wait::{
    create_agent_message_preview, AgentMessageWaitArrival, AgentMessageWaitFilter,
    AgentMessageWaitOutcome, AgentMessageWaitRequest, AGENT_MESSAGE_PREVIEW_MAX_CHARS,
    AGENT_MESSAGE_WAIT_MAX_TIMEOUT_MS,
};

mod waits;
pub use waits::{
    wait_cycle, wait_for_arrival, AgentMessageArrival, AgentMessageWait, AgentMessageWaitSenders,
    AgentMessageWaitStart, AgentMessageWaits, StillQueued, WaitingOnChanged,
};

mod rate_limit;
pub(crate) use rate_limit::{observe_agent_message_refusals, AgentMessageRefusals};
use rate_limit::{AgentMessageClock, AgentMessageRateLimiter, RETRY_AFTER_MS_FIELD};

/// The bounce body: the receiver label, the fact, and the dropped body —
/// truncated so the whole notice stays inside the delivery size limit the
/// receiving delivery enforces. Both daemons' queue clears bounce a dropped
/// agent message to its sender with it.
#[must_use]
pub fn cleared_agent_message_bounce_text(receiver: &str, message: &str) -> String {
    let limit = DEFAULT_AGENT_MESSAGE_MAX_CHARS;
    let opening = "Your agent message was cleared from ";
    let ending = "'s queue before delivery: ";
    // Session names are unbounded. Reserve the notice itself before fitting
    // the receiver label, then spend the remaining characters on the body.
    let receiver_budget = limit.saturating_sub(opening.chars().count() + ending.chars().count());
    let receiver: String = receiver.chars().take(receiver_budget).collect();
    let prefix = format!("{opening}{receiver}{ending}");
    let budget = limit.saturating_sub(prefix.chars().count());
    let body: String = message.chars().take(budget).collect();
    format!("{prefix}{body}")
}

// The observe half lives in the child module; the pub use re-exports keep
// every external agent_messaging:: path stable.
mod observe;
pub use observe::{
    create_agent_observe_message_preview, find_observe_target, normalize_observe_limit,
    normalize_observe_max_chars, register_agent_observe_host_handlers, AgentFamilyStatus,
    AgentObserveActivity, AgentObserveController, AgentObserveListResult,
    AgentObserveMessagePreview, AgentObserveRecentMessagesResult, AgentObserveSummary,
};

#[cfg(test)]
mod tests;
