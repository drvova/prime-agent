use base64::Engine;
use ratatui::style::{Color, Style};

use super::raster::tests::quadrants;
use super::*;

/// A base64 PNG of the quadrant fixture (`size` x `size`).
pub(crate) fn png_payload(size: u32) -> String {
    let png = raster::encode_png(&quadrants(size)).expect("encode");
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// Block until the decode worker finished every queued render.
pub(crate) fn wait_for_renders() {
    store::wait_idle();
}

fn dims(width_px: u32, height_px: u32) -> ImageDimensions {
    ImageDimensions {
        width_px,
        height_px,
    }
}

fn text(rows: &[Line]) -> Vec<String> {
    rows.iter()
        .map(|row| row.iter().map(|span| span.content.as_str()).collect())
        .collect()
}

#[test]
fn images_fit_their_aspect_into_the_width_and_row_caps() {
    let geometry = |width, height, max_cols| fit(dims(width, height), max_cols);
    assert_eq!(
        [
            // An icon stays icon-sized: 16 px is two native columns.
            geometry(16, 16, 100),
            // A screenshot fills the column cap, then the row cap narrows it.
            geometry(1920, 1080, 100),
            geometry(1920, 1080, 40),
            // A tall strip hits the row cap at its own aspect.
            geometry(100, 4000, 100),
            geometry(0, 10, 100),
            geometry(10, 10, 0),
        ],
        [
            Some(ImageGeometry { cols: 2, rows: 1 }),
            Some(ImageGeometry { cols: 71, rows: 20 }),
            Some(ImageGeometry { cols: 40, rows: 12 }),
            Some(ImageGeometry { cols: 1, rows: 20 }),
            None,
            None,
        ]
    );
}

#[test]
fn a_rendered_image_resolves_to_its_half_block_cells_and_reports_whole_visibility() {
    let payload = png_payload(32);
    // 32 px is four native columns: a 4 x 2 cell box.
    let rows = image_rows(
        &payload,
        "image/png",
        60,
        Style::default().bg(Color::Indexed(4)),
    );
    assert_eq!(image_row_count(&payload, "image/png", 60), 2);
    assert_eq!(rows.len(), 2);
    let mut window: Vec<Line> = rows
        .iter()
        .map(|row| {
            let mut line = vec![Span::raw("  ")];
            line.extend(row.iter().cloned());
            line
        })
        .collect();
    // The first frame queues the render and paints the reserved blanks, sentinels dropped.
    let mut first = window.clone();
    let pending = resolve_rows(&mut first, ImageProtocol::Kitty, OnMiss::Render);
    assert!(pending.is_empty());
    assert_eq!(text(&first), vec!["      ", "      "]);
    store::wait_idle();
    let visible = resolve_rows(&mut window, ImageProtocol::Kitty, OnMiss::Render);
    let red = Style::default()
        .fg(Color::Rgb(255, 0, 0))
        .bg(Color::Rgb(255, 0, 0));
    let green = Style::default()
        .fg(Color::Rgb(0, 255, 0))
        .bg(Color::Rgb(0, 255, 0));
    let blue = Style::default()
        .fg(Color::Rgb(0, 0, 255))
        .bg(Color::Rgb(0, 0, 255));
    // The transparent quadrant keeps the row's own background.
    let slab = Style::default().bg(Color::Indexed(4));
    assert_eq!(
        window,
        vec![
            vec![
                Span::raw("  "),
                Span::styled("\u{2580}\u{2580}", red),
                Span::styled("\u{2580}\u{2580}", green),
            ],
            vec![
                Span::raw("  "),
                Span::styled("  ", slab),
                Span::styled("\u{2580}\u{2580}", blue),
            ],
        ]
    );
    assert_eq!(
        visible
            .iter()
            .map(|image| (image.key.geometry, image.top, image.col))
            .collect::<Vec<_>>(),
        vec![(ImageGeometry { cols: 4, rows: 2 }, 0, 2)]
    );
}

#[test]
fn a_partly_visible_image_paints_half_blocks_but_no_pixel_placement() {
    let payload = png_payload(48);
    let rows = image_rows(&payload, "image/png", 60, Style::default());
    assert_eq!(rows.len(), 3);
    resolve_rows(&mut rows.clone(), ImageProtocol::Kitty, OnMiss::Render);
    store::wait_idle();
    // The window starts one row into the image (scrolled up by one).
    let mut window: Vec<Line> = rows[1..].to_vec();
    let visible = resolve_rows(&mut window, ImageProtocol::Kitty, OnMiss::Render);
    assert!(visible.is_empty(), "a cut image has no placement");
    assert!(
        text(&window).iter().all(|row| row.contains('\u{2580}')),
        "{window:?}"
    );
}

#[test]
fn the_exit_flush_never_waits_and_never_leaks_a_sentinel() {
    // A payload never rendered: the flush resolves without queueing and keeps the blanks.
    let payload = png_payload(40);
    let mut rows = image_rows(&payload, "image/png", 60, Style::default());
    let visible = resolve_rows(&mut rows, ImageProtocol::HalfBlock, OnMiss::Skip);
    assert!(visible.is_empty());
    assert!(text(&rows).iter().all(|row| row == "     "), "{rows:?}");
    assert!(rows
        .iter()
        .flatten()
        .all(|span| !span.content.contains('\x1b')));
}
