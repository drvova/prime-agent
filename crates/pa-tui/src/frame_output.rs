//! The chat surface's per-frame terminal output: one frame's bytes (the synchronized-output
//! bracket, the cell diff, the cursor update) collect in memory and reach the terminal in a
//! single write, and a frame that changes nothing on screen writes nothing (omp's one
//! `terminal.write` per frame, `packages/tui/src/tui.ts`).

use std::io::{self, Write};

/// The chat paint backend: the OSC 8 hyperlink writer over the per-frame buffer over stdout.
pub(crate) type ChatBackend =
    ratatui::backend::CrosstermBackend<crate::hyperlinks::HyperlinkWriter<FrameWriter<io::Stdout>>>;

/// The chat paint backend over stdout.
pub(crate) fn stdout_chat_backend() -> ChatBackend {
    ChatBackend::new(crate::hyperlinks::HyperlinkWriter::new(FrameWriter::new(
        io::stdout(),
    )))
}

/// A writer that holds one frame's bytes between [`FrameWriter::begin`] and
/// [`FrameWriter::commit`]: flushes inside the frame are deferred, and the commit hands the
/// whole frame to the sink as one `write_all` plus one flush. Outside a frame, writes pass
/// straight through (the suspend and resume paths write their own sequences).
///
/// It also remembers what the terminal was last told about the hardware cursor, so a frame
/// re-sends the visibility and the caret position only when they change.
pub(crate) struct FrameWriter<W> {
    inner: W,
    buffer: Vec<u8>,
    open: bool,
    /// The cursor visibility the terminal last received; `None` when unknown (before the first
    /// frame, or after a handoff wrote its own sequences).
    pub(crate) cursor_visible: Option<bool>,
    /// The caret position the last frame left the cursor at.
    pub(crate) cursor_at: Option<(u16, u16)>,
}

impl<W: Write> FrameWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            open: false,
            cursor_visible: None,
            cursor_at: None,
        }
    }

    /// Start collecting one frame.
    pub(crate) fn begin(&mut self) {
        self.open = true;
    }

    /// The number of bytes the open frame holds.
    pub(crate) fn len(&self) -> usize {
        self.buffer.len()
    }

    /// The open frame's bytes from `from` on.
    pub(crate) fn since(&self, from: usize) -> &[u8] {
        &self.buffer[from.min(self.buffer.len())..]
    }

    /// Drop the open frame's bytes: the frame changed nothing on screen.
    pub(crate) fn discard(&mut self) {
        self.open = false;
        self.buffer.clear();
    }

    /// Send the open frame to the sink: one `write_all` and one flush (nothing when empty).
    pub(crate) fn commit(&mut self) -> io::Result<()> {
        self.open = false;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let written = self.inner.write_all(&self.buffer);
        self.buffer.clear();
        written?;
        self.inner.flush()
    }

    /// Forget the remembered cursor state: something outside the frame path (a suspend, an
    /// external editor) wrote its own cursor sequences.
    pub(crate) fn forget_cursor(&mut self) {
        self.cursor_visible = None;
        self.cursor_at = None;
    }

    /// The sink (test accessor).
    #[cfg(test)]
    pub(crate) fn sink(&self) -> &W {
        &self.inner
    }
}

impl<W: Write> Write for FrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.open {
            self.buffer.extend_from_slice(buf);
            Ok(buf.len())
        } else {
            self.inner.write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.open {
            Ok(())
        } else {
            self.inner.flush()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FrameWriter;
    use std::io::{self, Write};

    /// A terminal stand-in that counts every write and flush it receives.
    #[derive(Default)]
    struct CountingSink {
        writes: usize,
        flushes: usize,
        bytes: Vec<u8>,
    }

    impl Write for CountingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    type CountingTerminal = ratatui::Terminal<
        ratatui::backend::CrosstermBackend<
            crate::hyperlinks::HyperlinkWriter<FrameWriter<CountingSink>>,
        >,
    >;

    fn terminal() -> CountingTerminal {
        ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(crate::hyperlinks::HyperlinkWriter::new(
                FrameWriter::new(CountingSink::default()),
            )),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("a fixed-viewport terminal")
    }

    fn sink(terminal: &mut CountingTerminal) -> &CountingSink {
        terminal.backend_mut().writer_mut().inner_mut().sink()
    }

    /// One frame is one write and one flush (the synchronized-output bracket, the cell diff and
    /// the cursor update together), and a frame that changes nothing writes no byte at all.
    #[test]
    fn a_frame_is_one_write_and_an_unchanged_frame_writes_nothing() {
        let mut view = crate::view::AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ));
        view.set_terminal_rows(24);
        let mut terminal = terminal();
        crate::app::draw(&mut terminal, &mut view).expect("the first frame paints");
        let first = sink(&mut terminal);
        assert_eq!((first.writes, first.flushes), (1, 1));
        let bytes = first.bytes.len();
        assert!(bytes > 0, "the first frame paints the chrome");
        let first_frame = String::from_utf8_lossy(&first.bytes).into_owned();
        assert!(first_frame.starts_with("\u{1b}[?2026h"), "{first_frame:?}");
        assert!(first_frame.ends_with("\u{1b}[?2026l"), "{first_frame:?}");
        crate::app::draw(&mut terminal, &mut view).expect("the unchanged frame");
        let second = sink(&mut terminal);
        assert_eq!(
            (second.writes, second.flushes, second.bytes.len()),
            (1, 1, bytes),
            "an unchanged frame writes nothing"
        );
        view.editor.handle_input("x");
        crate::app::draw(&mut terminal, &mut view).expect("the keystroke frame");
        let third = sink(&mut terminal);
        assert_eq!((third.writes, third.flushes), (2, 2));
        let keystroke = String::from_utf8_lossy(&third.bytes[bytes..]).into_owned();
        assert!(
            !keystroke.contains("\u{1b}[?25l"),
            "the cursor hide is not re-sent while the cursor stays hidden: {keystroke:?}"
        );
    }
}
