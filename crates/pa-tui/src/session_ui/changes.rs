//! The session changes panel's session side: opening it, routing its keys, attaching a diff to
//! the composer, and its adoption report.

use super::{key_event_to_id, AgentView, KeyEvent, SessionUi};
use crate::session_changes::{ChangesKey, ChangesPanel, LoadedHistory};

impl SessionUi {
    /// Open the session changes panel (`/diff`, `app.changes.open`) on this turn's changes.
    pub(super) fn open_changes_panel(&mut self, view: &mut AgentView) {
        view.editor.cancel_autocomplete();
        view.changes_panel = Some(ChangesPanel::new(if self.window.incomplete() {
            LoadedHistory::Partial
        } else {
            LoadedHistory::Whole
        }));
        self.subagents_focused = false;
        self.update_subagent_summary(view);
        self.track_changes("open", view);
        self.dirty = true;
    }

    /// One key press while the panel is open: it owns every key; an attach puts the file's diff
    /// in the composer (the editor's paste path, so a long diff is one `[paste #N]` marker) and
    /// closes the panel.
    pub(super) fn handle_changes_panel_key(&mut self, key: KeyEvent, view: &mut AgentView) {
        let Some(id) = key_event_to_id(&key) else {
            return;
        };
        let Some(panel) = view.changes_panel.as_mut() else {
            return;
        };
        let outcome = panel.handle_key(
            &id,
            view.editor.keybindings(),
            &mut view.change_ledger,
            &view.chat,
        );
        self.dirty = true;
        match outcome {
            ChangesKey::Handled => {}
            ChangesKey::Close => view.changes_panel = None,
            ChangesKey::Scoped => self.track_changes("scope", view),
            ChangesKey::OpenedDiff => self.track_changes("diff", view),
            ChangesKey::Attach(path) => {
                let text = view.changes_panel.as_ref().and_then(|panel| {
                    panel.attachment(&mut view.change_ledger, &view.chat, &path, &view.chrome.cwd)
                });
                let Some(text) = text else {
                    return;
                };
                self.track_changes("attach", view);
                view.changes_panel = None;
                view.editor.handle_paste(&text);
                let name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
                self.toast(&format!("Attached the diff of {name}"), view);
            }
        }
    }

    /// Report a panel action the first time it happens in the run (`tui session changes`), with
    /// the open scope's counts, fire-and-forget so the keypress never waits on the delivery.
    fn track_changes(&mut self, action: &'static str, view: &mut AgentView) {
        if self.changes_reported.contains(&action) {
            return;
        }
        let Some(panel) = view.changes_panel.as_ref() else {
            return;
        };
        self.changes_reported.push(action);
        let (files, untested) = panel.counts(&mut view.change_ledger, &view.chat);
        crate::session_changes::report_used(self.telemetry.as_ref(), action, files, untested);
    }
}
