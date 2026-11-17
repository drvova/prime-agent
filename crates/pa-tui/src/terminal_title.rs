//! The terminal window title (OSC 0): it carries the session's state so a background tab or
//! window reads it at a glance — `<session> · prime` idle, the shared working spinner glyph in
//! front while a turn or compaction runs, and `!` when a confirm waits on the user. The title is
//! written from the frame paint only when its text changes, so it ticks on the existing spinner
//! repaints and never while idle. The terminal's own title is pushed onto its title stack before
//! the first write and popped at exit (XTWINOPS 22/23; terminals without a stack ignore both).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::view::AgentView;

/// Push the window title onto the terminal's title stack (XTWINOPS 22;2).
const TITLE_PUSH: &[u8] = b"\x1b[22;2t";
/// Pop it back (XTWINOPS 23;2).
const TITLE_POP: &[u8] = b"\x1b[23;2t";

/// Whether this process pushed the terminal's title: the exit restore pops only then.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// What the title says about the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleState {
    Idle,
    /// A turn or compaction runs: the title leads with the shared spinner's current glyph.
    Working(&'static str),
    /// A confirm waits on the user.
    NeedsYou,
}

impl TitleState {
    /// The view's state: a waiting confirm outranks a running turn.
    pub(crate) fn of(view: &AgentView) -> Self {
        if view.confirm.is_some() {
            return TitleState::NeedsYou;
        }
        if view.working.is_some() || view.compaction.is_some() {
            let frames = crate::chat::LOADER_FRAMES;
            return TitleState::Working(frames[view.pulse_frame % frames.len()]);
        }
        TitleState::Idle
    }
}

/// The title text: the session name (control characters dropped, so a name can never carry an
/// escape sequence of its own) behind the state's lead.
pub(crate) fn compose(session: &str, state: TitleState) -> String {
    let session: String = session.chars().filter(|c| !c.is_control()).collect();
    let session = if session.trim().is_empty() {
        "Prime Agent".to_string()
    } else {
        session
    };
    match state {
        TitleState::Idle => format!("{session} \u{b7} prime"),
        TitleState::Working(glyph) => format!("{glyph} {session} \u{b7} prime"),
        TitleState::NeedsYou => format!("! {session} \u{b7} prime"),
    }
}

/// Queue the title for this frame when its text changed since the last write.
pub(crate) fn sync(out: &mut impl Write, view: &mut AgentView) -> std::io::Result<()> {
    let title = compose(&view.chrome.chat_name, TitleState::of(view));
    if view.terminal_title.as_deref() == Some(title.as_str()) {
        return Ok(());
    }
    if !TITLE_PUSHED.swap(true, Ordering::SeqCst) {
        out.write_all(TITLE_PUSH)?;
    }
    crossterm::queue!(out, crossterm::terminal::SetTitle(&title))?;
    view.terminal_title = Some(title);
    Ok(())
}

/// Hand the terminal its own title back at exit (a no-op when this process never set one).
pub(crate) fn restore(out: &mut impl Write) {
    if TITLE_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(TITLE_POP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_carries_the_session_state() {
        let titles = [
            TitleState::Idle,
            TitleState::Working("\u{280b}"),
            TitleState::NeedsYou,
        ]
        .map(|state| compose("fix the parser\x1b]0;x\x07", state));
        assert_eq!(
            titles,
            [
                "fix the parser]0;x \u{b7} prime".to_string(),
                "\u{280b} fix the parser]0;x \u{b7} prime".to_string(),
                "! fix the parser]0;x \u{b7} prime".to_string(),
            ]
        );
    }

    #[test]
    fn the_view_state_ranks_a_waiting_confirm_over_a_running_turn() {
        let mut view = AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ));
        assert_eq!(TitleState::of(&view), TitleState::Idle);
        view.working = Some(crate::chat::WorkingState {
            activity: "Working",
            message: None,
            download: false,
            tokens: 0,
            elapsed_secs: 0,
        });
        view.pulse_frame = 3;
        assert_eq!(
            TitleState::of(&view),
            TitleState::Working(crate::chat::LOADER_FRAMES[3])
        );
        view.confirm = Some(crate::confirm::ConfirmPanel::yes_no("Allow?", "run it"));
        assert_eq!(TitleState::of(&view), TitleState::NeedsYou);
        // The title is written once per change: a repaint with the same state writes nothing.
        let mut out: Vec<u8> = Vec::new();
        sync(&mut out, &mut view).expect("first write");
        let written = out.len();
        sync(&mut out, &mut view).expect("unchanged repaint");
        assert!(written > 0);
        assert_eq!(out.len(), written);
    }
}
