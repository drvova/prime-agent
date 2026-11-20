//! One event shared by two capabilities: `todo` owns its `todo_*` counts, `ask` its
//! `question_*` counts. It is emitted before `agent session ended`, the others after it.

use super::super::{count, required, uuid, EventRule};

/// The kernel `todo.*` and `ask(...)` adoption counters for one session (v2), emitted at session
/// close beside `agent session ended` (which sits at the 128-key cap) when the session used
/// either. Counts only: never item text, a question, a choice, or an answer.
pub(in crate::catalog) const AGENT_TODOS_AND_QUESTIONS: EventRule = EventRule {
    name: "agent todos and questions",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("todo_list_set_count", required(count())),
        ("todo_item_done_count", required(count())),
        ("question_asked_count", required(count())),
        ("question_answered_count", required(count())),
        ("question_declined_count", required(count())),
        ("question_timed_out_count", required(count())),
        ("question_no_user_count", required(count())),
        ("question_cancelled_count", required(count())),
    ],
};
