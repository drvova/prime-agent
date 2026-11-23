//! The kernel capabilities' standalone adoption events, byte for byte: every capability counter
//! driven through its seam with distinct values, then each event's own properties (the platform
//! base and the session id stripped) serialized in emission order. The expected strings were
//! captured from the per-feature counter structs before the generic capability counter replaced
//! them, so this test pins that the move changed no event name, no property, no value, and no
//! property order. The events emitted among them that no capability manifest claims (goal
//! verification, the transports, billing, messages, error enrichment, the daemon's
//! `session_crashed` event) are pinned the same way: they keep their own emission.

use std::collections::BTreeMap;

use super::*;

/// Drive every capability seam with distinct counts, and the session-end events emitted among
/// them.
fn drive(counters: &SessionCounters) {
    use crate::fs_edit::{EditFact, FsEditOperation, RereadFact};
    use crate::fs_search::{FsOperation, SearchFact};
    use crate::kernel::cell_window::CellWindowSource;
    use crate::kernel::command_cache::CacheLookup;
    use crate::kernel::edit_checks::EditCheckOutcome;
    use crate::kernel::error_context::EnrichmentFact;
    use crate::kernel::hypotheses::HypothesesRun;
    use crate::kernel::shared::BackgroundCellFact;
    use crate::kernel::shell_runs::ShellRun;
    use crate::kernel::spill::host::CtxRead;
    use crate::kernel::spill::{PreviewFact, SpillFact};
    use crate::kernel::test_runs::TestRunFact;
    use crate::prefetch::PrefetchFact;
    use crate::session_engine::checkpoint_host::CheckpointFact;
    use crate::session_engine::decision::DecideFact;
    use crate::session_engine::rizin_usage::RizinFact;
    use crate::session_engine::rlm_host::RlmAbortOutcome;
    use crate::session_engine::runtime::ScheduleFact;
    use crate::session_engine::session_search::SessionRecallFact;
    use crate::symbol_index::SymbolFact;
    use crate::web_read::{ReadKind, ReadOutcome};
    use crate::worktree_host::WorktreeFact;
    use pa_types::billing::{BillingMode, CostSource};
    use pa_types::control::DepthChange;
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
        EditFact::Written { created: true },
        EditFact::Written { created: false },
        EditFact::Written { created: false },
        EditFact::Failed {
            operation: FsEditOperation::Write,
        },
    ] {
        counters.note_fs_edit(fact);
    }
    for fact in [
        RereadFact::Unchanged,
        RereadFact::Unchanged,
        RereadFact::Unchanged,
        RereadFact::Diff,
        RereadFact::Full,
        RereadFact::Full,
    ] {
        counters.note_reread(fact);
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
        BetterTelemetryFact::CheckRound,
        BetterTelemetryFact::CheckRound,
        BetterTelemetryFact::CheckPassed,
        BetterTelemetryFact::CheckPassed,
        BetterTelemetryFact::CheckPassed,
        BetterTelemetryFact::CheckFailed,
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
    for (in_process, spawned, handed_over, unavailable) in [(4, 1, 1, 0), (2, 2, 0, 3)] {
        counters.note_shell_run(ShellRun {
            in_process,
            spawned,
            handed_over,
            unavailable,
        });
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
    for fact in [
        DecideFact::Decided,
        DecideFact::Decided,
        DecideFact::Limited,
        DecideFact::Failed,
        DecideFact::Failed,
        DecideFact::Failed,
    ] {
        counters.note_decision(fact);
    }
    for fact in [
        WorktreeFact::Created,
        WorktreeFact::Created,
        WorktreeFact::Created,
        WorktreeFact::Diffed,
        WorktreeFact::Merged,
        WorktreeFact::Branched,
        WorktreeFact::Branched,
        WorktreeFact::Conflicted,
        WorktreeFact::Removed,
        WorktreeFact::Failed,
        WorktreeFact::Failed,
        WorktreeFact::Failed,
        WorktreeFact::Failed,
    ] {
        counters.note_worktree(fact);
    }
    for fact in [
        RizinFact::Open,
        RizinFact::Open,
        RizinFact::DebugOpen,
        RizinFact::Command,
        RizinFact::Command,
        RizinFact::Command,
        RizinFact::JsonCommand,
        RizinFact::JsonCommand,
        RizinFact::Interrupted,
        RizinFact::Died,
        RizinFact::NotInstalled,
        RizinFact::NotInstalled,
    ] {
        counters.note_rizin(fact);
    }
    for (definitions, references, partial) in [(2, 5, true), (1, 0, false), (0, 7, false)] {
        counters.note_symbol_query(SymbolFact {
            definitions,
            references,
            partial,
        });
    }
    for change in [
        DepthChange::Raised,
        DepthChange::Raised,
        DepthChange::Lowered,
        DepthChange::Unchanged,
    ] {
        counters.note_rlm_depth(change);
    }
    for _ in 0..3 {
        counters.note_scaled_brief();
    }
    for (approaches, passed, winner) in [(3, 2, true), (2, 0, false), (4, 1, true)] {
        counters.note_hypotheses(HypothesesRun {
            approaches,
            passed,
            winner,
        });
    }
    for (issued, used, unused, skipped) in [(3, 0, 0, false), (0, 1, 2, true), (2, 0, 0, false)] {
        counters.note_prefetch(PrefetchFact {
            issued,
            used,
            unused,
            skipped,
        });
    }
    // A `ctx-*` read before the first preview is not a follow-up; the two after it are.
    counters.note_preview_followup();
    for (kept_lines, dropped_lines, repeats_collapsed, noise_lines, patterns_learned) in
        [(10, 1, 1, 11, 2), (7, 30, 4, 0, 0)]
    {
        counters.note_preview(PreviewFact {
            kept_lines,
            dropped_lines,
            repeats_collapsed,
            noise_lines,
            patterns_learned,
        });
    }
    counters.note_preview_followup();
    counters.note_preview_followup();
    // The events no manifest claims, emitted among the capability events: they keep their own
    // counter structs, and the move must leave their bytes alone too.
    for fact in [
        GoalTelemetryFact::CheckRun,
        GoalTelemetryFact::CheckRun,
        GoalTelemetryFact::CheckRun,
        GoalTelemetryFact::CheckFailed,
        GoalTelemetryFact::CheckSkipped,
        GoalTelemetryFact::JudgeCall,
        GoalTelemetryFact::JudgeCall,
        GoalTelemetryFact::JudgeNotMet,
        GoalTelemetryFact::TokenBudgetStop,
        GoalTelemetryFact::CostBudgetStop,
    ] {
        counters.note_goal(fact);
    }
    counters.note_responses_transport(pa_ai::responses_transport::ResponsesTransportCounts {
        websocket_requests: 4,
        delta_requests: 3,
        full_context_requests: 1,
        sse_fallbacks: 2,
        ..Default::default()
    });
    counters.note_endpoint_discovery(pa_ai::responses_transport::DiscoveryCounts {
        probes: 5,
        found_responses: 2,
        found_websocket: 1,
        upgrades: 1,
        demotions: 3,
    });
    for (failed_commands, locations, skipped, timed_out) in
        [(1, 3, 0, false), (0, 0, 2, true), (2, 1, 1, false)]
    {
        counters.note_error_enrichment(EnrichmentFact {
            failed_commands,
            locations,
            skipped,
            timed_out,
        });
    }
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

/// Each emitted event's own properties, serialized in emission order: the platform base every
/// event carries and the session id are stripped. `agent session ended` is left out (its run
/// totals are not this test's subject).
fn event_payloads(mock: &MockSink) -> BTreeMap<String, String> {
    let base = pa_telemetry::base_properties("interactive");
    mock.events()
        .iter()
        .filter(|event| event.name != "agent session ended")
        .map(|event| {
            let mut properties = serde_json::to_value(&event.properties)
                .expect("properties serialize")
                .as_object()
                .expect("an object")
                .clone();
            for (key, _) in base.iter() {
                properties.shift_remove(key);
            }
            properties.shift_remove("session_id");
            (
                event.name.clone(),
                serde_json::to_string(&properties).expect("serialize"),
            )
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
    // The daemon's own event for a contained session crash: no session emits it.
    track_session_crashed(&fixture.client, "panic");
    fixture.client.flush().await.unwrap();
    let actual = event_payloads(&fixture.mock);
    let expected: BTreeMap<String, String> = EXPECTED
        .iter()
        .map(|(name, payload)| ((*name).to_string(), (*payload).to_string()))
        .collect();
    assert_eq!(actual, expected);
}

/// Captured from the per-feature counter structs (the pre-refactor emission: this test run
/// against the Scheme-kernel branch's counters, before the generic counter replaced them).
const EXPECTED: &[(&str, &str)] = &[
    (
        "agent builtin shell",
        r#"{"builtin_shell_command_count":2,"builtin_shell_in_process_count":6,"builtin_shell_spawned_count":3,"builtin_shell_handed_over_count":1,"builtin_shell_unavailable_count":3}"#,
    ),
    (
        "agent better prompt",
        r#"{"better_run_count":3,"better_completed_count":1,"better_cancelled_count":1,"better_failed_count":1,"better_sent_as_is_count":1,"better_edited_count":2,"better_discarded_count":1,"better_brief_run_count":2,"better_brief_unchanged_count":1,"better_verification_pass_count":1,"better_check_round_count":2,"better_check_passed_count":3,"better_check_failed_count":1}"#,
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
        "agent endpoint discovery",
        r#"{"endpoint_probe_count":5,"endpoint_found_responses_count":2,"endpoint_found_websocket_count":1,"endpoint_upgrade_count":1,"endpoint_demotion_count":3}"#,
    ),
    (
        "agent error enrichment",
        r#"{"error_enrichment_count":2,"error_location_count":4,"error_location_skipped_count":3,"error_enrichment_timeout_count":1,"error_failed_command_count":3}"#,
    ),
    (
        "agent fs edit",
        r#"{"fs_read_count":2,"fs_read_line_count":123,"fs_read_failed_count":1,"fs_edit_count":1,"fs_edit_op_count":4,"fs_edit_rejected_count":1,"fs_edit_stale_anchor_count":2,"fs_edit_failed_count":2,"fs_write_count":3,"fs_write_created_count":1,"fs_write_failed_count":1}"#,
    ),
    (
        "agent fs rereads",
        r#"{"fs_reread_unchanged_count":3,"fs_reread_diff_count":1,"fs_reread_full_count":2}"#,
    ),
    (
        "agent fs search",
        r#"{"fs_grep_count":2,"fs_glob_count":1,"fs_find_count":1,"fs_search_failed_count":1,"fs_search_truncated_count":1,"fs_search_result_count":20,"fs_search_max_ms":55}"#,
    ),
    (
        "agent goal verification",
        r#"{"goal_check_run_count":3,"goal_check_failed_count":1,"goal_check_skipped_count":1,"goal_judge_call_count":2,"goal_judge_not_met_count":1,"goal_token_budget_stop_count":1,"goal_cost_budget_stop_count":1}"#,
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
        "agent prefetch",
        r#"{"prefetch_read_count":3,"prefetch_issued_count":5,"prefetch_used_count":1,"prefetch_unused_count":2,"prefetch_skipped_count":1}"#,
    ),
    (
        "agent previews",
        r#"{"preview_count":2,"preview_kept_line_count":17,"preview_dropped_line_count":31,"preview_repeat_collapsed_count":5,"preview_noise_line_count":11,"preview_noise_pattern_learned_count":2,"preview_followup_read_count":2}"#,
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
        "agent repl decide",
        r#"{"decide_count":2,"decide_limited_count":1,"decide_failed_count":3}"#,
    ),
    (
        "agent repl hypotheses",
        r#"{"hypotheses_run_count":3,"hypotheses_approach_count":9,"hypotheses_passed_count":3,"hypotheses_no_winner_count":1}"#,
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
        "agent repl rizin",
        r#"{"rizin_open_count":2,"rizin_debug_open_count":1,"rizin_command_count":3,"rizin_json_command_count":2,"rizin_interrupted_count":1,"rizin_died_count":1,"rizin_not_installed_count":2}"#,
    ),
    (
        "agent repl schedule",
        r#"{"schedule_once_added_count":1,"schedule_recurring_added_count":2,"schedule_list_count":1,"schedule_cancelled_count":1,"schedule_rejected_count":3}"#,
    ),
    (
        "agent responses transport",
        r#"{"responses_websocket_request_count":4,"responses_delta_request_count":3,"responses_full_context_request_count":1,"responses_sse_fallback_count":2,"responses_stale_continuation_retry_count":0,"responses_auto_websocket_count":0,"responses_auto_sse_one_shot_count":0,"responses_auto_sse_no_websocket_wire_count":0,"responses_auto_sse_unsupported_count":0,"responses_auto_sse_demoted_count":0,"responses_probe_count":0,"responses_probe_failed_count":0,"responses_websocket_without_wire_count":0}"#,
    ),
    (
        "agent rlm child abort",
        r#"{"rlm_abort_aborted_count":1,"rlm_abort_idle_count":2,"rlm_abort_failed_count":1}"#,
    ),
    (
        "agent rlm delegation depth",
        r#"{"rlm_depth_raised_count":2,"rlm_depth_lowered_count":1,"rlm_depth_scaled_brief_count":3}"#,
    ),
    (
        "agent symbol index",
        r#"{"symbol_query_count":3,"symbol_query_partial_count":1,"symbol_definition_count":3,"symbol_reference_count":12}"#,
    ),
    (
        "agent test summaries",
        r#"{"test_summary_count":3,"test_summary_failing_count":2,"test_summary_failed_test_count":4,"test_summary_failure_handle_count":3,"test_summary_stored_count":1}"#,
    ),
    (
        "agent todos and questions",
        r#"{"todo_list_set_count":1,"todo_item_done_count":2,"question_asked_count":6,"question_answered_count":2,"question_declined_count":1,"question_timed_out_count":1,"question_no_user_count":1,"question_cancelled_count":1}"#,
    ),
    ("agent usage reads", r#"{"usage_read_count":3}"#),
    (
        "agent web reads",
        r#"{"web_read_count":5,"web_read_failed_count":1,"web_read_site_count":1,"web_read_pdf_count":1,"web_read_page_count":2,"web_read_truncated_count":1,"web_read_max_ms":70}"#,
    ),
    (
        "agent worktrees",
        r#"{"worktree_created_count":3,"worktree_diff_count":1,"worktree_merged_count":1,"worktree_branched_count":2,"worktree_conflict_count":1,"worktree_removed_count":1,"worktree_failed_count":4}"#,
    ),
    (
        "daemon event",
        r#"{"kind":"session_crashed","source":"panic","count":1}"#,
    ),
];
