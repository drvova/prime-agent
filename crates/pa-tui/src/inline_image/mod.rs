//! Inline transcript images: user-pasted images in the user's row, the images a tool result
//! carries (the python card's `attach_image`, any image block) in its card, sized to fit.
//!
//! The layout is geometry only: an image's rows are computed from its pixel dimensions (a
//! bounded header read, never a decode) and the available width, and each row is laid out as
//! a zero-width sentinel plus the blank cells the image will cover. The frame compose resolves
//! the sentinels ([`resolve_rows`]): each row's blank cells become the image's half-block cells
//! once the decode worker rendered them (until then they stay blank and the render is queued),
//! and every image whose rows are all on screen is reported for the pixel overlay
//! ([`overlay`]), which draws the terminal's graphics protocol over those same cells after the
//! frame's cell diff. The half-block cells are therefore always the base layer: a terminal
//! that ignores the protocol, a partly scrolled image, or a pending decode all show them.
//!
//! The decode, the resample and the protocol encode run on one worker thread ([`store`]):
//! the paint path only performs map lookups, so a first paint or a keystroke never waits on
//! an image.

mod detect;
mod encode;
pub(crate) mod overlay;
mod raster;
mod store;

use std::sync::Arc;

use ratatui::style::Style;

use crate::terminal_image::ImageDimensions;
use crate::{Line, Span};

pub(crate) use detect::{active_protocol, ImageProtocol};
pub(crate) use store::{set_waker, shown_counts, OnMiss};

/// The widest an inline image renders, in cells.
const MAX_IMAGE_COLS: usize = 80;
/// The tallest an inline image renders, in rows.
const MAX_IMAGE_ROWS: usize = 20;
/// Source pixels per cell column at the native size: an image never renders wider than this
/// scale implies (an icon stays icon-sized).
const NATIVE_PIXELS_PER_COL: u32 = 8;
/// The fallback cell size (VT340, Windows Terminal's sixel cell) when the terminal reports none.
const DEFAULT_CELL_PIXELS: (u16, u16) = (10, 20);

/// The zero-width row sentinel: `ESC _ pa-img;<key>;<row>;<cols>;<rows> ESC \`. Painting
/// strips every escape (`normalize_terminal_output`), so an unresolved row shows its blanks.
const SENTINEL_PREFIX: &str = "\x1b_pa-img;";

/// An image's cell box: half-block pixels are square at a 1:2 cell aspect, so the box keeps
/// the image's aspect at `rows = cols * height / width / 2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ImageGeometry {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// Fit an image of `dimensions` into at most `max_cols` columns (and the row cap).
pub(crate) fn fit(dimensions: ImageDimensions, max_cols: usize) -> Option<ImageGeometry> {
    let (width, height) = (dimensions.width_px, dimensions.height_px);
    if width == 0 || height == 0 || max_cols == 0 {
        return None;
    }
    let native = width.div_ceil(NATIVE_PIXELS_PER_COL) as usize;
    let mut cols = native.min(max_cols).clamp(1, MAX_IMAGE_COLS);
    let rows_for = |cols: usize| -> usize {
        ((cols as u64 * u64::from(height)).div_ceil(u64::from(width) * 2)).max(1) as usize
    };
    let mut rows = rows_for(cols);
    if rows > MAX_IMAGE_ROWS {
        rows = MAX_IMAGE_ROWS;
        cols = ((MAX_IMAGE_ROWS as u64 * 2 * u64::from(width)) / u64::from(height))
            .clamp(1, cols as u64) as usize;
    }
    Some(ImageGeometry {
        cols: u16::try_from(cols).ok()?,
        rows: u16::try_from(rows).ok()?,
    })
}

/// The cell box one image payload lays out in, from a bounded header read; `None` when its
/// dimensions are unreadable (the caller keeps the textual caption alone).
pub(crate) fn layout(data: &str, mime_type: &str, max_cols: usize) -> Option<ImageGeometry> {
    if data.is_empty() {
        return None;
    }
    let dimensions = crate::terminal_image::get_image_dimensions_prefix(
        data,
        mime_type,
        crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES,
    )?;
    fit(dimensions, max_cols)
}

/// The layout rows of one image payload (empty when [`layout`] is `None`): each row is the
/// sentinel plus `cols` blank cells in `style`; the caller adds its own lead and padding.
pub(crate) fn image_rows(data: &str, mime_type: &str, max_cols: usize, style: Style) -> Vec<Line> {
    let Some(geometry) = layout(data, mime_type, max_cols) else {
        return Vec::new();
    };
    let key = store::image_key(data);
    store::register(key, data);
    (0..geometry.rows)
        .map(|row| {
            vec![
                Span::raw(format!(
                    "{SENTINEL_PREFIX}{key:016x};{row};{};{}\x1b\\",
                    geometry.cols, geometry.rows
                )),
                Span::styled(" ".repeat(usize::from(geometry.cols)), style),
            ]
        })
        .collect()
}

/// The row count [`image_rows`] lays out (the count-only geometry pass).
pub(crate) fn image_row_count(data: &str, mime_type: &str, max_cols: usize) -> usize {
    layout(data, mime_type, max_cols).map_or(0, |geometry| usize::from(geometry.rows))
}

/// One parsed sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sentinel {
    image: u64,
    row: u16,
    geometry: ImageGeometry,
}

fn parse_sentinel(content: &str) -> Option<Sentinel> {
    let body = content
        .strip_prefix(SENTINEL_PREFIX)?
        .strip_suffix("\x1b\\")?;
    let mut fields = body.split(';');
    let image = u64::from_str_radix(fields.next()?, 16).ok()?;
    let row = fields.next()?.parse().ok()?;
    let cols = fields.next()?.parse().ok()?;
    let rows = fields.next()?.parse().ok()?;
    Some(Sentinel {
        image,
        row,
        geometry: ImageGeometry { cols, rows },
    })
}

/// The pixel size of one cell for `protocol`'s payload: the terminal's report where it gives
/// one, Windows Terminal's fixed sixel cell on Windows, else the default — except for sixel,
/// which draws at the terminal's own scale and so needs the real size (`None` without it).
pub(crate) fn cell_pixels(protocol: ImageProtocol) -> Option<(u16, u16)> {
    match protocol {
        ImageProtocol::HalfBlock => return Some((0, 0)),
        ImageProtocol::Kitty | ImageProtocol::Iterm2 | ImageProtocol::Sixel => {}
    }
    if cfg!(windows) {
        return Some(DEFAULT_CELL_PIXELS);
    }
    let reported = crossterm::terminal::window_size().ok().and_then(|size| {
        (size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0)
            .then(|| (size.width / size.columns, size.height / size.rows))
            .filter(|(width, height)| *width > 0 && *height > 0)
    });
    match (reported, protocol) {
        (Some(cell), _) => Some(cell),
        (None, ImageProtocol::Sixel) => None,
        (None, ImageProtocol::Kitty | ImageProtocol::Iterm2 | ImageProtocol::HalfBlock) => {
            Some(DEFAULT_CELL_PIXELS)
        }
    }
}

/// One image whose every row the resolved rows hold, ready for the pixel overlay.
#[derive(Debug, Clone)]
pub(crate) struct VisibleImage {
    pub(crate) key: store::RenderKey,
    /// The index (into the resolved rows) of the image's first row.
    pub(crate) top: usize,
    /// The column the image starts at.
    pub(crate) col: usize,
    pub(crate) rendered: Arc<store::Rendered>,
}

/// Resolve every image sentinel in `rows` (a frame's transcript window, or the exit flush's
/// rows): each row's blank cells become its half-block cells when the rendering is ready,
/// the sentinel is dropped, and a miss is queued per `on_miss`. Returns the images whose rows
/// are all present, in order and adjacent, with a ready pixel payload.
pub(crate) fn resolve_rows(
    rows: &mut [Line],
    protocol: ImageProtocol,
    on_miss: OnMiss,
) -> Vec<VisibleImage> {
    let Some(cell) = cell_pixels(protocol) else {
        return Vec::new();
    };
    // (image, first row index, rows seen so far, column)
    let mut runs: Vec<(store::RenderKey, usize, u16, usize)> = Vec::new();
    let mut ready: Vec<(store::RenderKey, Arc<store::Rendered>)> = Vec::new();
    for (index, row) in rows.iter_mut().enumerate() {
        let Some(at) = row
            .iter()
            .position(|span| span.content.starts_with(SENTINEL_PREFIX))
        else {
            continue;
        };
        let Some(sentinel) = parse_sentinel(&row[at].content) else {
            continue;
        };
        let col: usize = row[..at]
            .iter()
            .map(|span| crate::width::str_width(&span.content))
            .sum();
        let key = store::RenderKey {
            image: sentinel.image,
            geometry: sentinel.geometry,
            protocol,
            cell,
        };
        let rendered = match ready.iter().find(|(candidate, _)| *candidate == key) {
            Some((_, rendered)) => Some(rendered.clone()),
            None => store::lookup(key, on_miss).inspect(|rendered| {
                ready.push((key, rendered.clone()));
            }),
        };
        let blank_style = row
            .get(at + 1)
            .map_or_else(Style::default, |span| span.style);
        let cells = rendered
            .as_ref()
            .and_then(|rendered| rendered.cells.get(usize::from(sentinel.row)));
        match cells {
            Some(cells) => {
                let restyled = cells
                    .iter()
                    .map(|span| Span::styled(span.content.clone(), blank_style.patch(span.style)));
                let end = (at + 2).min(row.len());
                row.splice(at..end, restyled);
                if sentinel.row == 0
                    && protocol == ImageProtocol::HalfBlock
                    && on_miss == OnMiss::Render
                {
                    store::note_shown(sentinel.image, protocol);
                }
            }
            None => {
                row.remove(at);
            }
        }
        match runs.iter_mut().find(|(candidate, top, seen, start)| {
            *candidate == key && *start == col && top + usize::from(*seen) == index
        }) {
            Some(run) if sentinel.row == run.2 => run.2 += 1,
            Some(_) | None if sentinel.row == 0 => runs.push((key, index, 1, col)),
            Some(_) | None => {}
        }
    }
    runs.into_iter()
        .filter(|(key, _, seen, _)| *seen == key.geometry.rows)
        .filter_map(|(key, top, _, col)| {
            let rendered = ready
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, rendered)| rendered.clone())?;
            rendered.payload.is_some().then_some(VisibleImage {
                key,
                top,
                col,
                rendered,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
