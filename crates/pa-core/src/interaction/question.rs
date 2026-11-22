//! The `question.ask` / `question.cancel` host requests: one open question per session, paired
//! with the surface's reply.
//!
//! `question.ask {id, text, kind, choices?, default?, timeoutSeconds?}` replies
//! `{status, answer}` once the question settles (a choice question also takes the user's own
//! typed answer, `allowOther`; a timeout's deadline rides the open question as `expiresAtMs`):
//! - no surface, or nobody attached who can answer: `no_user` at once, `answer` = the default;
//! - the user replied: `answered` (the answer) or `declined` (`[]`);
//! - `timeoutSeconds` elapsed: `timed_out`, `answer` = the default;
//! - every capable client detached while it waited: `no_user`, `answer` = the default;
//! - `question.cancel {id}` (the kernel cell was interrupted) or the session closed: `cancelled`.
//!
//! The wait parks on a oneshot reply, the deadline, and the presence notification: no polling,
//! no model calls while the user thinks.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use pa_types::question::{Question, QuestionKind, QuestionReply, QuestionStatus};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::{lock, InteractionEvent, InteractionHub};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The most choices one question offers.
const MAX_CHOICES: usize = 20;

/// The longest question text, in characters.
const MAX_QUESTION_CHARS: usize = 2_000;

/// The longest timeout a question may set (one day).
const MAX_TIMEOUT_SECONDS: u64 = 86_400;

/// The open question and the channel its reply settles through.
pub(super) struct OpenQuestion {
    pub(super) question: Question,
    settle: oneshot::Sender<Settled>,
}

/// How the waiting side learns the question settled.
enum Settled {
    Reply(QuestionReply),
    Cancelled,
}

pub(super) fn register(hub: &Arc<InteractionHub>, handlers: &mut HostRequestHandlers) {
    let ask_hub = Arc::clone(hub);
    handlers.register(
        "question.ask",
        host_handler(move |payload| {
            let hub = Arc::clone(&ask_hub);
            async move {
                let question = parse_question(&payload.data)?;
                let (status, answer) = ask(&hub, question).await?;
                Ok(json!({ "status": status.slug(), "answer": answer }))
            }
        }),
    );
    let cancel_hub = Arc::clone(hub);
    handlers.register(
        "question.cancel",
        host_handler(move |payload| {
            let hub = Arc::clone(&cancel_hub);
            async move {
                let id = payload
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("question.cancel requires an id"))?;
                cancel(&hub, Some(id));
                Ok(json!({}))
            }
        }),
    );
}

/// Open `question`, wait for it to settle, and announce the close.
#[tracing::instrument(skip_all, fields(kind = ?question.kind))]
pub(super) async fn ask(
    hub: &InteractionHub,
    mut question: Question,
) -> anyhow::Result<(QuestionStatus, Vec<String>)> {
    let Some(surface) = hub
        .surface
        .clone()
        .filter(|surface| surface.user_available())
    else {
        note(hub, QuestionStatus::NoUser);
        return Ok((QuestionStatus::NoUser, question.default));
    };
    // Every surface counts down to the host's deadline, a reattached one included.
    question.expires_at_ms = question.timeout_seconds.map(|seconds| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(now.as_millis()).unwrap_or(u64::MAX) + seconds * 1000
    });
    let (sender, mut receiver) = oneshot::channel();
    {
        let mut open = lock(&hub.question);
        if let Some(open) = open.as_ref() {
            bail!(
                "another question (\"{}\") is still waiting for the user; let it settle before asking again",
                open.question.id
            );
        }
        *open = Some(OpenQuestion {
            question: question.clone(),
            settle: sender,
        });
    }
    hub.publish(InteractionEvent::QuestionOpened(question.clone()));
    let deadline = question
        .timeout_seconds
        .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
    let (status, answer) = loop {
        // Register for the presence notification before reading availability, so a detach
        // between the read and the wait still wakes this loop.
        let presence = hub.presence.notified();
        tokio::pin!(presence);
        presence.as_mut().enable();
        if !surface.user_available() {
            break (QuestionStatus::NoUser, question.default.clone());
        }
        tokio::select! {
            settled = &mut receiver => break match settled.unwrap_or(Settled::Cancelled) {
                Settled::Reply(QuestionReply::Answered { answer }) => {
                    (QuestionStatus::Answered, answer)
                }
                Settled::Reply(QuestionReply::Declined) => (QuestionStatus::Declined, Vec::new()),
                Settled::Cancelled => (QuestionStatus::Cancelled, Vec::new()),
            },
            // Without a timeout the branch is disabled: the question waits for the user.
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)),
                if deadline.is_some() => {
                break (QuestionStatus::TimedOut, question.default.clone());
            }
            () = &mut presence => {}
        }
    };
    // A timeout or a lost user leaves the question open: withdraw it (a reply settles it by
    // taking it already).
    {
        let mut open = lock(&hub.question);
        if open
            .as_ref()
            .is_some_and(|open| open.question.id == question.id)
        {
            *open = None;
        }
    }
    hub.publish(InteractionEvent::QuestionClosed {
        question_id: question.id,
        status,
    });
    note(hub, status);
    Ok((status, answer))
}

pub(super) fn answer(
    hub: &InteractionHub,
    question_id: &str,
    reply: QuestionReply,
) -> anyhow::Result<()> {
    let mut open = lock(&hub.question);
    let Some(current) = open.as_ref().filter(|open| open.question.id == question_id) else {
        bail!("question \"{question_id}\" is not open");
    };
    if let QuestionReply::Answered { answer } = &reply {
        check_answer(&current.question, answer)?;
    }
    if let Some(current) = open.take() {
        // The asking task may have just settled on its own; the reply then goes nowhere.
        let _ = current.settle.send(Settled::Reply(reply));
    }
    Ok(())
}

/// Settle the open question as cancelled: `id` names the question a kernel withdraws (a
/// different open question stays); `None` cancels whichever is open.
pub(super) fn cancel(hub: &InteractionHub, id: Option<&str>) {
    let mut open = lock(&hub.question);
    let matches = open
        .as_ref()
        .is_some_and(|open| id.is_none_or(|id| open.question.id == id));
    if !matches {
        return;
    }
    if let Some(current) = open.take() {
        let _ = current.settle.send(Settled::Cancelled);
    }
}

fn note(hub: &InteractionHub, status: QuestionStatus) {
    if let Some(counters) = hub.counters() {
        counters.note_question(status);
    }
}

fn check_answer(question: &Question, answer: &[String]) -> anyhow::Result<()> {
    // The user's own typed answer: at most one, never blank, where the question takes one.
    let typed: Vec<&String> = answer
        .iter()
        .filter(|item| !question.choices.contains(item))
        .collect();
    let typed_fits = question.allow_other
        && typed.len() <= 1
        && typed.iter().all(|item| !item.trim().is_empty());
    match question.kind {
        QuestionKind::Text => {
            if answer.len() != 1 {
                bail!("a text answer is exactly one string");
            }
        }
        QuestionKind::Single => {
            if answer.len() != 1 || !(typed.is_empty() || typed_fits) {
                bail!("a single-choice answer is exactly one of the choices or one typed answer");
            }
        }
        QuestionKind::Multi => {
            if let Some(unknown) = typed.first().filter(|_| !typed_fits) {
                bail!("\"{unknown}\" is not one of the choices");
            }
        }
    }
    Ok(())
}

fn parse_question(data: &Value) -> anyhow::Result<Question> {
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("question.ask requires an id"))?;
    let text = data
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow!("ask requires a non-empty question"))?;
    if text.chars().count() > MAX_QUESTION_CHARS {
        bail!("a question is at most {MAX_QUESTION_CHARS} characters");
    }
    let kind = match data.get("kind").and_then(Value::as_str) {
        Some("text") => QuestionKind::Text,
        Some("single") => QuestionKind::Single,
        Some("multi") => QuestionKind::Multi,
        _ => bail!("question kind must be text, single, or multi"),
    };
    let strings = |field: &str| -> anyhow::Result<Vec<String>> {
        match data.get(field) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("ask {field} must be strings"))
                })
                .collect(),
            Some(_) => bail!("ask {field} must be a list of strings"),
        }
    };
    let choices = strings("choices")?;
    match kind {
        QuestionKind::Text if !choices.is_empty() => bail!("a free-text question has no choices"),
        QuestionKind::Single | QuestionKind::Multi if choices.len() < 2 => {
            bail!("a choice question needs at least two choices")
        }
        QuestionKind::Text | QuestionKind::Single | QuestionKind::Multi => {}
    }
    if choices.len() > MAX_CHOICES {
        bail!("a question offers at most {MAX_CHOICES} choices");
    }
    if choices.iter().any(|choice| choice.trim().is_empty()) {
        bail!("ask choices must not be empty");
    }
    let mut unique = choices.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != choices.len() {
        bail!("ask choices must be distinct");
    }
    let question = Question {
        id: id.to_string(),
        text: text.to_string(),
        kind,
        default: strings("default")?,
        choices,
        // A choice question always offers the user's own answer beside the choices.
        allow_other: kind != QuestionKind::Text,
        expires_at_ms: None,
        timeout_seconds: match data.get("timeoutSeconds") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|seconds| (1..=MAX_TIMEOUT_SECONDS).contains(seconds))
                    .ok_or_else(|| {
                        anyhow!("ask timeout must be 1 to {MAX_TIMEOUT_SECONDS} whole seconds")
                    })?,
            ),
        },
    };
    if !question.default.is_empty() {
        check_answer(&question, &question.default)
            .map_err(|error| anyhow!("ask default does not fit the question: {error}"))?;
    }
    Ok(question)
}
