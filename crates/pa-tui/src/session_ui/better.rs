//! `/better <prompt>`: the daemon's helper rewrites the prompt in the background (the
//! side-question lane: one run per client, Esc cancels it), its progress rides the status row,
//! and the rewrite lands in the composer above any draft, unsent, for the user to read and edit.
//! What the user then does with it (sent as is, edited, discarded) goes back to the session's
//! counters fire-and-forget.
use super::{AgentView, DaemonCommand, Duration, Map, Result, SessionUi, StatusKind, Value};

/// The notice once the rewrite is in the composer.
pub(crate) const BETTER_READY_NOTICE: &str =
    "Improved prompt is in your input box; edit it, then press Enter to send.";
/// The notice once Esc cancelled a run.
pub(crate) const BETTER_CANCELLED_NOTICE: &str = "/better cancelled; nothing was sent.";

/// A rewrite the composer holds until the user sends something or replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InsertedRewrite {
    rewrite: String,
    /// The whole composer right after the insertion.
    composer: String,
}

impl SessionUi {
    /// A notice's tail naming the key that leaves the current side run, from the effective
    /// `app.input.clear` binding (` Press Esc to return to the main thread.`, ` (Esc cancels)`):
    /// `wording` receives the key's display text. Empty when the user unbound the key, so a
    /// notice never names a key that does nothing.
    pub(super) fn clear_key_tail(&self, wording: impl Fn(&str) -> String) -> String {
        self.keybindings
            .first_key("app.input.clear")
            .map(|key| wording(&crate::keybindings::format_key_text(&key)))
            .unwrap_or_default()
    }

    /// Start one run; the composer stays usable while it runs.
    pub(super) async fn start_better(&mut self, prompt: &str, view: &mut AgentView) -> Result<()> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            self.note_as("Usage: /better <prompt>", StatusKind::Warning, view);
            return Ok(());
        }
        if self.better_run.is_some() {
            let cancel = self.clear_key_tail(|key| format!("; press {key} to cancel it"));
            self.note_as(
                &format!("A /better run is already improving a prompt{cancel}."),
                StatusKind::Warning,
                view,
            );
            return Ok(());
        }
        if self.active_side_question_id.is_some() {
            self.note_as(
                "Wait for the current side question to finish or cancel it first.",
                StatusKind::Warning,
                view,
            );
            return Ok(());
        }
        self.side_question_counter += 1;
        let id = format!(
            "better-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or_default(),
            self.side_question_counter
        );
        self.better_run = Some(id.clone());
        let cancel = self.clear_key_tail(|key| format!(" ({key} cancels)"));
        self.note(
            &format!("Improving your prompt: reading the code{cancel}"),
            view,
        );
        let started = self
            .bounded_request(
                Duration::from_millis(super::UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::StartSideQuestion {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    side_question_id: id,
                    question: prompt.to_string(),
                    previous_turns: None,
                    kind: Some(pa_types::better::SideRunKind::Better),
                    rest: Map::default(),
                },
            )
            .await;
        if let Err(error) = started {
            self.better_run = None;
            self.error_row(&format!("/better could not start: {error:#}"), view);
        }
        Ok(())
    }

    /// Route one `side_question_event` of the running `/better`; `false` when it is not ours.
    pub(super) fn apply_better_event(&mut self, event: &Value, view: &mut AgentView) -> bool {
        let id = event.get("id").and_then(Value::as_str).unwrap_or_default();
        if self.better_run.as_deref() != Some(id) {
            return false;
        }
        let text = |key: &str| {
            event
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match text("status").as_str() {
            "running" => {
                let reading = text("answer");
                if !reading.is_empty() {
                    let cancel = self.clear_key_tail(|key| format!(" ({key} cancels)"));
                    self.note(&format!("Improving your prompt: {reading}{cancel}"), view);
                }
            }
            "complete" => {
                self.better_run = None;
                self.insert_rewrite(&text("answer"), view);
                self.note(BETTER_READY_NOTICE, view);
            }
            "cancelled" => {
                self.better_run = None;
                self.note(BETTER_CANCELLED_NOTICE, view);
            }
            _ => {
                self.better_run = None;
                self.error_row(&format!("/better failed: {}", text("errorMessage")), view);
            }
        }
        self.dirty = true;
        true
    }

    /// The rewrite goes above whatever the user typed meanwhile; nothing is sent.
    fn insert_rewrite(&mut self, rewrite: &str, view: &mut AgentView) {
        // A rewrite still waiting in the composer is replaced: it was never sent.
        self.settle_inserted_rewrite(None);
        let draft = view.editor.get_text();
        let composer = if draft.trim().is_empty() {
            rewrite.to_string()
        } else {
            format!("{rewrite}\n\n{draft}")
        };
        view.editor.set_text(&composer);
        self.better_inserted = Some(InsertedRewrite {
            rewrite: rewrite.to_string(),
            composer,
        });
    }

    /// Esc while a run is in flight: abort it (fire-and-forget) and say nothing was sent.
    /// `false` when no run is in flight (the Esc ladder carries on).
    pub(super) fn cancel_better(&mut self, view: &mut AgentView) -> bool {
        let Some(side_question_id) = self.better_run.take() else {
            return false;
        };
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let _ = client
                .request_ok(DaemonCommand::AbortSideQuestion {
                    id: None,
                    active_session_id,
                    side_question_id,
                    rest: Map::default(),
                })
                .await;
        });
        self.note(BETTER_CANCELLED_NOTICE, view);
        true
    }

    /// The user sent `submitted` (or the rewrite left the composer unsent, `None`): report what
    /// became of a pending rewrite, fire-and-forget.
    pub(super) fn settle_inserted_rewrite(&mut self, submitted: Option<&str>) {
        let Some(inserted) = self.better_inserted.take() else {
            return;
        };
        let outcome = pa_types::better::classify_composer_outcome(
            &inserted.rewrite,
            &inserted.composer,
            submitted,
        );
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let _ = client
                .request_ok(DaemonCommand::RecordBetterOutcome {
                    id: None,
                    active_session_id,
                    outcome,
                    rest: Map::default(),
                })
                .await;
        });
    }
}
