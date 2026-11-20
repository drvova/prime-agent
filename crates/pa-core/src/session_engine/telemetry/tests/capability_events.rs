//! The kernel capabilities' standalone adoption events, byte for byte: every capability counter
//! driven through its seam once with distinct values, then each event's own properties (the
//! session and platform properties every event shares stripped) serialized in emission order.
//! The expected strings were captured from the per-feature counter structs before the generic
//! capability counter replaced them, so this test pins that the move changed no event name, no
//! property, no value, and no property order.

use std::collections::BTreeMap;

use super::*;

/// Drive every capability seam with distinct counts, and the session-end events emitted among
/// them.
fn drive(counters: &SessionCounters) {
    use crate::fs_edit::{EditFact, FsEditOperation};
    use crate::fs_search::{FsOperation, SearchFact};
    use crate::kernel::cell_window::CellWindowSource;
    use crate::kernel::command_cache::CacheLookup;
    use crate::kernel::edit_checks::EditCheckOutcome;
    use crate::kernel::shared::BackgroundCellFact;
    use crate::kernel::spill::host::CtxRead;
    use crate::kernel::spill::SpillFact;
    use crate::kernel::test_runs::TestRunFact;
    use crate::session_engine::checkpoint_host::CheckpointFact;
    use crate::session_engine::rlm_host::RlmAbortOutcome;
    use crate::session_engine::runtime::ScheduleFact;
    use crate::session_engine::session_search::SessionRecallFact;
    use crate::web_read::{ReadKind, ReadOutcome};
    use pa_types::billing::{BillingMode, CostSource};
    use pa_types::todo::{TodoItem, TodoList, TodoStatus};

    let item = |id: &str, status| TodoItem {
        id: id.to_string(),
        text: format!("item {id}"),
        status,
        note: None,
    };
    let open = TodoList {
        items: vec![
            item("1", TodoStatus::Pending),
            item("2", TodoStatus::Pending),
        ],
    };
    let done = TodoList {
        items: vec![item("1", TodoStatus::Done), item("2", TodoStatus::Done)],
    };
    counters.note_todo_change(&TodoList::default(), &open);
    counters.note_todo_change(&open, &done);
    for status in [
        QuestionStatus::Answered,
        QuestionStatus::Answered,
        QuestionStatus::Declined,
        QuestionStatus::TimedOut,
        QuestionStatus::NoUser,
        QuestionStatus::Cancelled,
    ] {
        counters.note_question(status);
    }
    for (outcome, ms) in [
        (
            ReadOutcome::Read {
                kind: ReadKind::Site,
                truncated: true,
            },
            40,
        ),
        (
            ReadOutcome::Read {
                kind: ReadKind::Pdf,
                truncated: false,
            },
            70,
        ),
        (
            ReadOutcome::Read {
                kind: ReadKind::Page,
                truncated: false,
            },
            10,
        ),
        (
            ReadOutcome::Read {
                kind: ReadKind::Page,
                truncated: false,
            },
            20,
        ),
        (ReadOutcome::Failed, 5),
    ] {
        counters.note_web_read(outcome, Duration::from_millis(ms));
    }
    for (operation, results, truncated, ms) in [
        (FsOperation::Grep, Some(7), true, 30),
        (FsOperation::Grep, Some(2), false, 12),
        (FsOperation::Glob, Some(11), false, 9),
        (FsOperation::Find, None, false, 55),
    ] {
        counters.note_fs_search(SearchFact {
            operation,
            results,
            truncated,
            elapsed: Duration::from_millis(ms),
        });
    }
    for fact in [
        EditFact::Read { lines: 120 },
        EditFact::Read { lines: 3 },
        EditFact::Applied { edits: 4 },
        EditFact::Rejected { stale: 2 },
        EditFact::Failed {
            operation: FsEditOperation::Read,
        },
        EditFact::Failed {
            operation: FsEditOperation::Edit,
        },
        EditFact::Failed {
            operation: FsEditOperation::Edit,
        },
    ] {
        counters.note_fs_edit(fact);
    }
    for fact in [
        ScheduleFact::AddedOnce,
        ScheduleFact::AddedRecurring,
        ScheduleFact::AddedRecurring,
        ScheduleFact::Listed,
        ScheduleFact::Cancelled,
        ScheduleFact::Rejected,
        ScheduleFact::Rejected,
        ScheduleFact::Rejected,
    ] {
        counters.note_repl_schedule(fact);
    }
    for fact in [
        CheckpointFact::Created,
        CheckpointFact::Created,
        CheckpointFact::Listed,
        CheckpointFact::Restored,
        CheckpointFact::RestoreDeclined,
        CheckpointFact::RestoreDisabled,
        CheckpointFact::RestoreDisabled,
        CheckpointFact::Failed,
    ] {
        counters.note_repl_checkpoint(fact);
    }
    for _ in 0..3 {
        counters.note_usage_read();
    }
    for outcome in [
        Some(RlmAbortOutcome::Aborted),
        Some(RlmAbortOutcome::Idle),
        Some(RlmAbortOutcome::Idle),
        None,
    ] {
        counters.note_rlm_abort(outcome);
    }
    counters.note_repl_reload(2);
    counters.note_repl_reload(3);
    counters.note_repl_settings_read();
    for outcome in [Some(true), Some(false), Some(false), None] {
        counters.note_kernel_mcp_login(outcome);
    }
    for fact in [
        SessionRecallFact::Searched { hits: 4 },
        SessionRecallFact::Searched { hits: 1 },
        SessionRecallFact::Read,
        SessionRecallFact::Failed,
    ] {
        counters.note_session_recall(fact);
    }
    counters.note_notification(true);
    counters.note_notification(false);
    counters.note_notification(true);
    counters.note_effort_switch(true);
    counters.note_effort_switch(false);
    counters.note_effort_switch(false);
    for fact in [
        BackgroundCellFact::Backgrounded,
        BackgroundCellFact::Backgrounded,
        BackgroundCellFact::EndedOk { ran_for: None },
        BackgroundCellFact::EndedWithError { ran_for: None },
        BackgroundCellFact::Interrupted,
    ] {
        counters.note_background_cell(fact);
    }
    for source in [
        CellWindowSource::Learned,
        CellWindowSource::ModelTimeout,
        CellWindowSource::ModelTimeout,
        CellWindowSource::Setting,
        CellWindowSource::ColdDefault,
    ] {
        counters.note_cell_window(source);
    }
    for fact in [
        BetterTelemetryFact::Run,
        BetterTelemetryFact::Run,
        BetterTelemetryFact::Run,
        BetterTelemetryFact::Completed,
        BetterTelemetryFact::Cancelled,
        BetterTelemetryFact::Failed,
        BetterTelemetryFact::SentAsIs,
        BetterTelemetryFact::Edited,
        BetterTelemetryFact::Edited,
        BetterTelemetryFact::Discarded,
        BetterTelemetryFact::BriefRun,
        BetterTelemetryFact::BriefRun,
        BetterTelemetryFact::BriefUnchanged,
        BetterTelemetryFact::VerificationPass,
    ] {
        counters.note_better(fact);
    }
    for fact in [
        SpillFact::Queued { bytes: 2 << 20 },
        SpillFact::Queued { bytes: 1_000 },
        SpillFact::NotStored,
        SpillFact::Evicted,
        SpillFact::Evicted,
    ] {
        counters.note_spill(fact);
    }
    for read in [
        CtxRead::Page,
        CtxRead::Page,
        CtxRead::Grep,
        CtxRead::Tail,
        CtxRead::Info,
        CtxRead::Info,
        CtxRead::Info,
    ] {
        counters.note_ctx_read(read);
    }
    for (failed, failures, stored) in [(0, 0, false), (3, 2, true), (1, 1, false)] {
        counters.note_test_summary(TestRunFact {
            failed,
            failures,
            stored,
        });
    }
    for (file_read, hit) in [
        (false, true),
        (false, true),
        (false, false),
        (true, true),
        (true, false),
        (true, false),
        (true, false),
    ] {
        counters.note_cache_lookup(CacheLookup { file_read, hit });
    }
    for outcome in [
        EditCheckOutcome::Clean,
        EditCheckOutcome::Clean,
        EditCheckOutcome::Found,
        EditCheckOutcome::Late,
        EditCheckOutcome::Skipped,
        EditCheckOutcome::Skipped,
        EditCheckOutcome::Failed,
    ] {
        counters.note_edit_check(outcome);
    }
    // The non-capability events that main emitted between the capability events: they keep
    // their own counter structs, and the move must leave their bytes alone too.
    for mode in [
        BillingMode::PayAsYouGo {
            source: CostSource::Catalog,
        },
        BillingMode::Plan {
            provider_quota: true,
        },
        BillingMode::Plan {
            provider_quota: false,
        },
        BillingMode::FlatRate,
        BillingMode::Free,
        BillingMode::Free,
        BillingMode::Free,
        BillingMode::Unknown,
    ] {
        counters.note_billing_mode(mode);
    }
    counters.note_agent_message_refusals(
        crate::session_engine::agent_messaging::AgentMessageRefusals {
            throttled: 3,
            wake_refused: 1,
        },
    );
    counters.note_agent_message_refusals(
        crate::session_engine::agent_messaging::AgentMessageRefusals {
            throttled: 0,
            wake_refused: 4,
        },
    );
    for outcome in [
        "message",
        "message",
        "timeout",
        "cycle",
        "no_sender",
        "no_sender",
    ] {
        counters.note_agent_message_wait(outcome);
    }
}

/// Each capability event's own properties, serialized in emission order: the properties every
/// emitted event shares with the same value (the session and platform base) are stripped.
fn capability_payloads(mock: &MockSink) -> BTreeMap<String, String> {
    let events: Vec<(String, serde_json::Map<String, serde_json::Value>)> = mock
        .events()
        .iter()
        .filter(|event| event.name != "agent session ended")
        .map(|event| {
            (
                event.name.clone(),
                serde_json::to_value(&event.properties)
                    .expect("properties serialize")
                    .as_object()
                    .expect("an object")
                    .clone(),
            )
        })
        .collect();
    let shared: Vec<String> = events[0]
        .1
        .iter()
        .filter(|(key, value)| {
            events
                .iter()
                .all(|(_, properties)| properties.get(*key) == Some(*value))
        })
        .map(|(key, _)| key.clone())
        .collect();
    events
        .into_iter()
        .map(|(name, mut properties)| {
            for key in &shared {
                properties.shift_remove(key);
            }
            (name, serde_json::to_string(&properties).expect("serialize"))
        })
        .collect()
}

#[tokio::test]
async fn capability_events_stay_byte_identical() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    drive(&counters);
    telemetry.end().await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let actual = capability_payloads(&fixture.mock);
    let expected: BTreeMap<String, String> = EXPECTED
        .iter()
        .map(|(name, payload)| ((*name).to_string(), (*payload).to_string()))
        .collect();
    assert_eq!(actual, expected);
}

/// Captured from the per-feature counter structs (the pre-refactor emission: this test run
/// against origin/main's counters).
const EXPECTED: &[(&str, &str)] = &[
    (
        "agent better prompt",
        r#"{"better_run_count":3,"better_completed_count":1,"better_cancelled_count":1,"better_failed_count":1,"better_sent_as_is_count":1,"better_edited_count":2,"better_discarded_count":1,"better_brief_run_count":2,"better_brief_unchanged_count":1,"better_verification_pass_count":1}"#,
    ),
    (
        "agent billing modes",
        r#"{"billing_pay_as_you_go_count":1,"billing_plan_count":2,"billing_flat_rate_count":1,"billing_free_count":3,"billing_unknown_count":1}"#,
    ),
    (
        "agent context paging",
        r#"{"spill_count":2,"spill_not_stored_count":1,"spill_evicted_count":2,"ctx_page_count":2,"ctx_grep_count":1,"ctx_tail_count":1,"ctx_info_count":3,"spill_bytes_bucket":"lt_16mb"}"#,
    ),
    (
        "agent edit checks",
        r#"{"edit_check_run_count":4,"edit_check_found_count":1,"edit_check_late_count":1,"edit_check_skipped_count":2,"edit_check_failed_count":1}"#,
    ),
    (
        "agent fs edit",
        r#"{"fs_read_count":2,"fs_read_line_count":123,"fs_read_failed_count":1,"fs_edit_count":1,"fs_edit_op_count":4,"fs_edit_rejected_count":1,"fs_edit_stale_anchor_count":2,"fs_edit_failed_count":2}"#,
    ),
    (
        "agent fs search",
        r#"{"fs_grep_count":2,"fs_glob_count":1,"fs_find_count":1,"fs_search_failed_count":1,"fs_search_truncated_count":1,"fs_search_result_count":20,"fs_search_max_ms":55}"#,
    ),
    (
        "agent kernel login and recall",
        r#"{"kernel_mcp_login_count":1,"kernel_mcp_login_failed_count":2,"kernel_mcp_login_unavailable_count":1,"session_search_count":2,"session_search_hit_count":5,"session_search_read_count":1,"session_search_failed_count":1}"#,
    ),
    (
        "agent message waits",
        r#"{"agent_message_wait_message_count":2,"agent_message_wait_timeout_count":1,"agent_message_wait_cycle_count":1,"agent_message_wait_no_sender_count":2}"#,
    ),
    (
        "agent messages",
        r#"{"agent_message_throttled_count":3,"agent_message_wake_refused_count":5}"#,
    ),
    (
        "agent readonly cache",
        r#"{"readonly_cache_hit_count":2,"readonly_cache_miss_count":1,"readonly_cache_fs_read_hit_count":1,"readonly_cache_fs_read_miss_count":3}"#,
    ),
    (
        "agent repl background cells",
        r#"{"cell_backgrounded_count":2,"cell_background_ok_count":1,"cell_background_error_count":1,"cell_background_interrupted_count":1,"cell_window_learned_count":1,"cell_window_timeout_count":2,"cell_window_setting_count":1,"cell_window_default_count":1}"#,
    ),
    (
        "agent repl checkpoint",
        r#"{"checkpoint_created_count":2,"checkpoint_list_count":1,"checkpoint_restored_count":1,"checkpoint_restore_declined_count":1,"checkpoint_restore_disabled_count":2,"checkpoint_failed_count":1}"#,
    ),
    (
        "agent repl notify",
        r#"{"notify_count":3,"notify_undelivered_count":1,"effort_set_count":1,"effort_refused_count":2}"#,
    ),
    (
        "agent repl reload",
        r#"{"repl_reload_count":2,"repl_reload_skills_added_count":5,"repl_settings_get_count":1}"#,
    ),
    (
        "agent repl schedule",
        r#"{"schedule_once_added_count":1,"schedule_recurring_added_count":2,"schedule_list_count":1,"schedule_cancelled_count":1,"schedule_rejected_count":3}"#,
    ),
    (
        "agent rlm child abort",
        r#"{"rlm_abort_aborted_count":1,"rlm_abort_idle_count":2,"rlm_abort_failed_count":1}"#,
    ),
    (
        "agent todos and questions",
        r#"{"todo_list_set_count":1,"todo_item_done_count":2,"question_asked_count":6,"question_answered_count":2,"question_declined_count":1,"question_timed_out_count":1,"question_no_user_count":1,"question_cancelled_count":1}"#,
    ),
    (
        "agent test summaries",
        r#"{"test_summary_count":3,"test_summary_failing_count":2,"test_summary_failed_test_count":4,"test_summary_failure_handle_count":3,"test_summary_stored_count":1}"#,
    ),
    ("agent usage reads", r#"{"usage_read_count":3}"#),
    (
        "agent web reads",
        r#"{"web_read_count":5,"web_read_failed_count":1,"web_read_site_count":1,"web_read_pdf_count":1,"web_read_page_count":2,"web_read_truncated_count":1,"web_read_max_ms":70}"#,
    ),
];
