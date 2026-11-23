//! The interaction-telemetry concern: the pa-tui interactive loop reports
//! through the `InteractionTelemetry` trait. Interactions only count into
//! the session run's counters, which ride its one `tui exit` event. The
//! standalone events remain their own (TS parity, plus upstream additions):
//! `agent command used`, `tui ipython bash rendered` — the
//! settled-shell-cell metric upstream #3307 tracks per render, kept intact
//! from main.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{Future, PathBuf, Pin};

/// The interactive client's telemetry: per-session-run adoption counters
/// flushed with `tui exit`, plus the standalone events — the TS
/// `agent command used`, the upstream #3307 `tui ipython bash rendered` — each on a one-shot client. Telemetry must
/// never fail the session: opt-out or a broken install id drops the events.
pub(super) struct CliInteractionTelemetry {
    pub(super) cwd: PathBuf,
    pub(super) agent_dir: PathBuf,
    pub(super) counters: Mutex<TuiCounters>,
}

/// The session run's adoption counters (`tui_*_count`, `feature_*_count`,
/// `input_*`) and its terminal capability flags.
#[derive(Default)]
pub(super) struct TuiCounters {
    counts: BTreeMap<String, u64>,
    flags: BTreeMap<&'static str, bool>,
}

impl CliInteractionTelemetry {
    pub(super) fn new(cwd: PathBuf, agent_dir: PathBuf) -> Self {
        Self {
            cwd,
            agent_dir,
            counters: Mutex::default(),
        }
    }

    fn settings(&self) -> pa_core::settings::SettingsManager {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
    }

    /// A one-shot client, or `None` when telemetry is opted out.
    fn client(&self) -> Option<pa_telemetry::TelemetryClient> {
        let settings = self.settings();
        if crate::mode::telemetry_disabled(&settings) {
            return None;
        }
        Some(pa_core::session_engine::telemetry::build_client(
            &settings,
            &self.agent_dir,
        ))
    }

    /// Count only while telemetry is on, so turning it on later never
    /// sends what happened while it was off.
    fn with(&self, update: impl FnOnce(&mut TuiCounters)) {
        if crate::mode::telemetry_disabled(&self.settings()) {
            return;
        }
        update(
            &mut self
                .counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    fn count(&self, key: &'static str) {
        self.with(|counters| *counters.counts.entry(key.to_string()).or_default() += 1);
    }
}

impl pa_tui::interactive::InteractionTelemetry for CliInteractionTelemetry {
    fn feature_outcome(
        &self,
        feature: &'static str,
        outcome: &'static str,
        _duration_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        if let Some(key) = pa_telemetry::feature_outcome_key(feature, outcome) {
            self.with(|counters| *counters.counts.entry(key).or_default() += 1);
        }
        Box::pin(std::future::ready(()))
    }

    fn input_stage(
        &self,
        _input_id: String,
        stage: &'static str,
        _outcome: &'static str,
        duration_ms: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        if let Some(prefix) = pa_telemetry::input_stage_key(stage) {
            self.with(|counters| {
                *counters
                    .counts
                    .entry(format!("{prefix}_count"))
                    .or_default() += 1;
                let max = counters
                    .counts
                    .entry(format!("{prefix}_max_ms"))
                    .or_default();
                *max = (*max).max(duration_ms);
            });
        }
        Box::pin(std::future::ready(()))
    }

    fn bash_shortcut_used(
        &self,
        _excluded: bool,
        _side_conversation: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_bash_shortcut_count");
        Box::pin(std::future::ready(()))
    }

    fn bash_bang_executed(
        &self,
        _duration_bucket: &'static str,
        _exit_class: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_bash_bang_count");
        Box::pin(std::future::ready(()))
    }

    fn prompt_stash(
        &self,
        _action: &'static str,
        _had_images: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_prompt_stash_count");
        Box::pin(std::future::ready(()))
    }

    fn external_editor_used(
        &self,
        _outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_external_editor_count");
        Box::pin(std::future::ready(()))
    }

    fn scoped_models_used(
        &self,
        _action: &'static str,
        _scoped: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_scoped_models_count");
        Box::pin(std::future::ready(()))
    }

    fn ipython_bash_rendered(
        &self,
        bash_lines: usize,
        cell_lines: usize,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("bash_lines", serde_json::Value::from(bash_lines));
            properties.set("cell_lines", serde_json::Value::from(cell_lines));
            properties.set("count", serde_json::Value::from(count));
            client.track("tui ipython bash rendered", properties);
            let _ = client.shutdown().await;
        })
    }

    fn live_tool_timer_started(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_live_tool_timer_count");
        Box::pin(std::future::ready(()))
    }

    fn turn_feedback_shown(
        &self,
        feedback: pa_tui::interactive::TurnFeedback,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count(match feedback {
            pa_tui::interactive::TurnFeedback::StallShown => "tui_stall_shown_count",
        });
        Box::pin(std::future::ready(()))
    }

    fn menu_used(
        &self,
        used: pa_tui::interactive::MenuUse,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count(match used {
            pa_tui::interactive::MenuUse::SlashCommandAccepted => "tui_slash_accept_count",
            pa_tui::interactive::MenuUse::SearchStarted => "tui_menu_search_count",
        });
        Box::pin(std::future::ready(()))
    }

    fn mention_pill_sent(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_mention_pill_count");
        Box::pin(std::future::ready(()))
    }

    fn draft_recalled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_draft_recall_count");
        Box::pin(std::future::ready(()))
    }

    fn scroll_used(
        &self,
        _action: &'static str,
        _resumed_following: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_scroll_count");
        Box::pin(std::future::ready(()))
    }

    fn transcript_navigated(
        &self,
        action: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui crash notice`: `tui exit` sits at the backend's 128-key cap.
        // Delivery is the client worker's: the key path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("action", serde_json::Value::from(action));
            client.track("tui transcript navigation", properties);
            let _ = client.shutdown().await;
        })
    }

    fn transcript_searched(
        &self,
        search: pa_tui::interactive::TranscriptSearchReport,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui transcript navigation`: `tui exit` sits at the backend's 128-key
        // cap. Delivery is the client worker's: the key path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("outcome", serde_json::Value::from(search.outcome));
            properties.set("steps", serde_json::Value::from(search.steps));
            properties.set("revealed", serde_json::Value::from(search.revealed));
            properties.set("history", serde_json::Value::from(search.history));
            client.track("tui transcript search", properties);
            let _ = client.shutdown().await;
        })
    }

    fn unfocused_signal_sent(
        &self,
        reason: &'static str,
        mode: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone for the same reason; the event path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("reason", serde_json::Value::from(reason));
            properties.set("mode", serde_json::Value::from(mode));
            client.track("tui unfocused signal", properties);
            let _ = client.shutdown().await;
        })
    }

    fn selection_used(&self, _lines: usize) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_selection_count");
        Box::pin(std::future::ready(()))
    }

    fn click_used(&self, _surface: &'static str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_click_count");
        Box::pin(std::future::ready(()))
    }

    fn activity_opened(
        &self,
        _kind: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_activity_open_count");
        Box::pin(std::future::ready(()))
    }

    fn crash_notice_shown(
        &self,
        kind: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui image paste`: `tui exit` sits at the backend's 128-key cap.
        // Delivery is the client worker's: the notice's paint spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("kind", serde_json::Value::from(kind));
            client.track("tui crash notice", properties);
            let _ = client.shutdown().await;
        })
    }

    fn menu_opened(
        &self,
        _menu: &'static str,
        _source: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_menu_open_count");
        Box::pin(std::future::ready(()))
    }

    fn subagents_view_opened(
        &self,
        _children_total: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_subagents_open_count");
        Box::pin(std::future::ready(()))
    }

    fn scoped_agent_created(&self, _depth: u32) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_scoped_agent_count");
        Box::pin(std::future::ready(()))
    }

    fn image_paste(
        &self,
        source: &'static str,
        outcome: &'static str,
        format: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        if outcome == "attached" {
            self.count("tui_image_paste_count");
        }
        // Standalone like `tui resume picker closed`: `tui exit` sits at the backend's 128-key
        // cap. Delivery is the client worker's: the paste path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("source", serde_json::Value::from(source));
            properties.set("outcome", serde_json::Value::from(outcome));
            properties.set("format", serde_json::Value::from(format));
            client.track("tui image paste", properties);
            let _ = client.shutdown().await;
        })
    }

    fn credential_added(
        &self,
        kind: &'static str,
        slot: &'static str,
        surface: &'static str,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui image paste`; the login path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("kind", serde_json::Value::from(kind));
            properties.set("slot", serde_json::Value::from(slot));
            properties.set("surface", serde_json::Value::from(surface));
            properties.set("outcome", serde_json::Value::from(outcome));
            client.track("agent credential added", properties);
            let _ = client.shutdown().await;
        })
    }

    fn credential_pinned(
        &self,
        source: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `agent credential added`; the run spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("source", serde_json::Value::from(source));
            client.track("agent credential pinned", properties);
            let _ = client.shutdown().await;
        })
    }

    fn profile_state(&self, active: bool) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone; the run spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let count = pa_core::settings::list_profiles(&self.agent_dir).len();
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("profile_active", serde_json::Value::from(active));
            properties.set("profile_count", serde_json::Value::from(count));
            client.track("agent profile state", properties);
            let _ = client.shutdown().await;
        })
    }

    fn queued_input(
        &self,
        _lane: &'static str,
        _steering_mode: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_input_queued_count");
        Box::pin(std::future::ready(()))
    }

    fn queue_edited(&self, _action: &'static str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_queue_edit_count");
        Box::pin(std::future::ready(()))
    }

    fn queue_dispatched_now(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_queue_dispatch_count");
        Box::pin(std::future::ready(()))
    }

    fn suspend_used(
        &self,
        _outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_suspend_count");
        Box::pin(std::future::ready(()))
    }

    fn agents_view_action(
        &self,
        _action: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.count("tui_agents_action_count");
        Box::pin(std::future::ready(()))
    }

    fn enhanced_keys(
        &self,
        kitty: bool,
        modify_other_keys: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.with(|counters| {
            counters.flags.insert("tui_enhanced_keys_kitty", kitty);
            counters
                .flags
                .insert("tui_enhanced_keys_modify_other_keys", modify_other_keys);
        });
        Box::pin(std::future::ready(()))
    }

    fn hyperlinks_active(&self, enabled: bool) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.with(|counters| {
            counters.flags.insert("tui_hyperlinks_enabled", enabled);
        });
        Box::pin(std::future::ready(()))
    }

    fn client_exit(
        &self,
        reason: &'static str,
        turn_active: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // Every session run of the process shares these counters (the
            // agents view hands one telemetry handle to each session it
            // opens), so each `tui exit` takes what its run counted.
            let counters = std::mem::take(
                &mut *self
                    .counters
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("exit_reason", serde_json::Value::from(reason));
            properties.set("turn_active", serde_json::Value::from(turn_active));
            for (key, count) in &counters.counts {
                properties.set(key, serde_json::Value::from(*count));
            }
            for (key, flag) in &counters.flags {
                properties.set(key, serde_json::Value::from(*flag));
            }
            client.track("tui exit", properties);
            let _ = client.shutdown().await;
        })
    }

    fn resume_picker_closed(
        &self,
        scope: &'static str,
        toggles: u64,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // A standalone event: `tui exit` already carries the backend's 128-key cap, so the
        // picker's scope adoption cannot fold into its counters.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("scope", serde_json::Value::from(scope));
            properties.set("scope_toggle_count", serde_json::Value::from(toggles));
            properties.set("outcome", serde_json::Value::from(outcome));
            client.track("tui resume picker closed", properties);
            let _ = client.shutdown().await;
        })
    }

    fn inline_images_rendered(
        &self,
        counts: pa_tui::interactive::InlineImageCounts,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui resume picker closed`: `tui exit` sits at the backend's 128-key
        // cap. Counts and the protocol name only.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("protocol", serde_json::Value::from(counts.protocol));
            properties.set("kitty_count", serde_json::Value::from(counts.kitty));
            properties.set("iterm2_count", serde_json::Value::from(counts.iterm2));
            properties.set("sixel_count", serde_json::Value::from(counts.sixel));
            properties.set(
                "half_block_count",
                serde_json::Value::from(counts.half_block),
            );
            client.track("tui inline images rendered", properties);
            let _ = client.shutdown().await;
        })
    }

    fn todos_and_questions(
        &self,
        counts: pa_tui::interactive::TodoQuestionCounts,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui inline images rendered`: `tui exit` sits at the backend's 128-key
        // cap. Counts only.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            for (key, count) in [
                ("todo_tray_toggle_count", counts.todo_tray_toggles),
                ("question_answer_count", counts.question_answers),
                ("question_digit_answer_count", counts.question_digit_answers),
                ("question_other_answer_count", counts.question_other_answers),
                ("question_decline_count", counts.question_declines),
                ("question_click_count", counts.question_clicks),
            ] {
                properties.set(key, serde_json::Value::from(count));
            }
            client.track("tui todos and questions", properties);
            let _ = client.shutdown().await;
        })
    }

    fn usage_page_closed(
        &self,
        summary: pa_tui::usage_page::UsagePageSummary,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui resume picker closed`: `tui exit` sits at the backend's 128-key
        // cap. Delivery is the client worker's: the close path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set(
                "provider_count",
                serde_json::Value::from(summary.provider_count),
            );
            properties.set("quotas", serde_json::Value::from(summary.quotas.as_str()));
            properties.set("spend", serde_json::Value::from(summary.spend.as_str()));
            properties.set(
                "details_opened",
                serde_json::Value::from(summary.details_opened),
            );
            properties.set(
                "refresh_count",
                serde_json::Value::from(summary.refresh_count),
            );
            properties.set(
                "reset_chip_shown_count",
                serde_json::Value::from(summary.reset_chip_shown_count),
            );
            for (key, value) in [
                ("billed_call_count", summary.billed_call_count),
                ("plan_call_count", summary.plan_call_count),
                ("free_call_count", summary.free_call_count),
            ] {
                properties.set(key, serde_json::Value::from(value));
            }
            client.track("tui usage page closed", properties);
            let _ = client.shutdown().await;
        })
    }

    fn context_view_closed(
        &self,
        summary: pa_tui::context_view::ContextViewSummary,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui usage page closed`: `tui exit` sits at the backend's 128-key cap.
        // Delivery is the client worker's: the close path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("source", serde_json::Value::from(summary.source.as_str()));
            properties.set(
                "breakdown",
                serde_json::Value::from(summary.breakdown.as_str()),
            );
            properties.set(
                "category_count",
                serde_json::Value::from(summary.category_count),
            );
            properties.set(
                "subagent_count",
                serde_json::Value::from(summary.subagent_count),
            );
            client.track("tui context window closed", properties);
            let _ = client.shutdown().await;
        })
    }

    fn model_picker_closed(
        &self,
        summary: pa_tui::model_picker::ModelPickerSummary,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Standalone like `tui usage page closed`: `tui exit` sits at the backend's 128-key cap.
        // Delivery is the client worker's: the close path spawned this and never awaits it.
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("outcome", serde_json::Value::from(summary.outcome.as_str()));
            properties.set(
                "chip_change_count",
                serde_json::Value::from(summary.chip_change_count),
            );
            properties.set(
                "sort_change_count",
                serde_json::Value::from(summary.sort_change_count),
            );
            properties.set(
                "filters_active",
                serde_json::Value::from(summary.filters_active),
            );
            properties.set("sort", serde_json::Value::from(summary.sort));
            for (name, count) in [
                ("action_menu_count", summary.action_menu_count),
                ("favorite_toggle_count", summary.favorite_toggle_count),
                ("default_set_count", summary.default_set_count),
                ("effort_set_count", summary.effort_set_count),
                ("details_count", summary.details_count),
                ("account_set_count", summary.account_set_count),
            ] {
                properties.set(name, serde_json::Value::from(count));
            }
            client.track("tui model picker closed", properties);
            let _ = client.shutdown().await;
        })
    }

    fn command_used(&self, command: &'static str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // `agent command used` (TS `captureAgentCommandUsed`): every
        // submitted builtin reports from the client, session commands
        // included (the session engine reports none).
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("command_name", serde_json::Value::from(command));
            client.track("agent command used", properties);
            let _ = client.shutdown().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use pa_tui::interactive::InteractionTelemetry as _;

    use super::CliInteractionTelemetry;

    /// Interactions while telemetry is off never count, so a later
    /// `/telemetry on` cannot send them with `tui exit`.
    #[test]
    fn interactions_while_off_never_count() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            futures::executor::block_on(count_interactions_while_off());
        });
    }

    async fn count_interactions_while_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        pa_core::settings::SettingsManager::create(dir.path(), &agent_dir)
            .set_telemetry_enabled(false)
            .unwrap();
        let telemetry = CliInteractionTelemetry::new(dir.path().to_path_buf(), agent_dir);
        telemetry.scroll_used("page", false).await;
        telemetry
            .input_stage(String::new(), "received", "ok", 5)
            .await;
        telemetry.hyperlinks_active(true).await;
        {
            let counters = telemetry.counters.lock().unwrap();
            assert!(counters.counts.is_empty());
            assert!(counters.flags.is_empty());
        }
        pa_core::settings::SettingsManager::create(dir.path(), dir.path().join("agent"))
            .set_telemetry_enabled(true)
            .unwrap();
        telemetry.scroll_used("page", false).await;
        let counters = telemetry.counters.lock().unwrap();
        assert_eq!(counters.counts.get("tui_scroll_count"), Some(&1));
    }

    /// The agents view shares one telemetry handle across the sessions it
    /// opens: each `tui exit` reports only its own run's interactions.
    /// The mirrored events as `[name, action, reason, mode]` rows.
    fn mirrored(agent_dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(agent_dir.join("telemetry.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .map(|event| {
                let properties = &event["properties"];
                serde_json::json!([
                    event["name"],
                    properties["action"],
                    properties["reason"],
                    properties["mode"],
                ])
            })
            .collect()
    }

    /// A transcript navigation report is a standalone event that keeps its catalogued enum.
    #[test]
    fn a_navigation_report_is_a_standalone_event() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let agent_dir = dir.path().join("agent");
            std::fs::create_dir_all(&agent_dir).unwrap();
            let telemetry =
                CliInteractionTelemetry::new(dir.path().to_path_buf(), agent_dir.clone());
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(telemetry.transcript_navigated("next_prompt"));
            assert_eq!(
                mirrored(&agent_dir),
                vec![serde_json::json!([
                    "tui transcript navigation",
                    "next_prompt",
                    null,
                    null
                ])]
            );
        });
    }

    /// A transcript search report is a standalone event carrying only its catalogued enums,
    /// count and flag.
    #[test]
    fn a_search_report_is_a_standalone_event() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let agent_dir = dir.path().join("agent");
            std::fs::create_dir_all(&agent_dir).unwrap();
            let telemetry =
                CliInteractionTelemetry::new(dir.path().to_path_buf(), agent_dir.clone());
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(telemetry.transcript_searched(
                    pa_tui::interactive::TranscriptSearchReport {
                        outcome: "jumped",
                        steps: 3,
                        revealed: true,
                        history: "loaded_only",
                    },
                ));
            let event: serde_json::Value = serde_json::from_str(
                std::fs::read_to_string(agent_dir.join("telemetry.jsonl"))
                    .unwrap()
                    .trim(),
            )
            .unwrap();
            let properties = &event["properties"];
            assert_eq!(
                serde_json::json!([
                    event["name"],
                    properties["outcome"],
                    properties["steps"],
                    properties["revealed"],
                    properties["history"],
                ]),
                serde_json::json!(["tui transcript search", "jumped", 3, true, "loaded_only"])
            );
        });
    }

    /// An unfocused signal report is a standalone event that keeps its catalogued enums.
    #[test]
    fn an_unfocused_signal_report_is_a_standalone_event() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let agent_dir = dir.path().join("agent");
            std::fs::create_dir_all(&agent_dir).unwrap();
            let telemetry =
                CliInteractionTelemetry::new(dir.path().to_path_buf(), agent_dir.clone());
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(telemetry.unfocused_signal_sent("question", "notification"));
            assert_eq!(
                mirrored(&agent_dir),
                vec![serde_json::json!([
                    "tui unfocused signal",
                    null,
                    "question",
                    "notification"
                ])]
            );
        });
    }

    #[test]
    fn each_tui_exit_reports_only_its_own_run() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(report_each_run_once());
        });
    }

    async fn report_each_run_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let telemetry = CliInteractionTelemetry::new(dir.path().to_path_buf(), agent_dir.clone());
        telemetry.scroll_used("page", false).await;
        telemetry.hyperlinks_active(true).await;
        telemetry.client_exit("session_request", false).await;
        telemetry.client_exit("ctrl_d", false).await;
        let mirror = std::fs::read_to_string(agent_dir.join("telemetry.jsonl")).unwrap();
        let exits: Vec<serde_json::Value> = mirror
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|event: &serde_json::Value| event["name"] == "tui exit")
            .collect();
        assert_eq!(exits.len(), 2);
        assert_eq!(exits[0]["properties"]["tui_scroll_count"], 1);
        assert_eq!(exits[0]["properties"]["tui_hyperlinks_enabled"], true);
        assert!(exits[1]["properties"].get("tui_scroll_count").is_none());
        assert!(exits[1]["properties"]
            .get("tui_hyperlinks_enabled")
            .is_none());
    }
}
