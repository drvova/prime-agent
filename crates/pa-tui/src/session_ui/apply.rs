//! The streamed-event pump: client events, turn updates, assistant/tool
//! rows, compaction aborts, and the telemetry seams.
use super::{
    assistant_message_parts, event_to_update, pop_superseded_attempt_row, AgentView,
    AttemptSummary, ChatEntry, CompactionReason, CompactionState, DaemonClientEvent, DaemonCommand,
    Map, MessageBlock, Result, RetryState, SessionUi, StatusKind, ToolResultView, TurnUpdate,
    Value,
};
use crate::interactive::reconnect::{ConnectionEffect, ConnectionEvent};

/// The widest running-tool title the live line shows before it truncates.
const TOOL_LABEL_MAX_WIDTH: usize = 48;

/// One backgrounded compaction-abort outcome: a failed abort surfaces as
/// the transcript note and clears the stuck compaction loader locally.
pub(crate) struct CompactionAbortNote {
    pub(crate) active_session_id: String,
    /// The loader generation the abort addressed: an outcome applies only to that
    /// `compaction_start`; a newer run's loader is never cleared by a stale one.
    pub(crate) compaction_generation: u64,
    pub(crate) outcome: Result<(), String>,
}

impl SessionUi {
    /// Apply one connection event and show its row; the effect tells the run loop what to do next.
    pub(crate) fn connection_event(
        &mut self,
        event: ConnectionEvent,
        view: &mut AgentView,
    ) -> ConnectionEffect {
        let before = std::mem::discriminant(&self.connection);
        let effect = self.connection.on(event);
        match &effect {
            ConnectionEffect::Note(text) | ConnectionEffect::Exit { note: text, .. } => {
                self.note(text, view);
            }
            ConnectionEffect::Info(text) => self.note_as(text, StatusKind::Info, view),
            ConnectionEffect::Warning(text) => self.note_as(text, StatusKind::Warning, view),
            ConnectionEffect::ErrorRow(text) => self.error_row(text, view),
            ConnectionEffect::None
            | ConnectionEffect::Connect { .. }
            | ConnectionEffect::Launch
            | ConnectionEffect::Attach(_) => {}
        }
        if let ConnectionEffect::Exit { reason, .. } = &effect {
            self.exit_reason = reason;
        }
        if before != std::mem::discriminant(&self.connection)
            || !matches!(
                effect,
                ConnectionEffect::None
                    | ConnectionEffect::Connect { .. }
                    | ConnectionEffect::Launch
                    | ConnectionEffect::Attach(_)
            )
        {
            self.dirty = true;
        }
        effect
    }

    /// Abort the active turn off the UI loop: with the schema-29 capability the daemon aborts the
    /// run and the queue keeps flowing behind the settled turn; otherwise the plain abort.
    fn abort_turn(&self) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let notes = self.notes.clone();
        tokio::spawn(async move {
            let mut result = if client.supports_server_capability("abort_and_send_queued") {
                client
                    .request_ok(DaemonCommand::AbortAndSendQueued {
                        id: None,
                        active_session_id: active_session_id.clone(),
                        rest: Map::default(),
                    })
                    .await
            } else {
                client
                    .request_ok(DaemonCommand::Abort {
                        id: None,
                        active_session_id: active_session_id.clone(),
                        rest: Map::default(),
                    })
                    .await
            };
            // A daemon that rejects the command gets the plain abort too.
            if matches!(&result, Err(error) if error.to_string().contains("abort_and_send_queued"))
            {
                result = client
                    .request_ok(DaemonCommand::Abort {
                        id: None,
                        active_session_id,
                        rest: Map::default(),
                    })
                    .await;
            }
            if let Err(error) = result {
                let _ = notes.send(format!("the abort failed: {error:#}"));
            }
        });
    }

    /// Cancel the in-flight compaction off the UI loop; the request never
    /// blocks key handling.
    fn abort_compaction(&self, compaction_generation: u64) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let abort_notes = self.compaction_abort_notes.clone();
        tokio::spawn(async move {
            // Via the supervisor even when a direct link serves the session: the direct link IS the
            // wedged worker in the case the supervisor's abort arm exists for.
            let result = client
                .request_ok_via_supervisor(DaemonCommand::AbortCompaction {
                    id: None,
                    active_session_id: active_session_id.clone(),
                    rest: Map::default(),
                })
                .await;
            if let Err(error) = result {
                let _ = abort_notes.send(CompactionAbortNote {
                    active_session_id,
                    compaction_generation,
                    outcome: Err(format!("{error:#}")),
                });
            }
        });
    }

    /// Apply one backgrounded compaction-abort outcome: the failed note
    /// surfaces as a transcript row and the compaction loader clears.
    pub(crate) fn apply_compaction_abort_outcome(
        &mut self,
        note: CompactionAbortNote,
        view: &mut AgentView,
    ) {
        if note.active_session_id != self.active_session_id
            || note.compaction_generation != view.compaction_generation
        {
            return;
        }
        if let Err(error) = note.outcome {
            self.note(&format!("the compaction abort failed: {error:#}"), view);
            view.compaction = None;
        }
    }

    pub(crate) fn apply_background_note(&mut self, text: &str, view: &mut AgentView) {
        self.note(text, view);
    }

    /// Report the first scroll action of the run (`tui_scroll_count`),
    /// fire-and-forget so the keypress never waits on the telemetry flush.
    pub(super) fn track_scroll(&mut self, action: &'static str, resumed_following: bool) {
        if self.scroll_adoption_emitted {
            return;
        }
        self.scroll_adoption_emitted = true;
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.scroll_used(action, resumed_following).await;
            });
        }
    }

    /// Report the run's first selection copy (`tui selection used`),
    /// fire-and-forget. `lines` is the copied text's line count.
    pub(super) fn track_selection(&mut self, lines: usize) {
        if self.selection_adoption_emitted {
            return;
        }
        self.selection_adoption_emitted = true;
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.selection_used(lines).await;
            });
        }
    }

    /// The run's first click-driven interaction (`tui click used`).
    pub(crate) fn track_click(&mut self, surface: &'static str) {
        if self.click_adoption_emitted {
            return;
        }
        self.click_adoption_emitted = true;
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.click_used(surface).await;
            });
        }
    }

    /// Take a pending `app.suspend` request: the interactive loop owns the
    /// renderer that hands the terminal over.
    pub(crate) fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_requested)
    }

    /// Take a pending `app.editor.external` request: the interactive loop
    /// performs the editor child's terminal handoff.
    pub(crate) fn take_external_editor_request(&mut self) -> Option<String> {
        self.external_editor_request.take()
    }

    /// Report the run's first suspend cycle (`tui suspend used`),
    /// fire-and-forget. `outcome` is `resumed` or `failed`.
    pub(crate) fn track_suspend_used(&mut self, outcome: &'static str) {
        if self.suspend_adoption_emitted {
            return;
        }
        self.suspend_adoption_emitted = true;
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.suspend_used(outcome).await;
            });
        }
    }

    /// Report a builtin command submission (`agent command used`),
    /// fire-and-forget like the scroll event: the command's handling never
    /// waits on the telemetry flush.
    pub(super) fn track_command_used(&mut self, command: &'static str) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.command_used(command).await;
            });
        }
    }

    /// Report a feature attempt's observed outcome (`agent feature
    /// outcome`), fire-and-forget.
    pub(super) fn track_feature_outcome(
        &mut self,
        feature: &'static str,
        outcome: &'static str,
        duration_ms: Option<u64>,
    ) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry
                    .feature_outcome(feature, outcome, duration_ms)
                    .await;
            });
        }
    }

    pub(crate) fn apply_client_event(&mut self, event: DaemonClientEvent, view: &mut AgentView) {
        match event {
            DaemonClientEvent::SessionEvent {
                active_session_id,
                event,
                meta_sequence,
            } => {
                if active_session_id != self.active_session_id {
                    return;
                }
                // Any frame from the session resets the loader's stall timer; a stall a paint
                // actually showed (`Waiting on the model`) ends here and counts once: the flag is
                // taken, so the rest of a drained burst sees no stall.
                if std::mem::take(&mut view.stall_shown) {
                    self.track_turn_feedback(crate::interactive::TurnFeedback::StallShown);
                }
                view.working_heard = Some(std::time::Instant::now());
                // The run's stash keys the LATEST sequence the worker has reported: a turn during
                // this run advances it, so the re-entry matches the post-turn value the next attach
                // reports. Monotonic max: a replayed event never lowers it.
                if meta_sequence > self.last_event_sequence {
                    self.last_event_sequence = meta_sequence;
                }
                // The live context reading a turn's message events carry (each settled row's
                // measurement, each streamed update's estimate) moves the tray now; it is newer
                // than any turn-end stats refresh still in flight, so that reply is superseded.
                if let Some(context) = event
                    .get("contextUsage")
                    .and_then(crate::chrome::ContextUsage::from_wire)
                {
                    self.stats_generation += 1;
                    self.context = crate::chrome::ContextUsage::settle(Some(context), self.context);
                    self.rebuild_tray(view);
                }
                if let Some(update) = event_to_update(&event) {
                    self.apply_update(update, view);
                }
            }
            DaemonClientEvent::SideQuestionEvent {
                active_session_id,
                event,
            } => {
                if active_session_id == self.active_session_id {
                    self.apply_side_question_event(&event, view);
                }
            }
            DaemonClientEvent::SessionClosed {
                active_session_id,
                reason,
            } => {
                if active_session_id == self.active_session_id {
                    self.turn_active = false;
                    view.working = None;
                    self.bash_list_epoch += 1;
                    self.factory_list_epoch += 1;
                    self.bash_activities = serde_json::json!({"activities": []});
                    self.factory_graph = serde_json::json!({"runs": []});
                    if let Some(factory_view) = view.factory_view.as_mut() {
                        factory_view.apply_runs(Vec::new());
                    }
                    self.update_subagent_summary(view);
                    match reason.as_str() {
                        "shutdown" => self.error_row(
                            "The Prime Agent daemon shut down while this window was attached. The session transcript remains saved; restart Prime Agent and reopen it from Agents View.",
                            view,
                        ),
                        "replaced" => self.error_row(
                            "The daemon replaced this agent session with another session. Reopen the current session from Agents View.",
                            view,
                        ),
                        "killed" => self.error_row(
                            "The daemon stopped this agent session. Its transcript remains saved and can be reopened from Agents View.",
                            view,
                        ),
                        _ => self.note(&format!("session closed ({reason})"), view),
                    }
                }
            }
            DaemonClientEvent::DirectLinkLost { active_session_id } => {
                // A direct-transport loss is never itself a session loss — the session re-attaches
                // through the supervisor; only the active session arms it.
                if active_session_id == self.active_session_id {
                    self.connection_event(
                        ConnectionEvent::DirectLinkLost { active_session_id },
                        view,
                    );
                }
            }
            DaemonClientEvent::DaemonClosing { reason, update } => {
                if let Some(update) = update {
                    // Spec §10: reattach is the default end state. The banner carries the
                    // resume contract; the interactive run's reconnect loop drives the rest.
                    let names = update
                        .sessions
                        .iter()
                        .map(|row| {
                            row.get("name")
                                .and_then(|value| value.as_str())
                                .filter(|name| !name.is_empty())
                                .unwrap_or_else(|| {
                                    row.get("sessionId")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                })
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    view.push_entry(crate::chat::ChatEntry::Status {
                        text: format!(
                            "Prime Agent is updating — restarting the daemon (about {}s). {} will resume automatically.",
                            update.est_seconds.max(1),
                            if names.is_empty() { "Your session".to_string() } else { names }
                        ),
                        kind: crate::chat::StatusKind::Info,
                    });
                    self.connection_event(
                        ConnectionEvent::UpdateNotice {
                            est_seconds: update.est_seconds,
                        },
                        view,
                    );
                } else {
                    self.note(&format!("the daemon is shutting down ({reason})"), view);
                    // The announcement is what arms the shutdown recovery: a bare session stop
                    // without it stays stopped.
                    if reason == "shutdown" {
                        self.connection_event(ConnectionEvent::ShutdownNotice, view);
                    }
                }
            }
            // The roster push keeps the subagent summary counts live.
            DaemonClientEvent::RosterUpdate {
                changed,
                removed,
                resync,
            } => {
                self.apply_roster_update(changed, removed, resync);
                self.update_subagent_summary(view);
                self.dirty = true;
            }
            // A heartbeat catalog change anywhere in the daemon: the scoped catalog refreshes in
            // the background — the dock's counts follow even with the view closed.
            DaemonClientEvent::HeartbeatsChanged => {
                self.spawn_heartbeat_refresh();
            }
            // A background daemon-side catalog refresh changed the served snapshot (Rust-only
            // no-stall picker-open extension): clients re-fetch instantly from the warm caches.
            DaemonClientEvent::ModelCatalogChanged => {
                self.spawn_model_catalog_refresh();
            }
            // A worker replacement superseded the id this client holds: the loop
            // re-attaches to the session's current id (silent rebind, no banner).
            DaemonClientEvent::SessionBinding {
                previous_active_session_id,
                active_session_id,
            } => {
                if previous_active_session_id == self.active_session_id
                    && !active_session_id.is_empty()
                    && active_session_id != self.active_session_id
                {
                    self.pending_rebind = Some(active_session_id);
                }
            }
            // A distribution build's background chat import: one status row when it starts and
            // one when it lands (the resume picker re-reads its catalog on the latter).
            DaemonClientEvent::HistoryImport {
                finished,
                sessions,
                kept,
                config,
                error,
            } => {
                view.push_entry(crate::chat::ChatEntry::Status {
                    text: crate::history_import_notice::history_import_notice(
                        finished,
                        sessions,
                        kept,
                        &config,
                        error.as_deref(),
                    ),
                    kind: if error.is_some() {
                        crate::chat::StatusKind::Warning
                    } else {
                        crate::chat::StatusKind::Info
                    },
                });
                self.dirty = true;
            }
            // The traffic feed is the agents view's own opt-in: the chat never subscribes.
            DaemonClientEvent::SessionListItem { .. }
            | DaemonClientEvent::SessionListProgress { .. }
            | DaemonClientEvent::AgentTraffic(_) => {}
        }
    }

    fn apply_update(&mut self, update: TurnUpdate, view: &mut AgentView) {
        match update {
            TurnUpdate::TurnStarted => {
                self.turn_active = true;
                self.turn_error_shown = false;
                // A new turn opens no retry episode yet: an aborted attempt's episode never saw
                // its `auto_retry_end`. The retried run's own `agent_start` lands while the
                // countdown is still up and keeps the episode it continues.
                if view.retry.is_none() {
                    self.retry_episode_waited = None;
                    self.retry_waited = None;
                }
                self.running_tool_labels.clear();
                // A model turn after an aborted tool means the run went on: no interrupt row.
                self.tool_aborted = false;
                // A new turn also settles leftovers from a lost turn-end frame.
                crate::snapshot::settle_pending_tool_cards(
                    view,
                    &mut self.pending_tools,
                    &mut self.aborted_tools,
                    "the turn ended before this call returned",
                );
                self.aborted_tools.clear();
                // Only the retry that follows a failed attempt's `agent_end` pops its summary.
                self.attempt_summary = None;
                // A retried run keeps the episode's start: the timer never resets mid-episode.
                let since = self.retry_run_since.unwrap_or_else(std::time::Instant::now);
                self.start_loader_at(view, since);
            }
            TurnUpdate::UserMessage { text, images } => {
                match crate::custom_message::skill_invocation_entries(&text) {
                    Some(entries) => {
                        for entry in entries {
                            view.push_entry(entry);
                        }
                    }
                    // A live prompt is stamped as it lands (its row's clock).
                    None => view.push_entry(ChatEntry::User {
                        text,
                        sent_at: i64::try_from(crate::agents_view_state::now_ms()).ok(),
                        images,
                    }),
                }
            }
            // The other-client arm of a display-name move (`/name` sets it locally).
            TurnUpdate::SessionInfoChanged { name } => {
                self.session_name = name;
                view.chrome.chat_name = self.session_display();
                self.dirty = true;
            }
            // Keep the local tier state current; `/fast` reads it.
            TurnUpdate::ServiceTierChanged { tier } => {
                self.service_tier = Some(tier);
                view.chrome.service_tier.clone_from(&self.service_tier);
                self.dirty = true;
            }
            TurnUpdate::CustomRow(entry) => {
                view.push_entry(entry);
            }
            // The daemon's report on this session's health lands in the transcript where it
            // happened; the count is handed off and never awaited here.
            TurnUpdate::SessionFault(fault) => {
                view.push_entry(crate::crash_notice::fault_row(&fault));
                crate::crash_notice::report_shown(
                    self.telemetry.as_ref(),
                    crate::crash_notice::fault_kind(&fault),
                );
                self.dirty = true;
            }
            // A recovered episode this client watched leaves the nc-style receipt with the
            // wait it measured; a replayed or unwatched one keeps the daemon's row text.
            TurnUpdate::RetryRecovered {
                entry,
                attempts,
                last_error,
            } => match self.retry_waited.take() {
                Some(waited) => {
                    let retries = if attempts == 1 { "retry" } else { "retries" };
                    let waited = waited.as_secs_f64();
                    let text = if last_error.is_empty() {
                        format!("\u{2022} Waited {waited:.1}s through {attempts} {retries}")
                    } else {
                        format!(
                            "\u{2022} Waited {waited:.1}s through {attempts} {retries} \u{00b7} {last_error}"
                        )
                    };
                    view.push_entry(ChatEntry::Status {
                        text,
                        kind: StatusKind::Info,
                    });
                }
                None => view.push_entry(entry),
            },
            TurnUpdate::AssistantMessage {
                message,
                streaming,
                stream_event,
            } => {
                self.apply_assistant_message(&message, streaming, stream_event.as_ref(), view);
            }
            TurnUpdate::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                // The failed frame's sweep owns the call: the run's late tool frames land
                // on nothing. A start outside a live turn is one of those late
                // frames — it would allocate a fresh card whose timer then
                // reticks the settled id on this run's paints, so it lands
                // on nothing too.
                if !self.aborted_tools.contains(&tool_call_id) && self.turn_active {
                    // The live label is the call's title: a kernel cell's preview (a Scheme
                    // cell's kernel call and its main argument, as its card is titled; a
                    // Python `%%bash` cell previews its command), else the tool's name.
                    let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
                    let preview = match pa_types::ai::kernel_tool_language(&tool_name) {
                        Some(pa_types::ai::KernelLanguage::Scheme) => {
                            let preview = crate::code_preview::scheme_outline(code).preview();
                            [preview.head, preview.text]
                                .into_iter()
                                .filter(|part| !part.is_empty())
                                .collect::<Vec<_>>()
                                .join(" ")
                        }
                        Some(pa_types::ai::KernelLanguage::Python) => {
                            crate::code_preview::preview_ipython_code(code).text
                        }
                        None => String::new(),
                    };
                    let title = if preview.is_empty() {
                        tool_name.clone()
                    } else {
                        preview
                    };
                    let title =
                        crate::width::truncate_to_width(&title, TOOL_LABEL_MAX_WIDTH, "\u{2026}");
                    if let Some(working) = &mut view.working {
                        working.tool_label = Some(title.clone());
                    }
                    self.running_tool_labels
                        .retain(|(running_id, _)| *running_id != tool_call_id);
                    self.running_tool_labels.push((tool_call_id.clone(), title));
                    crate::snapshot::apply_tool_execution_start(
                        view,
                        &tool_call_id,
                        &tool_name,
                        args,
                    );
                    // Re-register the call in the pending map (a card may predate a
                    // pending-state reset or arrive without its own streamed frame).
                    self.pending_tools.insert(tool_call_id.clone());
                    self.working_tokens.run.0 += 1;
                    Self::set_working_activity(crate::chat::TOOL_ACTIVITY, false, view);
                    self.track_live_tool_timer();
                }
            }
            TurnUpdate::ToolExecutionUpdate {
                tool_call_id,
                partial,
            } => {
                // A `starting` partial (python-kernel bootstrap) owns the loader note; other
                // updates leave any current note alone. The note rides the landed-result gate so an
                // aborted card's late frames cannot clobber the delivered turn's loader.
                let loader_message = crate::snapshot::working_message_from_update(&partial);
                if self.apply_tool_result(&tool_call_id, &partial, false, true, view) {
                    if let (Some(message), Some(working)) = (loader_message, view.working.as_mut())
                    {
                        working.message = Some(message);
                    }
                }
            }
            TurnUpdate::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => {
                // The landed-result gate keeps the loader safe: a late end frame from the aborted
                // run must not rewrite the new turn's activity label or clear its loader note.
                let aborted =
                    result.pointer("/details/status").and_then(Value::as_str) == Some("aborted");
                if self.apply_tool_result(&tool_call_id, &result, is_error, false, view) {
                    self.tool_aborted |= aborted;
                    // A parallel call still executing keeps the live line on it (its title, the
                    // tool activity); only the last call's end falls back to waiting.
                    let pending = &self.pending_tools;
                    self.running_tool_labels
                        .retain(|(running_id, _)| pending.contains(running_id));
                    let remaining = self.running_tool_labels.last().map(|(_, title)| title);
                    if remaining.is_none() {
                        Self::set_working_activity("Waiting", false, view);
                    }
                    if let Some(working) = &mut view.working {
                        working.tool_label = remaining.cloned();
                        if remaining.is_none() {
                            working.message = None;
                        }
                    }
                }
            }
            TurnUpdate::TurnEnded { error } => {
                // One LLM turn ended; the run, and with it the busy state, lasts until
                // `agent_end`.
                self.streaming_index = None;
                self.turn_ends_seen += 1;
                view.retry = None;
                crate::snapshot::settle_pending_tool_cards(
                    view,
                    &mut self.pending_tools,
                    &mut self.aborted_tools,
                    "the turn ended before this call returned",
                );
                self.aborted_tools.clear();
                // A provider failure already surfaced through the failed assistant message
                // or the retry-exhausted banner; the turn result error is only a backstop.
                if let (Some(error), false) = (error, self.turn_error_shown) {
                    view.push_entry(ChatEntry::Status {
                        text: format!("turn failed: {error}"),
                        kind: StatusKind::Warning,
                    });
                }
            }
            TurnUpdate::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                // The retry countdown loader replaces the working loader until the loop settles.
                // SANCTIONED DIVERGENCE
                // (operator ruling 2026-09-23): the just-failed attempt's error row leaves the chat
                // — the transient loader line is the ONE error shown while the episode runs, and
                // the durable outcome row replaces it at the settle.
                if let Some(since) = self.pop_superseded_attempt(view) {
                    self.retry_run_since = Some(since);
                }
                // The receipt's wait is the episode's backoff: each start adds its own delay, so
                // the attempts' streaming time (the recovered one's included) never counts.
                let delay = std::time::Duration::from_millis(delay_ms);
                *self.retry_episode_waited.get_or_insert_default() += delay;
                view.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                    ends_at: std::time::Instant::now() + delay,
                    error_message,
                    provider: view.chrome.model_provider.clone(),
                });
            }
            TurnUpdate::AutoRetryEnd {
                success,
                attempt: _,
                final_error,
            } => {
                view.retry = None;
                let waited = self.retry_episode_waited.take();
                self.retry_waited = waited.filter(|_| success);
                if final_error.is_some() {
                    self.turn_error_shown = true;
                    // The give-up's final attempt (error and summary rows) is superseded by the
                    // ONE terminal line.
                    self.pop_superseded_attempt(view);
                }
            }
            TurnUpdate::CompactionStart {
                reason,
                custom_instructions,
            } => {
                // The compaction loader replaces the working loader; the generation bump retires
                // every in-flight abort outcome from an earlier run's loader.
                view.working = None;
                view.compaction_generation += 1;
                view.compaction = Some(CompactionState {
                    reason: CompactionReason::parse(&reason),
                    custom_instructions,
                    // A fresh run starts with an empty live summary: a replayed
                    // `compaction_start` after a re-attach drops the old partial text too.
                    summary: String::new(),
                });
            }
            TurnUpdate::CompactionSummaryDelta { delta } => {
                // One streamed chunk of the summary: append onto the live loader's state. A delta
                // without a live loader (late attach, stale frame) drops — the settling end still
                // carries the full summary.
                if let Some(compaction) = view.compaction.as_mut() {
                    compaction.summary.push_str(&delta);
                }
            }
            TurnUpdate::CompactionEnd {
                reason,
                result,
                custom_instructions,
                aborted,
                error_message,
                error_severity,
            } => {
                view.compaction = None;
                if let Some(result) = result {
                    // The display transcript keeps every row: the summary appends at the
                    // compaction's own position.
                    view.push_entry(ChatEntry::CompactionSummary {
                        summary: result
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        tokens_before: result
                            .get("tokensBefore")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                        custom_instructions,
                    });
                    self.compaction_settled = true;
                } else if reason == "manual" {
                    // Aborts and error messages surface only for user-issued compactions.
                    if aborted {
                        view.push_entry(ChatEntry::Status {
                            text: "\u{26a0} Error: Compaction cancelled".to_string(),
                            kind: StatusKind::Error,
                        });
                    } else if let Some(message) = error_message {
                        if error_severity.as_deref() == Some("warning") {
                            view.push_entry(ChatEntry::Status {
                                text: format!("\u{26a0} {message}"),
                                kind: StatusKind::Warning,
                            });
                        } else {
                            view.push_entry(ChatEntry::Status {
                                text: format!("\u{26a0} Error: {message}"),
                                kind: StatusKind::Error,
                            });
                        }
                    }
                }
            }
            TurnUpdate::Idle => self.end_run(view),
            TurnUpdate::GoalUpdate(goal) => {
                self.apply_goal_update(goal, view);
            }
            TurnUpdate::TodoUpdate(todos) => {
                view.todos = todos;
                self.dirty = true;
            }
            TurnUpdate::QuestionOpen(question) => {
                view.question = Some(crate::question_prompt::QuestionPrompt::new(question));
                // The turn is blocked on the user, not working: the loader says so.
                if let Some(working) = view.working.as_mut() {
                    working.message = Some("Waiting for your answer".to_string());
                }
                self.dirty = true;
            }
            TurnUpdate::Notification(notification) => {
                use pa_types::notification::NotificationLevel;
                let (mark, kind) = match notification.level {
                    NotificationLevel::Info => ("\u{2139}", StatusKind::Info),
                    NotificationLevel::Success => ("\u{2713}", StatusKind::Info),
                    NotificationLevel::Warning => ("\u{26a0}", StatusKind::Warning),
                    NotificationLevel::Error => ("\u{2717}", StatusKind::Error),
                };
                view.push_entry(ChatEntry::Status {
                    text: format!("{mark} {}", notification.message),
                    kind,
                });
                // The terminal's attention signal rides the next frame's write.
                let osc_indicators = self
                    .client_settings
                    .as_ref()
                    .is_some_and(|settings| settings.show_terminal_progress());
                view.pending_attention = Some(crate::terminal_attention::Attention::for_ping(
                    &notification.message,
                    osc_indicators,
                ));
                self.dirty = true;
            }
            TurnUpdate::QuestionClosed { question_id } => {
                if view
                    .question
                    .as_ref()
                    .is_some_and(|prompt| prompt.question.id == question_id)
                {
                    view.question = None;
                }
                if let Some(working) = view.working.as_mut() {
                    working.message = None;
                }
                self.dirty = true;
            }
            TurnUpdate::BashStart {
                command,
                exclude_from_context,
                transient,
                run_id,
            } => {
                self.apply_bash_start(
                    &command,
                    exclude_from_context,
                    transient,
                    run_id.as_deref(),
                    view,
                );
            }
            TurnUpdate::BashOutput { chunk } => {
                self.apply_bash_output(&chunk, view);
            }
            TurnUpdate::BashEnd {
                exit_code,
                cancelled,
                truncated,
                full_output_path,
                error_message,
                transient,
                run_id,
            } => {
                self.apply_bash_end(
                    exit_code,
                    cancelled,
                    truncated,
                    full_output_path,
                    error_message,
                    transient,
                    run_id.as_deref(),
                    view,
                );
            }
            TurnUpdate::QueueUpdated {
                steering,
                follow_ups,
                starting,
                rlm_child_status,
                injected_prompts,
            } => {
                view.queued = crate::queued::QueuedMessages {
                    steering,
                    follow_ups,
                    starting,
                    rlm_child_status,
                    injected_prompts,
                };
                // A queue change under an active browse reconciles the selection: the cursor
                // survives only when the addressed item is unchanged; a stale selection drops.
                if let Some(selected) = self.queue_selection.selected() {
                    let (lane, index, text) =
                        (selected.lane, selected.index, selected.text.clone());
                    if let Some(draft) =
                        self.queue_selection
                            .refresh_at(&view.queued, lane, index, &text)
                    {
                        if view.editor.get_text() == text {
                            view.editor.set_text(&draft);
                        }
                    }
                }
                self.sync_queue_selection(view);
            }
            TurnUpdate::StatusUpdate => {}
        }
        self.dirty = true;
    }

    /// The agent run ended: the busy state clears and the held bash cards stream into the
    /// transcript.
    fn end_run(&mut self, view: &mut AgentView) {
        self.turn_active = false;
        view.working = None;
        // An interrupt that lands on a running tool ends the run with no aborted assistant
        // message: the row the model turn would have carried is written here, once.
        if std::mem::take(&mut self.tool_aborted) && !self.turn_error_shown {
            view.push_entry(ChatEntry::Assistant(Box::new(
                crate::chat::AssistantMessage {
                    blocks: Vec::new(),
                    has_tool_calls: false,
                    streaming: false,
                    error: Some(crate::chat::live_abort_text(
                        0,
                        view.working_since.map(|since| since.elapsed().as_secs()),
                    )),
                    aborted: true,
                },
            )));
        }
        // The run's summary row: how long the prompt took and what it cost.
        if let Some(since) = view.working_since.take() {
            let elapsed = crate::chat::format_working_elapsed(since.elapsed().as_secs());
            let (calls, tokens) = self.working_tokens.run;
            let (s, tokens) = (
                if calls == 1 { "" } else { "s" },
                crate::chrome::format_token_count(tokens),
            );
            let text =
                format!("Worked for {elapsed} \u{b7} {calls} tool call{s} \u{b7} {tokens} tokens");
            // The cache segment shows only for a run that read from the cache: a provider
            // without cache support (or a cold cache) reports zero reads, and a figure that
            // rounds to `0% cached` says nothing either.
            let cache_hit = crate::context_view::cache_hit_percent(self.working_tokens.run_cache)
                .filter(|hit| hit.round() >= 1.0);
            let follows_failed_attempt = view
                .chat
                .last()
                .is_some_and(crate::snapshot::is_superseded_attempt_row);
            let summary = ChatEntry::RunSummary(crate::chat::RunSummaryRow { text, cache_hit });
            view.run_summary = Some(summary.clone());
            view.push_entry(summary);
            if follows_failed_attempt {
                self.attempt_summary = Some(AttemptSummary {
                    index: view.chat_len() - 1,
                    since,
                });
            }
            self.track_feature_outcome("summary", "completed", None);
        }
        self.retry_run_since = None;
        // A run that ends without its `turn_end` (a pre-run abort, a detach) still
        // leaves no live streaming slot or retry countdown behind.
        self.streaming_index = None;
        view.retry = None;
        // The retry episode outlives its attempts' runs: every failed attempt ends its run
        // before its `auto_retry_start`, and the recovered run's `agent_end` can land before the
        // `auto_retry_end` and its outcome row. The next human turn's start drops a stale one.
        self.running_tool_labels.clear();
        Self::flush_pending_bash(view);
        // A detached run's `turn_end` never arrives: its open cards settle here.
        crate::snapshot::settle_pending_tool_cards(
            view,
            &mut self.pending_tools,
            &mut self.aborted_tools,
            "the turn ended before this call returned",
        );
    }

    /// The retry-episode collapse for one failed attempt: its run summary (while it is still the
    /// last row) and then its error row leave the chat. Returns the popped run's start.
    fn pop_superseded_attempt(&mut self, view: &mut AgentView) -> Option<std::time::Instant> {
        let summary = self
            .attempt_summary
            .take()
            .filter(|summary| summary.index + 1 == view.chat_len());
        if summary.is_some() {
            view.pop_chat_entry();
        }
        pop_superseded_attempt_row(view);
        summary.map(|summary| summary.since)
    }

    /// The abort ladder shared by the Ctrl+C and Escape interrupts: a compacting session aborts the
    /// compaction run, a streaming turn aborts through `abort_and_send_queued`, and a running user
    /// bash aborts alongside. The side-question abort stays at the call sites.
    pub(super) fn interrupt_running_work(&self, view: &AgentView) {
        // Abort the trace upload sweep first.
        if let Some(run) = &self.trace_upload {
            run.cancel.cancel();
        }
        if view.compaction.is_some() {
            // The interrupt cancels the compaction run only — the agent is not
            // streaming, so no turn abort goes out.
            self.abort_compaction(view.compaction_generation);
        } else if self.turn_active {
            // No transient abort hint: the aborted turn's own assistant row carries
            // the interrupt, so nothing is noted here.
            self.abort_turn();
        }
        // A running user-bash command aborts the same way; the settled run
        // reports cancelled through its bash_end.
        if self.user_bash_running {
            self.abort_user_bash();
        }
    }

    /// Apply an assistant message frame: an open streaming message is updated in place; otherwise
    /// the message expands into a chat component plus a card per tool call.
    fn apply_assistant_message(
        &mut self,
        message: &Value,
        streaming: bool,
        stream_event: Option<&Value>,
        view: &mut AgentView,
    ) {
        if let Some(event) = stream_event {
            Self::track_stream_activity(event, view);
        }
        let (blocks, tool_calls) = assistant_message_parts(message);
        // A message_start always opens a new streaming message; later frames
        // update it in place.
        let starts_message = stream_event
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str)
            == Some("start");
        let usage_output = message
            .get("usage")
            .and_then(|usage| usage.get("output"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if streaming {
            if starts_message {
                self.working_tokens.start_message();
            }
            let content_chars: u64 = blocks
                .iter()
                .map(|block| match block {
                    MessageBlock::Text(text) | MessageBlock::Thinking(text) => {
                        text.chars().count() as u64
                    }
                })
                .sum();
            let current = self
                .working_tokens
                .apply_streaming(usage_output, content_chars);
            // The live thought: the thinking block's newest closed sentence or line (it holds
            // still while the next one streams), or the text so far before anything closes.
            let thought = match (blocks.last(), tool_calls.is_empty()) {
                (Some(MessageBlock::Thinking(text)), true) => {
                    let (mut closed, mut start) = (None, 0);
                    for (at, ch) in text.char_indices() {
                        let end = at + ch.len_utf8();
                        let boundary = ch == '\n'
                            || (matches!(ch, '.' | '?' | '!')
                                && text[end..].chars().next().is_none_or(char::is_whitespace));
                        if boundary {
                            closed = Some(text[start..end].trim())
                                .filter(|seg| !seg.is_empty())
                                .or(closed);
                            start = end;
                        }
                    }
                    closed
                        .or_else(|| Some(text[start..].trim()).filter(|seg| !seg.is_empty()))
                        .map(str::to_string)
                }
                (Some(MessageBlock::Thinking(_)), false)
                | (Some(MessageBlock::Text(_)) | None, _) => None,
            };
            if let Some(working) = &mut view.working {
                working.tokens = working.tokens.max(current);
                working.thought = thought;
            }
        } else {
            if let Some(working) = &mut view.working {
                working.thought = None;
            }
            self.working_tokens.settle(usage_output);
            let usage = &message["usage"];
            self.working_tokens.run.1 += usage["totalTokens"].as_u64().unwrap_or(0);
            let run_cache = &mut self.working_tokens.run_cache;
            run_cache.input += usage["input"].as_u64().unwrap_or(0);
            run_cache.cache_read += usage["cacheRead"].as_u64().unwrap_or(0);
            run_cache.cache_write += usage["cacheWrite"].as_u64().unwrap_or(0);
            self.record_speed_sample(message, view);
            self.observe_rate_limit(message);
        }
        if let Some(text) = blocks.iter().rev().find_map(|block| match block {
            MessageBlock::Text(text) => Some(text.clone()),
            MessageBlock::Thinking(_) => None,
        }) {
            self.last_assistant_text = Some(text);
        }
        let has_tool_calls = !tool_calls.is_empty();
        if starts_message {
            self.streaming_index = None;
        }
        match self.streaming_index {
            Some(index) => {
                view.prepare_entry_mutation(index);
                if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(index) {
                    open.blocks = blocks;
                    open.has_tool_calls = has_tool_calls;
                    open.streaming = streaming;
                }
                view.mark_entry_stale(index);
            }
            None => {
                if !blocks.is_empty() {
                    view.push_entry(ChatEntry::Assistant(Box::new(
                        crate::chat::AssistantMessage {
                            blocks,
                            has_tool_calls,
                            streaming,
                            error: None,
                            aborted: false,
                        },
                    )));
                    self.streaming_index = Some(view.chat.len() - 1);
                }
            }
        }
        for (id, name, args) in &tool_calls {
            // A streamed tool call first appears queued; the execution start flips it to running,
            // later frames refresh its name and args. The failed run's late frames touch no card.
            if !self.aborted_tools.contains(id) {
                crate::snapshot::apply_streamed_tool_card(view, id, name, args);
            }
            if starts_message {
                // A new assistant message re-arms a reused id; a late `message_update` from a
                // failed run must not. The re-armed invocation's next frame pushes its own fresh
                // card.
                self.aborted_tools.remove(id);
            }
            // `message_update` registers every streamed call in the pending map
            // (`message_start` and the final frame never do).
            if streaming && !starts_message && !self.aborted_tools.contains(id) {
                self.pending_tools.insert(id.clone());
            }
        }
        if !streaming {
            let open = self.streaming_index.take();
            self.finalize_assistant_error(message, &tool_calls, open, view);
        }
    }

    /// The final frame of a failed assistant message renders its error row: `aborted` always shows,
    /// `error` only when the message carries no tool calls. A content-less provider failure stacks
    /// its own error row per failed attempt instead of decorating the previous reply.
    fn finalize_assistant_error(
        &mut self,
        message: &Value,
        tool_calls: &[(String, String, Value)],
        open: Option<usize>,
        view: &mut AgentView,
    ) {
        // An aborted run's row text is the client's own — the retry count and the working-elapsed
        // suffix never ride the wire — and every pending tool card settles with the failure text.
        let stop_reason = message.get("stopReason").and_then(Value::as_str);
        let abort_text = if stop_reason == Some("aborted") {
            Some(crate::chat::live_abort_text(
                view.retry.as_ref().map_or(0, |retry| retry.attempt),
                view.working_since.map(|since| since.elapsed().as_secs()),
            ))
        } else {
            None
        };
        if abort_text.is_some() || stop_reason == Some("error") {
            let settle_text = abort_text.clone().unwrap_or_else(|| {
                message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("Error")
                    .to_string()
            });
            crate::snapshot::settle_pending_tool_cards(
                view,
                &mut self.pending_tools,
                &mut self.aborted_tools,
                &settle_text,
            );
        }
        let Some(error) = crate::snapshot::assistant_error_row(message, tool_calls) else {
            return;
        };
        let error = match abort_text {
            Some(text) => crate::snapshot::AssistantErrorRow {
                text,
                aborted: true,
            },
            None => error,
        };
        self.turn_error_shown = true;
        if let Some(index) = open {
            view.prepare_entry_mutation(index);
            if let Some(ChatEntry::Assistant(entry)) = view.chat.get_mut(index) {
                entry.error = Some(error.text);
                entry.aborted = error.aborted;
                view.mark_entry_stale(index);
                return;
            }
        }
        view.push_entry(ChatEntry::Assistant(Box::new(
            crate::chat::AssistantMessage {
                blocks: Vec::new(),
                has_tool_calls: false,
                streaming: false,
                error: Some(error.text),
                aborted: error.aborted,
            },
        )));
    }

    /// Attach a (partial or final) tool result to the matching card. Returns whether the result
    /// landed (a card exists and is not aborted): the caller's loader work rides the same gate.
    fn apply_tool_result(
        &mut self,
        tool_call_id: &str,
        result: &Value,
        is_error: bool,
        partial: bool,
        view: &mut AgentView,
    ) -> bool {
        let result = ToolResultView {
            content: result
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            details: result.get("details").cloned().unwrap_or(Value::Null),
            is_error,
        };
        // The result lands on the newest card carrying the id; the older settled
        // card keeps its sweep-written result.
        let card_index = view
            .chat
            .iter()
            .rposition(|entry| matches!(entry, ChatEntry::Tool(card) if card.id == tool_call_id));
        if let Some(index) = card_index {
            view.prepare_entry_mutation(index);
            if let Some(ChatEntry::Tool(card)) = view.chat.get_mut(index) {
                // The failed frame's sweep owns the call: late result frames land on nothing.
                if self.aborted_tools.contains(tool_call_id) || card.aborted {
                    return false;
                }
                card.result = Some(result);
                card.result_partial = partial;
                if !partial {
                    card.ended_at = Some(std::time::Instant::now());
                    self.pending_tools.remove(tool_call_id);
                    self.track_ipython_bash_rendered(card);
                }
                view.mark_entry_stale(index);
                return true;
            }
        }
        false
    }

    /// Count one live-line feedback moment (a stall shown) with the session's other interaction
    /// counters; delivery is the telemetry worker's.
    fn track_turn_feedback(&self, feedback: crate::interactive::TurnFeedback) {
        let Some(telemetry) = self.telemetry.clone() else {
            return;
        };
        tokio::spawn(async move {
            telemetry.turn_feedback_shown(feedback).await;
        });
    }

    /// Count one menu interaction (see [`crate::interactive::MenuUse`]) with the session's other
    /// interaction counters; delivery is the telemetry worker's.
    pub(super) fn track_menu_use(&self, used: crate::interactive::MenuUse) {
        let Some(telemetry) = self.telemetry.clone() else {
            return;
        };
        tokio::spawn(async move {
            telemetry.menu_used(used).await;
        });
    }

    /// Count live timer starts with the session's other interaction counters.
    fn track_live_tool_timer(&self) {
        let Some(telemetry) = self.telemetry.clone() else {
            return;
        };
        tokio::spawn(async move {
            telemetry.live_tool_timer_started().await;
        });
    }

    /// `tui ipython bash rendered`: the settled kernel cell card that renders
    /// as bash reports its bash share, primitives only.
    fn track_ipython_bash_rendered(&self, card: &crate::tool_card::ToolCallCard) {
        if !pa_types::ai::is_kernel_tool_name(&card.name) {
            return;
        }
        let Some(stats) = crate::tool_card::ipython::bash_dominated_stats(card) else {
            return;
        };
        let Some(telemetry) = self.telemetry.clone() else {
            return;
        };
        tokio::spawn(async move {
            telemetry
                .ipython_bash_rendered(stats.bash_lines, stats.cell_lines, stats.count)
                .await;
        });
    }

    /// Update the loader activity label (agent-activity tracker subset).
    fn set_working_activity(activity: &'static str, download: bool, view: &mut AgentView) {
        if let Some(working) = &mut view.working {
            working.activity = activity;
            working.download = download;
        }
    }
}
