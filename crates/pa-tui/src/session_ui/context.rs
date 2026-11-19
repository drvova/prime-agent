//! The context window's background fetch and keys. The window opens at once over the tray's own
//! state (and the last landed breakdown); its one `get_context_breakdown` request is spawned and
//! its reply lands through the run loop's channel, so a slow daemon never holds a frame (AGENTS.md
//! "Performance and the critical path").

use pa_types::context_breakdown::ContextBreakdown;

use super::{key_event_to_id, AgentView, DaemonCommand, Duration, KeyEvent, Map, SessionUi};
use crate::context_view::{ContextView, ContextViewAction, ContextViewSource};

/// The request bound: the reply is in-memory work on the worker, but a session whose loop is not
/// built yet builds it first.
const CONTEXT_REQUEST_TIMEOUT_MS: u64 = 15_000;

/// A landed breakdown, tagged with the request generation that issued it.
#[derive(Debug)]
pub(crate) struct ContextUpdate {
    generation: u64,
    result: Result<ContextBreakdown, String>,
}

impl SessionUi {
    /// Open the window over the last landed breakdown and fetch a fresh one in the background.
    pub(crate) fn open_context_view(&mut self, view: &mut AgentView, source: ContextViewSource) {
        // A draft's pending completion must not materialize over the window on the next tick.
        view.editor.cancel_autocomplete();
        view.context_view = Some(ContextView::new(source, self.context_breakdown.clone()));
        self.context_generation += 1;
        let generation = self.context_generation;
        let client = self.client.clone();
        let updates = self.context_updates.clone();
        let request = DaemonCommand::GetContextBreakdown {
            id: None,
            active_session_id: self.active_session_id.clone(),
            rest: Map::default(),
        };
        tokio::spawn(async move {
            let result = match tokio::time::timeout(
                Duration::from_millis(CONTEXT_REQUEST_TIMEOUT_MS),
                client.request_ok(request),
            )
            .await
            {
                Ok(Ok(data)) => serde_json::from_value(data).map_err(|error| error.to_string()),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(_) => Err("the daemon did not answer in time".to_string()),
            };
            let _ = updates.send(ContextUpdate { generation, result });
        });
        self.dirty = true;
    }

    /// Fold a landed breakdown: a success refreshes the cache whatever its generation; the open
    /// window takes only its own generation's reply.
    pub(crate) fn apply_context_update(&mut self, update: ContextUpdate, view: &mut AgentView) {
        if let Ok(breakdown) = &update.result {
            self.context_breakdown = Some(breakdown.clone());
        }
        if let Some(window) = view
            .context_view
            .as_mut()
            .filter(|_| update.generation == self.context_generation)
        {
            window.apply_breakdown(update.result);
        }
        self.dirty = true;
    }

    /// One key press while the window is open; the close emits the adoption event off the key
    /// path.
    pub(super) fn handle_context_view_key(&mut self, key: KeyEvent, view: &mut AgentView) {
        let Some(id) = key_event_to_id(&key) else {
            return;
        };
        let Some(window) = view.context_view.as_mut() else {
            return;
        };
        match window.handle_key(&id, view.editor.keybindings()) {
            ContextViewAction::None => {}
            ContextViewAction::Close => {
                let summary = window.summary();
                view.context_view = None;
                if let Some(telemetry) = self.telemetry.clone() {
                    tokio::spawn(async move {
                        telemetry.context_view_closed(summary).await;
                    });
                }
            }
        }
        self.dirty = true;
    }
}
