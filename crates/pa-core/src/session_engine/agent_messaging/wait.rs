//! `agent_message.wait`: park the calling cell until a family member's message arrives at this
//! session, the timeout passes, or the wait would close a wait cycle. This half validates the
//! request, resolves the awaited sender through the family roster, and renders the result; the
//! controller owns the receiving side (arrivals, `waitingOn`, cycle detection).

use std::time::Duration;

use super::{
    fence_agent_message_body, json, AgentFamilyMember, AgentFamilyRelationship,
    AgentMessageController, Value,
};

/// The longest `timeout_ms` one wait accepts (10 minutes).
pub const AGENT_MESSAGE_WAIT_MAX_TIMEOUT_MS: u64 = 600_000;
/// The longest preview a wait result (or a traffic entry) carries.
pub const AGENT_MESSAGE_PREVIEW_MAX_CHARS: usize = 240;

/// Which deliveries end one wait.
#[derive(Debug, Clone)]
pub enum AgentMessageWaitFilter {
    /// Any direct message from anyone (broadcasts never end a wait).
    Any,
    /// A direct message from one of these family members (matched by id or alias).
    Members(Vec<AgentFamilyMember>),
}

/// One resolved wait.
#[derive(Debug, Clone)]
pub struct AgentMessageWaitRequest {
    pub filter: AgentMessageWaitFilter,
    /// The one member this wait names, when the filter resolves to exactly one: the session's
    /// `waitingOn`, and where cycle detection starts.
    pub waiting_on: Option<AgentFamilyMember>,
    /// The family at wait start: an arrival's sender role resolves against it.
    pub family: Vec<AgentFamilyMember>,
    pub timeout: Duration,
}

/// The message that ended a wait: identity and a sanitized preview, never the body (the body
/// arrives once, as the ordinary agent-message turn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageWaitArrival {
    pub message_id: String,
    pub sender_name: String,
    pub sender_role: Option<AgentFamilyRelationship>,
    pub preview: String,
}

/// How one wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMessageWaitOutcome {
    Message(AgentMessageWaitArrival),
    Timeout,
    /// The awaited session waits (transitively) on the caller: `blocked_via` names the chain,
    /// from the awaited session back to the one that waits on the caller.
    Cycle {
        blocked_via: Vec<String>,
    },
    /// No family member matches the requested sender.
    NoSender,
}

impl AgentMessageWaitOutcome {
    /// The kernel result: every key present, `null` where the outcome has no value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let (outcome, arrival, blocked_via) = match self {
            AgentMessageWaitOutcome::Message(arrival) => ("message", Some(arrival), None),
            AgentMessageWaitOutcome::Timeout => ("timeout", None, None),
            AgentMessageWaitOutcome::Cycle { blocked_via } => ("cycle", None, Some(blocked_via)),
            AgentMessageWaitOutcome::NoSender => ("no_sender", None, None),
        };
        json!({
            "outcome": outcome,
            "messageId": arrival.map(|arrival| arrival.message_id.clone()),
            "senderName": arrival.map(|arrival| arrival.sender_name.clone()),
            "senderRole": arrival.and_then(|arrival| arrival.sender_role.map(|role| role.as_str())),
            "preview": arrival.map(|arrival| arrival.preview.clone()),
            "blockedVia": blocked_via,
        })
    }
}

/// A message body as a one-line preview of at most [`AGENT_MESSAGE_PREVIEW_MAX_CHARS`]: whitespace
/// runs collapse to one space (no line of it can pass for a header), a leading harness-notice head
/// is fenced like a delivered body line, and an overlong preview ends in `…`.
#[must_use]
pub fn create_agent_message_preview(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let fenced = fence_agent_message_body(&collapsed);
    if fenced.chars().count() <= AGENT_MESSAGE_PREVIEW_MAX_CHARS {
        return fenced;
    }
    let mut preview: String = fenced
        .chars()
        .take(AGENT_MESSAGE_PREVIEW_MAX_CHARS - 1)
        .collect();
    preview.push('…');
    preview
}

/// Validate one `agent_message.wait` request and resolve its sender through the family roster,
/// then hand the wait to the controller.
pub(super) async fn handle_agent_message_wait<C: AgentMessageController>(
    controller: &C,
    data: &Value,
) -> anyhow::Result<Value> {
    let timeout_ms = data
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .filter(|ms| (1..=AGENT_MESSAGE_WAIT_MAX_TIMEOUT_MS).contains(ms))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "agent_message.wait timeout_ms must be an integer from 1 to {AGENT_MESSAGE_WAIT_MAX_TIMEOUT_MS}"
            )
        })?;
    let role = match data.get("sender_role").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(AgentFamilyRelationship::parse)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                    "agent_message.wait sender_role must be \"parent\", \"sibling\", or \"child\""
                )
                })?,
        ),
    };
    let sender = match data.get("sender").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("agent_message.wait sender must be a non-empty string")
                })?
                .to_string(),
        ),
    };
    if role == Some(AgentFamilyRelationship::Parent) && sender.is_some() {
        anyhow::bail!("agent_message.wait sender must be omitted for the parent");
    }
    let family = controller.family().await?;
    let (filter, waiting_on) = if role.is_none() && sender.is_none() {
        (AgentMessageWaitFilter::Any, None)
    } else {
        let members: Vec<AgentFamilyMember> = family
            .iter()
            .filter(|member| {
                role.is_none_or(|role| member.relationship == role)
                    && sender
                        .as_deref()
                        .is_none_or(|selector| member.matches_selector(selector))
            })
            .cloned()
            .collect();
        if members.is_empty() {
            return Ok(AgentMessageWaitOutcome::NoSender.to_value());
        }
        let waiting_on = match members.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        };
        (AgentMessageWaitFilter::Members(members), waiting_on)
    };
    let outcome = controller
        .wait_for_agent_message(AgentMessageWaitRequest {
            filter,
            waiting_on,
            family,
            timeout: Duration::from_millis(timeout_ms),
        })
        .await?;
    Ok(outcome.to_value())
}

/// Hears how each `agent_message.wait` call ended (its outcome name; never a sender or a body).
pub(crate) type AgentMessageWaitObserver = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Report every settled `agent_message.wait` outcome to `observer`: wraps the registered wait
/// handler (a no-op when none is registered). A refused request (an error) reports nothing.
pub(crate) fn observe_agent_message_waits(
    handlers: &mut crate::kernel::shared::HostRequestHandlers,
    observer: AgentMessageWaitObserver,
) {
    let Some(wait) = handlers.get("agent_message.wait").cloned() else {
        return;
    };
    handlers.register(
        "agent_message.wait",
        crate::kernel::shared::host_handler(move |payload| {
            let wait = std::sync::Arc::clone(&wait);
            let observer = std::sync::Arc::clone(&observer);
            async move {
                let result = wait(payload).await;
                if let Some(outcome) = result
                    .as_ref()
                    .ok()
                    .and_then(|value| value.get("outcome"))
                    .and_then(Value::as_str)
                {
                    observer(outcome);
                }
                result
            }
        }),
    );
}
