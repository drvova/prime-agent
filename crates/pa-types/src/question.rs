//! The kernel `ask(...)` question's wire vocabulary: the open question a surface renders (the
//! `question_open` session event and the attach snapshot's `state.pendingQuestion`), the reply a
//! surface sends back (`answer_question`), and the settled outcome (`question_closed`). The broker
//! that pairs them lives in pa-core.

use serde::{Deserialize, Serialize};

/// The session event an open question rides.
pub const QUESTION_OPEN_EVENT: &str = "question_open";

/// The session event a settled question rides.
pub const QUESTION_CLOSED_EVENT: &str = "question_closed";

/// The client capability an attach declares when it can render and answer questions. A session
/// with no attached client declaring it has no user to ask.
pub const QUESTIONS_CAPABILITY: &str = "user_questions";

/// What kind of answer the question takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// A free-text answer.
    Text,
    /// Exactly one of `choices`.
    Single,
    /// Any subset of `choices`.
    Multi,
}

/// One open question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub text: String,
    pub kind: QuestionKind,
    /// The options for `single` and `multi`; empty for `text`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// The preselected answer: the prefilled text, the preselected choice, or the preselected
    /// choices. Submitted on a timeout and reported when no user can answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default: Vec<String>,
    /// Seconds before the default (or nothing) is submitted; `None` waits for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// A choice question also takes a typed answer (the surface's `Other…` row): one string
    /// that is not a choice (with the chosen ones, for `multi`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_other: bool,
    /// When the timeout settles the question (epoch milliseconds, stamped by the host as it
    /// opens), so every surface counts down to the same moment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// A surface's reply to an open question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QuestionReply {
    /// The user's answer: one text, one choice, or the chosen subset.
    Answered { answer: Vec<String> },
    /// The user dismissed the question (Esc).
    Declined,
}

/// How a question settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Answered,
    Declined,
    /// The timeout elapsed; the default (if any) is the answer.
    TimedOut,
    /// Nobody can answer: no attached interactive client (print, RPC, a detached daemon session,
    /// a subagent nobody watches).
    NoUser,
    /// The asking cell was interrupted, or the session closed.
    Cancelled,
}

impl QuestionStatus {
    /// The wire slug (`"answered"`, `"timed_out"`, ...).
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            QuestionStatus::Answered => "answered",
            QuestionStatus::Declined => "declined",
            QuestionStatus::TimedOut => "timed_out",
            QuestionStatus::NoUser => "no_user",
            QuestionStatus::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_status_tagged() {
        let answered = QuestionReply::Answered {
            answer: vec!["b".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&answered).unwrap(),
            serde_json::json!({"status": "answered", "answer": ["b"]})
        );
        assert_eq!(
            serde_json::from_value::<QuestionReply>(serde_json::json!({"status": "declined"}))
                .unwrap(),
            QuestionReply::Declined
        );
    }
}
