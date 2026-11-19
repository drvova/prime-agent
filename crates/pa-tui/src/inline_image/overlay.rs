//! The pixel overlay: after a frame's cell diff, draw the terminal's graphics protocol over
//! the half-block cells of every fully visible image, and take back what the last frame drew
//! where the image moved or left.
//!
//! The overlay remembers what it drew (image, position, and the cells it covered). An image
//! is drawn again only when it is new, moved, or the diff rewrote cells under it (a cell-bound
//! protocol's pixels live in those cells, and a kitty placement in `WezTerm` attaches to them
//! too). A drawing that left its place is taken back per protocol: a kitty placement is
//! deleted by id (its data stays for a later placement), a sixel or iTerm2 picture is
//! overwritten by repainting the frame's own cells over its old rectangle. The frame paint
//! does not shift the scroll region while the overlay holds a drawing (see `app::draw`): a
//! terminal-side scroll would move pixels the overlay no longer knows the position of.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;

use super::store::{Payload, RenderKey, Rendered};
use super::{encode, ImageProtocol};

/// One image the frame shows whole: where its top-left cell sits on the screen.
#[derive(Debug, Clone)]
pub(crate) struct Placement {
    pub(crate) key: RenderKey,
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) rendered: Arc<Rendered>,
}

impl Placement {
    fn rect(&self) -> Rect {
        Rect::new(
            self.col,
            self.row,
            self.key.geometry.cols,
            self.key.geometry.rows,
        )
    }
}

/// One drawing the terminal shows.
struct Drawn {
    key: RenderKey,
    rect: Rect,
    /// The frame cells under the drawing when it was drawn.
    cells: Vec<Cell>,
}

/// The overlay's memory of one terminal surface.
#[derive(Default)]
pub(crate) struct ImageOverlay {
    drawn: Vec<Drawn>,
    area: Option<Rect>,
    /// The kitty image id per rendering, and the ids whose data the terminal holds.
    kitty_ids: HashMap<RenderKey, u32>,
    transmitted: HashSet<u32>,
}

/// The interactive surface's overlay (`app::draw`).
pub(crate) static OVERLAY: LazyLock<Mutex<ImageOverlay>> = LazyLock::new(Mutex::default);

fn rect_cells(buffer: &Buffer, rect: Rect) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                cells.push(cell.clone());
            }
        }
    }
    cells
}

impl ImageOverlay {
    /// Forget every drawing: the screen was cleared or repainted outside the frame path (a
    /// resume's full redraw), and the terminal may have dropped the kitty image data with it.
    pub(crate) fn invalidate(&mut self) {
        self.drawn.clear();
        self.area = None;
        self.transmitted.clear();
    }

    /// Take every drawing back before the surface hands the terminal on (a suspend, an exit,
    /// the agents-view handoff): kitty placements outlive the cells under them, so each is
    /// deleted; a cell-bound picture goes with the next surface's repaint. The memory resets
    /// as [`Self::invalidate`] does.
    ///
    /// # Errors
    ///
    /// Propagates the write error.
    pub(crate) fn release(&mut self, out: &mut impl Write) -> io::Result<()> {
        for drawn in &self.drawn {
            match drawn.key.protocol {
                ImageProtocol::Kitty => {
                    if let Some(id) = self.kitty_ids.get(&drawn.key) {
                        write!(out, "{}", encode::kitty_delete(*id))?;
                    }
                }
                ImageProtocol::Iterm2 | ImageProtocol::Sixel | ImageProtocol::HalfBlock => {}
            }
        }
        self.invalidate();
        out.flush()
    }

    /// Whether the terminal shows a drawing this overlay made.
    pub(crate) fn holds_drawings(&self) -> bool {
        !self.drawn.is_empty()
    }

    /// Draw `placements` over `buffer` (the frame the diff just painted) through `backend`.
    /// Returns whether anything was written.
    ///
    /// # Errors
    ///
    /// Propagates the backend's write error.
    pub(crate) fn paint<B: Backend + Write>(
        &mut self,
        backend: &mut B,
        buffer: &Buffer,
        placements: &[Placement],
    ) -> io::Result<bool> {
        if self.area != Some(buffer.area) {
            // A resize cleared the screen: nothing drawn survives it.
            self.drawn.clear();
            self.area = Some(buffer.area);
        }
        // Cell-bound pictures must not reach the screen's last row: the cursor moves below a
        // sixel or iTerm2 picture, and a move past the bottom would scroll the screen.
        let placements: Vec<&Placement> = placements
            .iter()
            .filter(|placement| {
                !placement.key.protocol.cell_bound()
                    || placement.rect().bottom() < buffer.area.bottom()
            })
            .filter(|placement| buffer.area.contains(placement.rect().as_position()))
            .filter(|placement| placement.rect().right() <= buffer.area.right())
            .collect();
        let mut wrote = false;
        let mut repainted: Vec<Rect> = Vec::new();
        let previous = std::mem::take(&mut self.drawn);
        for drawn in &previous {
            let kept = placements
                .iter()
                .any(|placement| placement.key == drawn.key && placement.rect() == drawn.rect);
            if kept {
                continue;
            }
            match drawn.key.protocol {
                ImageProtocol::Kitty => {
                    if let Some(id) = self.kitty_ids.get(&drawn.key) {
                        write!(backend, "{}", encode::kitty_delete(*id))?;
                    }
                }
                ImageProtocol::Iterm2 | ImageProtocol::Sixel => {
                    let rect = drawn.rect.intersection(buffer.area);
                    let cells = rect_cells(buffer, rect);
                    let positions = (rect.top()..rect.bottom())
                        .flat_map(|y| (rect.left()..rect.right()).map(move |x| (x, y)));
                    backend.draw(
                        positions
                            .zip(cells.iter())
                            .map(|((x, y), cell)| (x, y, cell)),
                    )?;
                    repainted.push(rect);
                }
                ImageProtocol::HalfBlock => {}
            }
            wrote = true;
        }
        for placement in placements {
            let rect = placement.rect();
            let cells = rect_cells(buffer, rect);
            let unchanged = previous.iter().any(|drawn| {
                drawn.key == placement.key && drawn.rect == rect && drawn.cells == cells
            }) && !repainted
                .iter()
                .any(|other| other.intersects(rect) && placement.key.protocol.cell_bound());
            if !unchanged {
                let Some(payload) = placement.rendered.payload.as_ref() else {
                    continue;
                };
                backend.set_cursor_position(rect.as_position())?;
                match payload {
                    Payload::KittyPng(png) => {
                        let next_id = u32::try_from(self.kitty_ids.len() + 1).unwrap_or(u32::MAX);
                        let id = *self.kitty_ids.entry(placement.key).or_insert(next_id);
                        if self.transmitted.insert(id) {
                            write!(backend, "{}", encode::kitty_transmit(id, png))?;
                        }
                        write!(
                            backend,
                            "{}",
                            encode::kitty_place(id, rect.width, rect.height)
                        )?;
                    }
                    Payload::Sequence(sequence) => write!(backend, "{sequence}")?,
                }
                super::store::note_shown(placement.key.image, placement.key.protocol);
                wrote = true;
            }
            self.drawn.push(Drawn {
                key: placement.key,
                rect,
                cells,
            });
        }
        Ok(wrote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline_image::store::RenderKey;
    use crate::inline_image::ImageGeometry;
    use ratatui::backend::CrosstermBackend;

    fn key(protocol: ImageProtocol) -> RenderKey {
        RenderKey {
            image: 7,
            geometry: ImageGeometry { cols: 2, rows: 2 },
            protocol,
            cell: (10, 20),
        }
    }

    fn placement(protocol: ImageProtocol, row: u16) -> Placement {
        let payload = match protocol {
            ImageProtocol::Kitty => Payload::KittyPng(vec![1, 2, 3]),
            ImageProtocol::Iterm2 | ImageProtocol::Sixel | ImageProtocol::HalfBlock => {
                Payload::Sequence("<PIXELS>".to_string())
            }
        };
        Placement {
            key: key(protocol),
            row,
            col: 1,
            rendered: Arc::new(Rendered {
                cells: Vec::new(),
                payload: Some(payload),
            }),
        }
    }

    fn frame(fill: &str) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 6));
        for cell in &mut buffer.content {
            cell.set_symbol(fill);
        }
        buffer
    }

    fn paint(overlay: &mut ImageOverlay, buffer: &Buffer, placements: &[Placement]) -> String {
        let mut backend = CrosstermBackend::new(Vec::new());
        overlay
            .paint(&mut backend, buffer, placements)
            .expect("paint");
        String::from_utf8(backend.writer().clone()).expect("utf-8")
    }

    #[test]
    fn a_kitty_image_transmits_once_moves_by_replacement_and_is_deleted_when_it_leaves() {
        let mut overlay = ImageOverlay::default();
        let buffer = frame("x");
        let first = paint(&mut overlay, &buffer, &[placement(ImageProtocol::Kitty, 1)]);
        assert_eq!(
            first,
            format!(
                "\x1b[2;2H{}{}",
                encode::kitty_transmit(1, &[1, 2, 3]),
                encode::kitty_place(1, 2, 2)
            )
        );
        // The same frame again draws nothing; a scroll moves the placement without a resend.
        assert_eq!(
            paint(&mut overlay, &buffer, &[placement(ImageProtocol::Kitty, 1)]),
            ""
        );
        assert_eq!(
            paint(&mut overlay, &buffer, &[placement(ImageProtocol::Kitty, 0)]),
            format!(
                "{}\x1b[1;2H{}",
                encode::kitty_delete(1),
                encode::kitty_place(1, 2, 2)
            )
        );
        // Scrolled out: the placement is deleted, the data kept.
        assert_eq!(paint(&mut overlay, &buffer, &[]), encode::kitty_delete(1));
        assert!(!overlay.holds_drawings());
        // A surface hand-off takes the placements back.
        paint(&mut overlay, &buffer, &[placement(ImageProtocol::Kitty, 2)]);
        let mut released = Vec::new();
        overlay.release(&mut released).expect("release");
        assert_eq!(
            String::from_utf8(released).expect("utf-8"),
            encode::kitty_delete(1)
        );
        assert!(!overlay.holds_drawings());
    }

    #[test]
    fn a_cell_bound_image_repaints_its_old_cells_and_redraws_when_the_diff_touched_it() {
        let mut overlay = ImageOverlay::default();
        let buffer = frame("x");
        assert_eq!(
            paint(&mut overlay, &buffer, &[placement(ImageProtocol::Sixel, 1)]),
            "\x1b[2;2H<PIXELS>"
        );
        // A frame that rewrote cells under the picture (a restyle) erased those pixels: the
        // picture is drawn again even though it did not move.
        let touched = frame("y");
        assert_eq!(
            paint(
                &mut overlay,
                &touched,
                &[placement(ImageProtocol::Sixel, 1)]
            ),
            "\x1b[2;2H<PIXELS>"
        );
        // Scrolled out: the frame's own cells overwrite the old rectangle (rows 2-3, cols 2-3
        // in 1-based terms), with no picture after.
        let gone = paint(&mut overlay, &touched, &[]);
        assert!(gone.starts_with("\x1b[2;2H"), "{gone:?}");
        assert_eq!(gone.matches('y').count(), 4, "{gone:?}");
        assert!(!gone.contains("<PIXELS>"), "{gone:?}");
        assert!(!overlay.holds_drawings());
    }

    #[test]
    fn a_cell_bound_image_never_reaches_the_last_screen_row() {
        let mut overlay = ImageOverlay::default();
        // Rows 4-5 of a 6-row screen: the picture would end on the last row.
        assert_eq!(
            paint(
                &mut overlay,
                &frame("x"),
                &[placement(ImageProtocol::Iterm2, 4)]
            ),
            ""
        );
        assert!(!overlay.holds_drawings());
    }
}
