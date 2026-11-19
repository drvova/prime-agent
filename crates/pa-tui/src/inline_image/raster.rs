//! Pixels: decode an image payload (bounded), resample it to a target box, and turn the
//! result into the half-block cells every terminal can paint. Runs on the decode worker only:
//! nothing here is reachable from the paint path.

use ratatui::style::{Color, Style};

use crate::{Line, Span};

/// The decoder's strict bounds: a payload past them (a decompression bomb, a panorama) is
/// not decoded and its rows keep the textual caption.
const MAX_SOURCE_SIDE: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// The upper half block: the cell's foreground paints the top pixel, its background the
/// bottom one.
const UPPER_HALF: &str = "\u{2580}";
/// The lower half block, for a cell whose top pixel is transparent.
const LOWER_HALF: &str = "\u{2584}";

/// Straight RGBA pixels, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<[u8; 4]>,
}

impl Rgba {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.pixels[(y * self.width + x) as usize]
    }
}

/// Decode raw image bytes (PNG, JPEG, GIF's first frame, WebP) within the decoder bounds.
pub(crate) fn decode(bytes: &[u8]) -> Result<Rgba, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| error.to_string())?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    Ok(Rgba {
        width,
        height,
        pixels: decoded.pixels().map(|pixel| pixel.0).collect(),
    })
}

/// Resample to exactly `width` x `height`: each target pixel averages the source pixels its
/// area covers (a box filter, so a downscale never aliases), and an upscale repeats the
/// nearest source pixel. Colour averages are alpha-weighted so transparent pixels never
/// darken their neighbours.
pub(crate) fn resample(source: &Rgba, width: u32, height: u32) -> Rgba {
    let width = width.max(1);
    let height = height.max(1);
    let span = |target: u32, target_len: u32, source_len: u32| -> (u32, u32) {
        let start = u64::from(target) * u64::from(source_len) / u64::from(target_len);
        let end = (u64::from(target) + 1) * u64::from(source_len) / u64::from(target_len);
        let start = start.min(u64::from(source_len) - 1) as u32;
        (start, (end as u32).max(start + 1).min(source_len))
    };
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        let (y0, y1) = span(y, height, source.height);
        for x in 0..width {
            let (x0, x1) = span(x, width, source.width);
            let mut sum = [0u64; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let [r, g, b, a] = source.at(sx, sy);
                    let alpha = u64::from(a);
                    sum[0] += u64::from(r) * alpha;
                    sum[1] += u64::from(g) * alpha;
                    sum[2] += u64::from(b) * alpha;
                    sum[3] += alpha;
                }
            }
            let count = u64::from((x1 - x0) * (y1 - y0));
            // A fully transparent area has no colour to average: it stays transparent black.
            let channel = |total: u64| total.checked_div(sum[3]).map_or(0, |value| value as u8);
            pixels.push([
                channel(sum[0]),
                channel(sum[1]),
                channel(sum[2]),
                (sum[3] / count) as u8,
            ]);
        }
    }
    Rgba {
        width,
        height,
        pixels,
    }
}

/// A pixel's half-block color: `None` for a (mostly) transparent pixel, which shows the
/// terminal's default background.
fn cell_color(pixel: [u8; 4]) -> Option<Color> {
    (pixel[3] >= 128).then(|| Color::Rgb(pixel[0], pixel[1], pixel[2]))
}

/// The half-block rows for `cols` x `rows` cells: the image resampled to `cols` x `2*rows`
/// pixels, two vertically stacked pixels per cell. Adjacent cells of one style share a span.
pub(crate) fn half_block_rows(source: &Rgba, cols: u16, rows: u16) -> Vec<Line> {
    let scaled = resample(source, u32::from(cols), u32::from(rows) * 2);
    let mut lines = Vec::with_capacity(usize::from(rows));
    for row in 0..u32::from(rows) {
        let mut line: Line = Vec::new();
        for col in 0..u32::from(cols) {
            let top = cell_color(scaled.at(col, row * 2));
            let bottom = cell_color(scaled.at(col, row * 2 + 1));
            let (glyph, style) = match (top, bottom) {
                (Some(top), Some(bottom)) => (UPPER_HALF, Style::default().fg(top).bg(bottom)),
                (Some(top), None) => (UPPER_HALF, Style::default().fg(top)),
                (None, Some(bottom)) => (LOWER_HALF, Style::default().fg(bottom)),
                (None, None) => (" ", Style::default()),
            };
            match line.last_mut() {
                Some(span) if span.style == style && span.content.ends_with(glyph) => {
                    span.content.push_str(glyph);
                }
                _ => line.push(Span::styled(glyph, style)),
            }
        }
        lines.push(line);
    }
    lines
}

/// Encode RGBA pixels as PNG (the kitty and iTerm2 payload format).
pub(crate) fn encode_png(image: &Rgba) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let flat: Vec<u8> = image.pixels.iter().flatten().copied().collect();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &flat,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| error.to_string())?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `size` x `size` image whose quadrants are red, green, blue and transparent
    /// (clockwise from the top left).
    pub(crate) fn quadrants(size: u32) -> Rgba {
        let half = size / 2;
        let pixels = (0..size * size)
            .map(|index| {
                let (x, y) = (index % size, index / size);
                match (x < half, y < half) {
                    (true, true) => [255, 0, 0, 255],
                    (false, true) => [0, 255, 0, 255],
                    (false, false) => [0, 0, 255, 255],
                    (true, false) => [0, 0, 0, 0],
                }
            })
            .collect();
        Rgba {
            width: size,
            height: size,
            pixels,
        }
    }

    #[test]
    fn a_quadrant_fixture_renders_its_exact_half_block_cells() {
        let red = Color::Rgb(255, 0, 0);
        let green = Color::Rgb(0, 255, 0);
        let blue = Color::Rgb(0, 0, 255);
        // 2 cols x 2 rows = 2 x 4 pixels: the top row of cells is all colour, the bottom row
        // keeps the transparent quadrant as the default background.
        assert_eq!(
            half_block_rows(&quadrants(32), 2, 2),
            vec![
                vec![
                    Span::styled(UPPER_HALF, Style::default().fg(red).bg(red)),
                    Span::styled(UPPER_HALF, Style::default().fg(green).bg(green)),
                ],
                vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(UPPER_HALF, Style::default().fg(blue).bg(blue)),
                ],
            ]
        );
    }

    #[test]
    fn resampling_averages_by_area_and_repeats_on_upscale() {
        let image = quadrants(4);
        let down = resample(&image, 2, 2);
        assert_eq!(
            down.pixels,
            vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 0, 0],
                [0, 0, 255, 255]
            ]
        );
        // A 2x1 average of an opaque and a transparent pixel keeps the opaque colour at half
        // coverage (alpha-weighted), never a darkened mix.
        let mixed = Rgba {
            width: 2,
            height: 1,
            pixels: vec![[200, 100, 50, 255], [0, 0, 0, 0]],
        };
        assert_eq!(resample(&mixed, 1, 1).pixels, vec![[200, 100, 50, 127]]);
        assert_eq!(resample(&down, 4, 4), resample(&image, 4, 4));
    }

    #[test]
    fn a_png_round_trips_through_the_encoder_and_the_bounded_decoder() {
        let image = quadrants(8);
        let png = encode_png(&image).expect("encode");
        assert_eq!(decode(&png), Ok(image));
        assert!(decode(b"not an image").is_err());
    }
}
