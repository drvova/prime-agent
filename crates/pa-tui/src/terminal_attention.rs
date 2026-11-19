//! The terminal attention signal a kernel `notify(...)` ping raises: an OSC 9 desktop
//! notification where the user turned terminal indicators on (`showTerminalProgress`, the
//! OSC 9 family the progress bar already speaks), else the terminal bell, which every terminal
//! maps to its own attention behavior (a tab badge, a dock bounce, a sound). The signal rides
//! the next frame's single write, so a ping never adds a write of its own or blocks a paint.

use std::io::Write;

use crate::view::AgentView;

/// How the terminal is asked for attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attention {
    /// OSC 9: a desktop notification carrying the text.
    Notification(String),
    /// BEL.
    Bell,
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

    fn bytes(&self) -> Vec<u8> {
        match self {
            Attention::Notification(text) => format!("\x1b]9;{text}\x07").into_bytes(),
            Attention::Bell => b"\x07".to_vec(),
        }
    }
}

/// Queue the pending signal into this frame; whether one was written.
pub(crate) fn flush(out: &mut impl Write, view: &mut AgentView) -> std::io::Result<bool> {
    let Some(attention) = view.pending_attention.take() else {
        return Ok(false);
    };
    out.write_all(&attention.bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::Attention;

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
}
