//! The terminal attention signal. A kernel `notify(...)` ping raises one: an OSC 9 desktop
//! notification where the user turned terminal indicators on (`showTerminalProgress`, the
//! OSC 9 family the progress bar already speaks), else the terminal bell, which every terminal
//! maps to its own attention behavior (a tab badge, a dock bounce, a sound). A run that ends, or
//! a question that opens, while the terminal is unfocused raises the one the user chose
//! (`terminal.notifyOnTurnEnd`: `off`, `bell`, `notification`). The signal rides the next frame's
//! single write, so it never adds a write of its own or blocks a paint.
//!
//! Focus is what the terminal reports (mode `?1004`: `CSI I` on focus, `CSI O` on blur). The mode
//! is requested only while the setting asks for a signal, in the same frame write, and released
//! with the surface. A terminal that never reports focus (the mode unsupported, or tmux without
//! `focus-events on`) reads as focused, so it never signals.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::view::AgentView;

const ENABLE_FOCUS_REPORTS: &[u8] = b"\x1b[?1004h";
const DISABLE_FOCUS_REPORTS: &[u8] = b"\x1b[?1004l";

/// Whether the terminal was asked for focus reports (process-wide, like the mouse tracking
/// state: the exit restore releases the mode without a view).
static FOCUS_REPORTS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// How the terminal is asked for attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attention {
    /// OSC 9: a desktop notification carrying the text.
    Notification(String),
    /// BEL.
    Bell,
}

/// The `terminal.notifyOnTurnEnd` setting: what an unfocused terminal is sent when the agent
/// stops for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEndSignal {
    Off,
    Bell,
    Notification,
}

impl TurnEndSignal {
    /// The setting's wire name; an unknown value reads as `off`.
    pub(crate) fn from_wire_name(name: &str) -> Self {
        match name {
            "bell" => TurnEndSignal::Bell,
            "notification" => TurnEndSignal::Notification,
            _ => TurnEndSignal::Off,
        }
    }

    /// The setting's wire name, also the telemetry `mode`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            TurnEndSignal::Off => "off",
            TurnEndSignal::Bell => "bell",
            TurnEndSignal::Notification => "notification",
        }
    }
}

/// Why the agent stopped for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waiting {
    /// The run ended.
    TurnEnded,
    /// A question prompt opened.
    Question,
}

impl Waiting {
    /// The telemetry `reason`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Waiting::TurnEnded => "turn_end",
            Waiting::Question => "question",
        }
    }
}

impl Attention {
    /// The signal for one ping: `osc_indicators` is the `showTerminalProgress` setting.
    pub(crate) fn for_ping(message: &str, osc_indicators: bool) -> Self {
        if osc_indicators {
            // Control characters dropped, so the text can never end the sequence early or carry
            // an escape of its own.
            Attention::Notification(message.chars().filter(|c| !c.is_control()).collect())
        } else {
            Attention::Bell
        }
    }

    /// The signal for the agent stopping for the user: none while the terminal is focused or the
    /// setting is `off`. The notification text is fixed, never session content.
    pub(crate) fn for_waiting(
        waiting: Waiting,
        signal: TurnEndSignal,
        focused: bool,
    ) -> Option<Self> {
        if focused {
            return None;
        }
        match signal {
            TurnEndSignal::Off => None,
            TurnEndSignal::Bell => Some(Attention::Bell),
            TurnEndSignal::Notification => Some(Attention::Notification(
                match waiting {
                    Waiting::TurnEnded => "Prime Agent finished its turn",
                    Waiting::Question => "Prime Agent is waiting for your answer",
                }
                .to_string(),
            )),
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        match self {
            Attention::Notification(text) => format!("\x1b]9;{text}\x07").into_bytes(),
            Attention::Bell => b"\x07".to_vec(),
        }
    }
}

/// Hand one signal's report to the telemetry seam and return at once: the event path that
/// queued the signal never waits on the delivery.
pub(crate) fn report_signal(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    waiting: Waiting,
    signal: TurnEndSignal,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry
                .unfocused_signal_sent(waiting.wire_name(), signal.wire_name())
                .await;
        });
    }
}

/// Queue the focus-report mode change the view asks for and its pending signal into this frame;
/// whether anything was written.
pub(crate) fn flush(out: &mut impl Write, view: &mut AgentView) -> std::io::Result<bool> {
    let focus_reports = view.turn_end_signal != TurnEndSignal::Off;
    let mode_changed = FOCUS_REPORTS_ACTIVE.swap(focus_reports, Ordering::SeqCst) != focus_reports;
    if mode_changed {
        out.write_all(if focus_reports {
            ENABLE_FOCUS_REPORTS
        } else {
            DISABLE_FOCUS_REPORTS
        })?;
    }
    let Some(attention) = view.pending_attention.take() else {
        return Ok(mode_changed);
    };
    out.write_all(&attention.bytes())?;
    Ok(true)
}

/// Stop the focus reports as the surface hands the terminal on (a suspend, a teardown, the exit
/// restore); a no-op when the mode was never requested. The next surface's first frame asks
/// again.
pub(crate) fn release_focus_reports(out: &mut impl Write) -> std::io::Result<()> {
    if FOCUS_REPORTS_ACTIVE.swap(false, Ordering::SeqCst) {
        out.write_all(DISABLE_FOCUS_REPORTS)?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{flush, release_focus_reports, Attention, TurnEndSignal, Waiting};
    use crate::theme::{ColorMode, Theme};
    use crate::view::AgentView;

    /// The OSC 9 text never carries a control character, and indicators off rings the bell.
    #[test]
    fn a_ping_signals_osc9_or_the_bell() {
        assert_eq!(
            Attention::for_ping("build \x1b]0;x\x07done", true).bytes(),
            b"\x1b]9;build ]0;xdone\x07".to_vec()
        );
        assert_eq!(Attention::for_ping("done", false), Attention::Bell);
        assert_eq!(Attention::Bell.bytes(), b"\x07".to_vec());
    }

    /// Every setting value against both focus states and both reasons: only an unfocused
    /// terminal is signalled, with the bell or the fixed OSC 9 text the setting names.
    #[test]
    fn the_agent_stopping_signals_an_unfocused_terminal_per_the_setting() {
        let signals: Vec<Option<Vec<u8>>> = ["off", "bell", "notification", "unknown"]
            .into_iter()
            .flat_map(|name| {
                [
                    (Waiting::TurnEnded, false),
                    (Waiting::Question, false),
                    (Waiting::TurnEnded, true),
                    (Waiting::Question, true),
                ]
                .map(|(waiting, focused)| {
                    Attention::for_waiting(waiting, TurnEndSignal::from_wire_name(name), focused)
                        .map(|attention| attention.bytes())
                })
            })
            .collect();
        let bell = Some(b"\x07".to_vec());
        assert_eq!(
            signals,
            vec![
                None,
                None,
                None,
                None,
                bell.clone(),
                bell,
                None,
                None,
                Some(b"\x1b]9;Prime Agent finished its turn\x07".to_vec()),
                Some(b"\x1b]9;Prime Agent is waiting for your answer\x07".to_vec()),
                None,
                None,
                None,
                None,
                None,
                None,
            ]
        );
    }

    /// The focus-report mode is written once when the view starts asking, once when it stops,
    /// and the release leaves nothing for a later frame to undo. The only test that touches the
    /// process-wide mode flag.
    #[test]
    fn the_focus_report_mode_follows_the_view_in_the_frame_write() {
        let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
        let mut out: Vec<u8> = Vec::new();
        assert!(!flush(&mut out, &mut view).expect("flush"));
        view.turn_end_signal = TurnEndSignal::Bell;
        view.pending_attention = Some(Attention::Bell);
        assert!(flush(&mut out, &mut view).expect("flush"));
        assert!(!flush(&mut out, &mut view).expect("flush"));
        view.turn_end_signal = TurnEndSignal::Off;
        assert!(flush(&mut out, &mut view).expect("flush"));
        view.turn_end_signal = TurnEndSignal::Notification;
        assert!(flush(&mut out, &mut view).expect("flush"));
        release_focus_reports(&mut out).expect("release");
        release_focus_reports(&mut out).expect("release");
        assert_eq!(
            out,
            b"\x1b[?1004h\x07\x1b[?1004l\x1b[?1004h\x1b[?1004l".to_vec()
        );
    }
}
