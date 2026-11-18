//! Scroll-region repaint for a following transcript: when the transcript
//! window scrolled by whole rows since the last painted frame, the terminal
//! shifts the region itself (DECSTBM margins plus SU) and the cell diff then
//! repaints only the rows that differ from the shifted screen.
//!
//! Correctness never rests on the shift guess. The painter mirrors the
//! terminal's operation onto ratatui's back buffer (rows move up inside the
//! margins, the vacated rows become blank default cells), so the ordinary
//! diff that follows corrects anything the shift did not predict; a wrong
//! guess only costs bytes. The margins are reset in the same write that sets
//! them, so no exit path can leave a scroll region behind.

use std::io::{self, Write};
use std::sync::Mutex;

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::Terminal;

use crate::frame_output::PaintStamp;

/// The frame rows the transcript window occupies (`top..bottom`), reported
/// by the view only while the window follows the tail with no selection or
/// overlay over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollRegion {
    pub(crate) top: u16,
    pub(crate) bottom: u16,
}

/// The last painted buffer, as ratatui's back buffer holds it.
struct Shadow {
    buffer: Buffer,
    region: ScrollRegion,
    /// The frame that painted it: only the immediately next frame through
    /// the same writer may shift against it.
    stamp: PaintStamp,
}

/// Owns the shadow of the last painted frame for one terminal surface.
pub(crate) struct ScrollPainter {
    shadow: Option<Shadow>,
}

/// The interactive surface's painter (`app::draw`).
pub(crate) static PAINTER: Mutex<ScrollPainter> = Mutex::new(ScrollPainter::new());

impl ScrollPainter {
    pub(crate) const fn new() -> Self {
        Self { shadow: None }
    }

    /// Forget the last frame: the screen was cleared or repainted outside
    /// the painter (a resume's full redraw).
    pub(crate) fn invalidate(&mut self) {
        self.shadow = None;
    }

    /// Paint `next` (a full-area buffer) as the frame `stamp` on `terminal`,
    /// shifting the region first when it scrolled. `next` becomes ratatui's
    /// current buffer and `paint` diffs it and finishes the frame (the
    /// flush, the cursor); the painter then keeps the painted buffer and
    /// swaps ratatui's buffers.
    pub(crate) fn draw<B: Backend + Write, T, E: From<io::Error>>(
        &mut self,
        terminal: &mut Terminal<B>,
        next: Buffer,
        region: Option<ScrollRegion>,
        stamp: PaintStamp,
        paint: impl FnOnce(&mut Terminal<B>) -> Result<T, E>,
    ) -> Result<T, E> {
        let shadow = self.shadow.take().filter(|shadow| {
            shadow.stamp.next() == stamp
                && Some(shadow.region) == region
                && shadow.buffer.area == next.area
        });
        if let Some(mut shadow) = shadow {
            if let Some(rows) = scrolled_rows(&shadow.buffer, &next, shadow.region) {
                let ScrollRegion { top, bottom } = shadow.region;
                // SGR reset first: the vacated rows fill with the default
                // background, the blank cell the back buffer gets below.
                write!(
                    terminal.backend_mut(),
                    "\x1b[0m\x1b[{};{bottom}r\x1b[{rows}S\x1b[r",
                    top + 1
                )?;
                shift_up(&mut shadow.buffer, shadow.region, rows);
                // The shifted shadow becomes ratatui's back buffer: the swap
                // makes the current buffer the previous one.
                *terminal.current_buffer_mut() = shadow.buffer;
                terminal.swap_buffers();
            }
        }
        *terminal.current_buffer_mut() = next;
        let painted = paint(terminal)?;
        self.shadow = region.map(|region| Shadow {
            buffer: terminal.current_buffer_mut().clone(),
            region,
            stamp,
        });
        terminal.swap_buffers();
        Ok(painted)
    }
}

fn row(buffer: &Buffer, y: u16) -> &[Cell] {
    let width = usize::from(buffer.area.width);
    let start = usize::from(y) * width;
    &buffer.content[start..start + width]
}

fn is_blank(cells: &[Cell]) -> bool {
    cells.iter().all(|cell| *cell == Cell::EMPTY)
}

/// Leading rows of the region that match when `next` is `previous` moved up
/// by `rows`, and whether any matched row has content.
fn matched_prefix(
    previous: &Buffer,
    next: &Buffer,
    region: ScrollRegion,
    rows: u16,
) -> (u16, bool) {
    let mut matched = 0;
    let mut content = false;
    for y in region.top..region.bottom.saturating_sub(rows) {
        let moved = row(previous, y + rows);
        if moved != row(next, y) {
            break;
        }
        content |= !is_blank(moved);
        matched += 1;
    }
    (matched, content)
}

/// The rows the region scrolled up by, when that is unambiguous: the shift
/// keeps at least half of the overlapping rows (and two at the least) in
/// place, beats painting in place, and the kept rows are not just blanks.
fn scrolled_rows(previous: &Buffer, next: &Buffer, region: ScrollRegion) -> Option<u16> {
    let height = region.bottom.saturating_sub(region.top);
    let (in_place, _) = matched_prefix(previous, next, region, 0);
    if in_place == height {
        return None;
    }
    (1..height).find(|&rows| {
        let (kept, content) = matched_prefix(previous, next, region, rows);
        content && kept > in_place && kept >= 2 && kept * 2 >= height - rows
    })
}

/// Mirror the terminal's scroll-up inside the margins onto `buffer`.
fn shift_up(buffer: &mut Buffer, region: ScrollRegion, rows: u16) {
    let width = usize::from(buffer.area.width);
    let top = usize::from(region.top) * width;
    let bottom = usize::from(region.bottom) * width;
    let moved = usize::from(rows) * width;
    buffer.content[top..bottom].rotate_left(moved);
    for cell in &mut buffer.content[bottom - moved..bottom] {
        *cell = Cell::EMPTY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;
    use ratatui::text::{Line, Text};
    use ratatui::widgets::Widget;
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::rc::Rc;

    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 20;
    const REGION: ScrollRegion = ScrollRegion { top: 1, bottom: 16 };

    /// The terminal stand-in: the bytes it received and how many writes carried them.
    #[derive(Clone, Default)]
    struct Sink {
        bytes: Rc<RefCell<Vec<u8>>>,
        writes: Rc<RefCell<usize>>,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.borrow_mut().extend_from_slice(buf);
            *self.writes.borrow_mut() += 1;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A top bar, a transcript window following a growing paragraph, and a
    /// dock whose counter changes every frame.
    fn frame(words: usize) -> Buffer {
        let mut text = String::new();
        for word in 0..words {
            write!(text, "w{word} ").unwrap();
        }
        let wrapped: Vec<String> = text
            .as_bytes()
            .chunks(usize::from(WIDTH))
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        let window = usize::from(REGION.bottom - REGION.top);
        let mut lines = vec![Line::from("prime agent - top bar")];
        lines.extend(
            wrapped[wrapped.len().saturating_sub(window)..]
                .iter()
                .map(|row| Line::from(row.clone())),
        );
        while lines.len() < usize::from(REGION.bottom) {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(format!("> composer ({words} words)")));
        let area = Rect::new(0, 0, WIDTH, HEIGHT);
        let mut buffer = Buffer::empty(area);
        Text::from(lines).render(area, &mut buffer);
        buffer
    }

    /// What the terminal shows, cell by cell: the glyph (a never-written
    /// cell reads as the blank it displays) and its colors and attributes.
    fn visible_cells(screen: &vt100::Screen) -> Vec<String> {
        (0..HEIGHT)
            .map(|row| {
                (0..WIDTH).fold(String::new(), |mut cells, col| {
                    let cell = screen.cell(row, col).expect("an on-screen cell");
                    let glyph = if cell.has_contents() {
                        cell.contents()
                    } else {
                        " ".to_string()
                    };
                    write!(
                        cells,
                        "{glyph}{:?}{:?}{}{}{}{}",
                        cell.fgcolor(),
                        cell.bgcolor(),
                        u8::from(cell.bold()),
                        u8::from(cell.italic()),
                        u8::from(cell.underline()),
                        u8::from(cell.inverse())
                    )
                    .unwrap();
                    cells
                })
            })
            .collect()
    }

    /// `CSI row;col H` sequences: the cursor moves the diff paid for.
    fn cursor_moves(bytes: &[u8]) -> usize {
        let text = String::from_utf8_lossy(bytes);
        text.split("\x1b[")
            .skip(1)
            .filter(|tail| {
                let end = tail.find(|c: char| !(c.is_ascii_digit() || c == ';'));
                end.is_some_and(|end| tail[end..].starts_with('H'))
            })
            .count()
    }

    /// Paint every frame the way `app::draw` does (through the hyperlink
    /// writer over the one-write frame buffer); returns the bytes written,
    /// the cursor moves, and the vt100 screen after each frame. Every frame,
    /// its region shift included, reaches the terminal as one write.
    fn paint(frames: &[Buffer], region: Option<ScrollRegion>) -> (usize, usize, Vec<Vec<String>>) {
        let sink = Sink::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(crate::hyperlinks::HyperlinkWriter::new(
                crate::frame_output::FrameWriter::new(sink.clone()),
            )),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, WIDTH, HEIGHT)),
            },
        )
        .unwrap();
        let mut painter = ScrollPainter::new();
        let mut parser = vt100::Parser::new(HEIGHT, WIDTH, 0);
        let mut screens = Vec::new();
        let mut bytes = 0;
        let mut moves = 0;
        for next in frames {
            let writer = terminal.backend_mut().writer_mut().inner_mut();
            writer.begin();
            let stamp = writer.stamp();
            painter
                .draw(&mut terminal, next.clone(), region, stamp, |terminal| {
                    terminal.flush()?;
                    Backend::flush(terminal.backend_mut())
                })
                .unwrap();
            terminal
                .backend_mut()
                .writer_mut()
                .inner_mut()
                .commit()
                .unwrap();
            assert!(
                std::mem::take(&mut *sink.writes.borrow_mut()) <= 1,
                "a frame is one write"
            );
            let written = std::mem::take(&mut *sink.bytes.borrow_mut());
            bytes += written.len();
            moves += cursor_moves(&written);
            parser.process(&written);
            screens.push(visible_cells(parser.screen()));
        }
        (bytes, moves, screens)
    }

    #[test]
    fn a_scrolling_stream_shifts_the_region_and_paints_the_plain_diff_screen() {
        let frames: Vec<Buffer> = (1..=400).step_by(3).map(frame).collect();
        let (plain_bytes, plain_moves, plain_screens) = paint(&frames, None);
        let (region_bytes, region_moves, region_screens) = paint(&frames, Some(REGION));
        // Every frame's screen is exactly the plain diff's screen.
        for (index, (region, plain)) in region_screens.iter().zip(&plain_screens).enumerate() {
            assert_eq!(region, plain, "frame {index}");
        }
        // The plain diff rewrites the whole window on every scrolled frame;
        // the shifted region repaints only the new rows.
        assert!(
            region_bytes * 3 < plain_bytes,
            "region {region_bytes} bytes vs plain {plain_bytes} bytes"
        );
        assert!(
            region_moves * 3 < plain_moves,
            "region {region_moves} cursor moves vs plain {plain_moves}"
        );
    }
}
