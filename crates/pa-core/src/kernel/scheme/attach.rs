//! `(attach-image path ...)`: image files loaded into the model's context as
//! attachments of the running cell.
//!
//! A port of the Python kernel's `attach_image` skill, with the same limits:
//! PNG, JPEG, GIF and WebP by their signatures; at most 20 MB and 36
//! megapixels on disk; a small image is attached as it is, a larger one is
//! scaled to at most 1200 pixels a side and re-encoded as JPEG (an animated
//! one from its first frame, a transparent one over gray) until its base64
//! text fits 350,000 characters. Every path is checked before any image is
//! attached, so a bad one leaves none behind. Decoding and encoding are the
//! `image` crate's; its JPEG encoder writes baseline files, so sizes differ
//! a little from the Python skill's progressive ones.

use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage};
use serde_json::{json, Value};

use super::cell::{CellHandle, CellRecord};
use crate::tools::ipython::KernelAttachment;
use crate::tools::path_utils::resolve_to_cwd;

const MAX_SOURCE_BYTES: u64 = 20_000_000;
const MAX_SOURCE_PIXELS: u64 = 36_000_000;
/// The longest base64 text one attachment may carry: small enough that
/// clients render and replay image-heavy sessions cheaply.
const MAX_DATA_CHARS: usize = 350_000;
const MAX_DIMENSION: u32 = 1200;
/// What transparent pixels are laid over before the JPEG encoding.
const BACKGROUND: [u8; 3] = [0x88, 0x88, 0x88];
const JPEG_QUALITIES: [u8; 5] = [82, 72, 60, 48, 36];

/// Attach each of `paths` to `cell`: a line naming what was loaded, and how
/// each resized image was changed.
///
/// # Errors
///
/// A path that is not a string, not a regular file, too large, not one of
/// the four formats, not decodable, or not compressible to the limit.
/// Nothing is attached then.
pub(super) fn attach(cell: &CellHandle, paths: &[Value]) -> anyhow::Result<Value> {
    let cwd = cell.session().cwd.to_string_lossy().into_owned();
    let mut prepared = Vec::new();
    let mut notes = Vec::new();
    let mut named = Vec::new();
    for path in paths {
        let shown = path
            .as_str()
            .ok_or_else(|| anyhow!("attach-image: each path must be a string"))?;
        let file = PathBuf::from(resolve_to_cwd(shown, &cwd));
        if !file.is_file() {
            bail!("attach-image: {shown} is not an existing regular file");
        }
        let size = std::fs::metadata(&file)
            .with_context(|| format!("attach-image: {shown} cannot be read"))?
            .len();
        if size > MAX_SOURCE_BYTES {
            bail!(
                "attach-image: {shown} is {}MB; images must be under {}MB. Resize it first.",
                size / 1_000_000,
                MAX_SOURCE_BYTES / 1_000_000
            );
        }
        let bytes = std::fs::read(&file)
            .with_context(|| format!("attach-image: {shown} cannot be read"))?;
        let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            ImageFormat::Png
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            ImageFormat::Jpeg
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            ImageFormat::Gif
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            ImageFormat::WebP
        } else {
            bail!(
                "attach-image: {shown} is not a supported image (PNG, JPEG, GIF, WebP). Only images \
can be loaded into context; read other files with (fs-read path) or (bash command)."
            );
        };
        let unreadable = || {
            format!(
                "attach-image: {shown} is not a readable supported image (PNG, JPEG, GIF, WebP)"
            )
        };
        let (width, height) = ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .with_context(unreadable)?;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_SOURCE_PIXELS {
            bail!(
                "attach-image: {shown} is {width}x{height} ({}MP); images must be at most {}MP. \
Resize it first.",
                pixels / 1_000_000,
                MAX_SOURCE_PIXELS / 1_000_000
            );
        }
        let animated = match format {
            ImageFormat::Gif => {
                GifDecoder::new(Cursor::new(&bytes))
                    .with_context(unreadable)?
                    .into_frames()
                    .take(2)
                    .count()
                    > 1
            }
            ImageFormat::Png => PngDecoder::new(Cursor::new(&bytes))
                .and_then(|decoder| decoder.is_apng())
                .with_context(unreadable)?,
            ImageFormat::WebP => WebPDecoder::new(Cursor::new(&bytes))
                .with_context(unreadable)?
                .has_animation(),
            // The signatures above admit only the four formats.
            _ => false,
        };
        // Decoded in full: a file that only starts like an image is refused
        // here, before anything is attached.
        let mut decoder = ImageReader::with_format(Cursor::new(&bytes), format)
            .into_decoder()
            .with_context(unreadable)?;
        let orientation = decoder.orientation().with_context(unreadable)?;
        let mut image = DynamicImage::from_decoder(decoder).with_context(unreadable)?;
        let encode = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);
        let attachment =
            if bytes.len() <= MAX_DATA_CHARS * 3 / 4 && width.max(height) <= MAX_DIMENSION {
                KernelAttachment {
                    mime_type: format.to_mime_type().to_string(),
                    data: encode(&bytes),
                }
            } else {
                image.apply_orientation(orientation);
                let transparent = image.color().has_alpha();
                let flat: RgbImage = if transparent {
                    let mut flat =
                        RgbImage::from_pixel(image.width(), image.height(), image::Rgb(BACKGROUND));
                    for (pixel, over) in flat.pixels_mut().zip(image.to_rgba8().pixels()) {
                        let alpha = u32::from(over.0[3]);
                        for channel in 0..3 {
                            let blended = u32::from(over.0[channel]) * alpha
                                + u32::from(pixel.0[channel]) * (255 - alpha);
                            pixel.0[channel] = ((blended + 127) / 255) as u8;
                        }
                    }
                    flat
                } else {
                    image.into_rgb8()
                };
                let flat = DynamicImage::ImageRgb8(flat);
                let (full_width, full_height) = (flat.width(), flat.height());
                let scale = f64::min(
                    1.0,
                    f64::from(MAX_DIMENSION) / f64::from(full_width.max(full_height)),
                );
                let mut target = (
                    ((f64::from(full_width) * scale).round() as u32).max(1),
                    ((f64::from(full_height) * scale).round() as u32).max(1),
                );
                let mut smallest = 0;
                let fitted = loop {
                    let resized = flat
                        .resize_exact(target.0, target.1, image::imageops::FilterType::Lanczos3)
                        .into_rgb8();
                    let mut fitted = None;
                    for quality in JPEG_QUALITIES {
                        let mut jpeg = Vec::new();
                        JpegEncoder::new_with_quality(&mut jpeg, quality)
                            .encode_image(&resized)
                            .with_context(|| {
                                format!("attach-image: {shown} could not be re-encoded")
                            })?;
                        let data = encode(&jpeg);
                        smallest = data.len();
                        if data.len() <= MAX_DATA_CHARS {
                            fitted = Some((data, quality));
                            break;
                        }
                    }
                    if let Some((data, quality)) = fitted {
                        break (data, quality);
                    }
                    let next = (
                        ((f64::from(target.0) * 0.75) as u32).max(1),
                        ((f64::from(target.1) * 0.75) as u32).max(1),
                    );
                    if next == target {
                        bail!(
                            "attach-image: {shown} could not be compressed below {}KB of base64 \
(smallest was {}KB at {}x{})",
                            MAX_DATA_CHARS / 1000,
                            smallest / 1000,
                            target.0,
                            target.1
                        );
                    }
                    target = next;
                };
                let mut note = format!(
                "{shown}: original {full_width}x{full_height}; attached {}x{} JPEG at quality {}",
                target.0, target.1, fitted.1
            );
                if animated {
                    note.push_str("; animated image flattened to first frame");
                }
                if transparent {
                    note.push_str("; transparent pixels composited on #888888 background");
                }
                notes.push(note);
                KernelAttachment {
                    mime_type: "image/jpeg".to_string(),
                    data: fitted.0,
                }
            };
        prepared.push(attachment);
        named.push(shown);
    }
    let count = prepared.len();
    for attachment in prepared {
        cell.record(CellRecord::Attachment(attachment));
    }
    let mut message = format!("Loaded {count} image(s) into context: {}", named.join(", "));
    if !notes.is_empty() {
        message.push_str("\nResized for efficient inline rendering/replay:\n- ");
        message.push_str(&notes.join("\n- "));
    }
    Ok(json!(message))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use image::{ImageFormat, Rgba, RgbaImage};
    use serde_json::json;

    use crate::kernel::scheme::SchemeKernelProvisioner;
    use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
    use crate::tools::ipython::{
        ExecuteResult, IpythonKernelProvisioner, KernelAttachment, KernelExecuteOptions,
    };

    /// One cell in a kernel working in `dir` whose model takes `input`.
    async fn run(dir: &std::path::Path, input: &[&str], code: &str) -> ExecuteResult {
        let model = json!({"id": "p/m", "input": input});
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "model.info",
            host_handler(move |_payload: HostRequestPayload| {
                let model = model.clone();
                async move { Ok(model) }
            }),
        );
        SchemeKernelProvisioner::new(handlers, dir)
            .ensure(None, None)
            .await
            .unwrap()
            .execute(
                code,
                KernelExecuteOptions {
                    signal: None,
                    timeout_ms: None,
                    on_stream: None,
                    on_late_sent_agent_message: None,
                },
            )
            .await
            .unwrap()
    }

    /// A small image is attached as it is; a large transparent one is scaled
    /// to the limit and re-encoded over gray; a file that is not an image, or
    /// a model that takes none, attaches nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn images_are_attached_as_they_are_or_scaled_and_bad_input_attaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.png");
        RgbaImage::from_pixel(2, 2, Rgba([200, 10, 10, 255]))
            .save_with_format(&small, ImageFormat::Png)
            .unwrap();
        RgbaImage::from_fn(1600, 800, |x, y| {
            Rgba([
                (x / 8) as u8,
                (y / 4) as u8,
                90,
                if x < 800 { 0 } else { 255 },
            ])
        })
        .save_with_format(dir.path().join("wide.png"), ImageFormat::Png)
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not an image").unwrap();

        let both = run(
            dir.path(),
            &["text", "image"],
            r#"(display (attach-image "small.png" "wide.png"))"#,
        )
        .await;
        let mixed = run(
            dir.path(),
            &["text", "image"],
            r#"(attach-image "small.png" "notes.txt")"#,
        )
        .await;
        let blind = run(dir.path(), &["text"], r#"(attach-image "small.png")"#).await;

        let scaled = &both.attachments[1];
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(&scaled.data)
            .unwrap();
        let shown = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg)
            .unwrap()
            .into_rgb8();
        // The left half was transparent: it shows the gray it was laid over.
        let laid_over = shown.get_pixel(100, 300).0;
        let refused = |result: ExecuteResult, reason: &str| {
            (
                result
                    .error
                    .is_some_and(|error| error.evalue.contains(reason)),
                result.attachments,
            )
        };
        assert_eq!(
            (
                both.stdout,
                both.attachments[0].clone(),
                scaled.mime_type.as_str(),
                shown.dimensions(),
                laid_over.iter().all(|channel| channel.abs_diff(0x88) <= 4),
                scaled.data.len() <= super::MAX_DATA_CHARS,
                refused(mixed, "notes.txt is not a supported image"),
                refused(blind, "p/m does not take images"),
            ),
            (
                "Loaded 2 image(s) into context: small.png, wide.png\n\
                 Resized for efficient inline rendering/replay:\n\
                 - wide.png: original 1600x800; attached 1200x600 JPEG at quality 82; \
                 transparent pixels composited on #888888 background"
                    .to_string(),
                KernelAttachment {
                    mime_type: "image/png".to_string(),
                    data: base64::engine::general_purpose::STANDARD
                        .encode(std::fs::read(&small).unwrap()),
                },
                "image/jpeg",
                (1200, 600),
                true,
                true,
                (true, Vec::new()),
                (true, Vec::new()),
            )
        );
    }
}
