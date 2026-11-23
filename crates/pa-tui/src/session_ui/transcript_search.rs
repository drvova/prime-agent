//! The session's side of the transcript search bar (`view/search.rs`): opening it from the
//! viewport keys, its keys while it owns the dock, the run loop's index tick, the scope of what
//! the window has loaded, and the adoption report when it closes.

use super::{key_event_to_id, AgentView, KeyEvent, Result, SessionUi};
use crate::view::search::{HistoryScope, SearchKey};

impl SessionUi {
    /// What the bar searches: the whole session, or only the rows the transcript window holds
    /// while older pages live in the daemon.
    fn transcript_search_scope(&self) -> HistoryScope {
        if self.window.incomplete() {
            HistoryScope::LoadedOnly
        } else {
            HistoryScope::Complete
        }
    }

    /// `app.transcript.search`: open the bar over the transcript.
    pub(super) fn open_transcript_search(&mut self, view: &mut AgentView) {
        view.editor.clear_selection();
        // Scroll-up rows still owed to an older page are dropped: the bar remembers this row.
        self.window.scroll(0, view);
        view.open_transcript_search(self.transcript_search_scope());
        view.continue_transcript_search();
        self.dirty = true;
    }

    /// One key while the bar is open. A close reports `tui transcript search` the first time
    /// per outcome in the run, fire-and-forget.
    pub(super) fn handle_transcript_search_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        view.set_transcript_search_history(self.transcript_search_scope());
        match view.transcript_search_key(&id) {
            Some(SearchKey::Handled) | None => {}
            Some(SearchKey::Closed { summary, to_bottom }) => {
                if to_bottom {
                    self.collapse_transcript_window(view);
                }
                let outcome = summary.outcome;
                if !self.search_reported.contains(&outcome) {
                    self.search_reported.push(outcome);
                    crate::view::search::report_closed(self.telemetry.as_ref(), summary);
                }
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// The run loop's tick while the open bar still has index work: one slice.
    pub(crate) fn continue_transcript_search(&mut self, view: &mut AgentView) {
        view.set_transcript_search_history(self.transcript_search_scope());
        if view.continue_transcript_search() {
            self.dirty = true;
        }
    }
}
