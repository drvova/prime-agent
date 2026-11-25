//! Reading an image from the system clipboard for editor paste: prefer a supported
//! image type (PNG, JPEG, GIF, WebP) and hand back the payload plus its mime type.
//! Every reader is best-effort: a missing tool, an empty clipboard, or an unsupported
//! type reads as [`ImagePasteError::NoImage`], which the paste reports instead of sending.
//! Native Windows reads the clipboard's raw formats through user32 (`pa_types::platform`) and
//! converts bitmaps to PNG here (`bitmap`).

#[cfg(any(windows, test))]
mod bitmap;

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use crate::image_load::{LoadedImage, SUPPORTED_IMAGE_MIME_TYPES};

/// The largest image a paste attaches, in raw bytes (omp's `MAX_IMAGE_INPUT_BYTES`): above it the
/// paste reports the size instead of shipping a payload providers reject.
pub(crate) const MAX_PASTED_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Where a pasted image came from (the `tui image paste` `source` property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteSource {
    /// Image data on the clipboard (a screenshot, a browser copy).
    Clipboard,
    /// An image file a clipboard file copy names (Explorer's Copy on Windows).
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the Win32 reader sees file copies; the variant stays so the telemetry labels match on every platform"
        )
    )]
    ClipboardFile,
    /// An image file path pasted or dragged into the prompt.
    Path,
}

impl ImagePasteSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ImagePasteSource::Clipboard => "clipboard",
            ImagePasteSource::ClipboardFile => "clipboard_file",
            ImagePasteSource::Path => "path",
        }
    }
}

/// Why a paste attached no image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteError {
    /// The clipboard holds no image in a supported format.
    NoImage,
    /// The image is larger than [`MAX_PASTED_IMAGE_BYTES`].
    TooLarge { bytes: u64 },
    /// The clipboard offers a bitmap this reader cannot decode, or the image file cannot be read.
    Unreadable,
}

impl ImagePasteError {
    /// The `tui image paste` `outcome` property.
    pub(crate) fn outcome(self) -> &'static str {
        match self {
            ImagePasteError::NoImage => "no_image",
            ImagePasteError::TooLarge { .. } => "too_large",
            ImagePasteError::Unreadable => "unreadable",
        }
    }

    /// The status row the paste shows.
    pub(crate) fn message(self) -> String {
        match self {
            ImagePasteError::NoImage => {
                "Clipboard has no image (PNG, JPEG, GIF, or WebP).".to_string()
            }
            ImagePasteError::TooLarge { bytes } => format!(
                "Image too large: {} (limit {}).",
                format_image_size(bytes),
                format_image_size(MAX_PASTED_IMAGE_BYTES)
            ),
            ImagePasteError::Unreadable => {
                "Clipboard image could not be read in a supported format.".to_string()
            }
        }
    }
}

/// `bytes` for a status row: whole kilobytes (rounded up) under a megabyte, megabytes with one
/// decimal above (`312 KB`, `20.0 MB`).
pub(crate) fn format_image_size(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        return format!("{} KB", bytes.div_ceil(1024));
    }
    let tenths = bytes * 10 / (1024 * 1024);
    format!("{}.{} MB", tenths / 10, tenths % 10)
}

/// One attached image and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastedImage {
    pub(crate) image: ClipboardImage,
    pub(crate) source: ImagePasteSource,
    /// The raw (pre-base64) size, for the attach notice.
    pub(crate) bytes: u64,
}

const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_POWERSHELL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// One clipboard image: the base64 payload plus its sniffed mime type.
pub type ClipboardImage = LoadedImage;

/// Strip parameters from a mime type (`image/png; charset=...`).
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// The supported image type among `types`, in the `SUPPORTED_IMAGE_MIME_TYPES` order;
/// any other `image/*` type loses to `None` (it cannot be sent without a converter).
fn select_supported_image_mime_type(types: &[String]) -> Option<String> {
    let normalized: Vec<String> = types
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(base_mime_type)
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some(found) = normalized.iter().find(|t| t.as_str() == preferred) {
            return Some(found.clone());
        }
    }
    None
}

/// Run one command with a hard timeout; `None` on spawn failure, nonzero
/// exit, or timeout.
async fn run_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let child = Command::new(program)
        .args(args)
        // Without the drop-kill, a hung converter keeps running past the
        // deadline — an orphaned direct child of the TUI.
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let Ok(Ok(output)) = tokio::time::timeout(timeout, child.wait_with_output()).await else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    if output.stdout.len() > DEFAULT_MAX_BUFFER_BYTES {
        return None;
    }
    Some(output.stdout)
}

/// Whether this is a WSL session.
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSLENV").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version").is_ok_and(|version| {
        version.contains("microsoft") || version.contains("Microsoft") || version.contains("WSL")
    })
}

/// Whether this is a Wayland session.
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// Split a command's textual stdout into trimmed lines.
fn stdout_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// `wl-paste` (Wayland): list the offered types, pick the preferred
/// supported image type, read it without a trailing newline.
async fn read_via_wl_paste() -> Option<Vec<u8>> {
    let types =
        stdout_lines(&run_command("wl-paste", &["--list-types"], DEFAULT_LIST_TIMEOUT).await?);
    let selected = select_supported_image_mime_type(&types)?;
    let bytes = run_command(
        "wl-paste",
        &["--type", &selected, "--no-newline"],
        DEFAULT_READ_TIMEOUT,
    )
    .await?;
    Some(bytes).filter(|bytes| is_supported_image(bytes))
}

/// `xclip` (X11): enumerate `TARGETS`, then read the preferred supported
/// image type (trying each in order when no target matched).
async fn read_via_xclip() -> Option<Vec<u8>> {
    let targets = run_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        DEFAULT_LIST_TIMEOUT,
    )
    .await
    .map(|stdout| stdout_lines(&stdout))
    .unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(preferred) = select_supported_image_mime_type(&targets) {
        candidates.push(preferred);
    }
    candidates.extend(SUPPORTED_IMAGE_MIME_TYPES.iter().map(ToString::to_string));
    for mime_type in candidates {
        let bytes = run_command(
            "xclip",
            &["-selection", "clipboard", "-t", &mime_type, "-o"],
            DEFAULT_READ_TIMEOUT,
        )
        .await
        .filter(|bytes| is_supported_image(bytes));
        if bytes.is_some() {
            return bytes;
        }
    }
    None
}

/// Validate image bytes by content (the clipboard-reported type may be wrong and there is no
/// converter to fall back on — content sniffing is this port's equivalent of TS's Photon
/// conversion), enforce [`MAX_PASTED_IMAGE_BYTES`], and encode the payload.
fn pasted_image_from_bytes(
    bytes: &[u8],
    source: ImagePasteSource,
) -> Result<PastedImage, ImagePasteError> {
    use base64::Engine;
    let size = bytes.len() as u64;
    if size > MAX_PASTED_IMAGE_BYTES {
        return Err(ImagePasteError::TooLarge { bytes: size });
    }
    let mime_type = crate::image_load::detect_supported_image_mime_from_bytes(bytes)
        .ok_or(ImagePasteError::NoImage)?;
    Ok(PastedImage {
        image: ClipboardImage {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: mime_type.to_string(),
        },
        source,
        bytes: size,
    })
}

/// Load an image file a paste names: `Ok(None)` when the file is not a supported image (the
/// paste stays text), the size refusal above [`MAX_PASTED_IMAGE_BYTES`].
pub(crate) fn load_pasted_image_file(
    path: &Path,
    source: ImagePasteSource,
) -> Result<Option<PastedImage>, ImagePasteError> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(None);
    };
    if !metadata.is_file()
        || crate::image_load::detect_supported_image_mime_from_path(path)
            .ok()
            .flatten()
            .is_none()
    {
        return Ok(None);
    }
    if metadata.len() > MAX_PASTED_IMAGE_BYTES {
        return Err(ImagePasteError::TooLarge {
            bytes: metadata.len(),
        });
    }
    let bytes = std::fs::read(path).map_err(|_| ImagePasteError::Unreadable)?;
    pasted_image_from_bytes(&bytes, source).map(Some)
}

/// Whether the bytes sniff as a supported image.
fn is_supported_image(bytes: &[u8]) -> bool {
    crate::image_load::detect_supported_image_mime_from_bytes(bytes).is_some()
}

/// WSL: PowerShell reads the Windows clipboard and saves it as a PNG at a
/// temp path, which WSL reads back and removes.
async fn read_via_powershell() -> Option<Vec<u8>> {
    let temp_dir = std::env::temp_dir();
    std::fs::create_dir_all(&temp_dir).ok()?;
    let temp_path = temp_dir.join(format!("prime-agent-clip-{}.png", unique_suffix()));
    let windows_path = String::from_utf8_lossy(
        &run_command(
            "wslpath",
            &["-w", temp_path.to_str()?],
            DEFAULT_LIST_TIMEOUT,
        )
        .await?,
    )
    .trim()
    .to_string();
    if windows_path.is_empty() {
        return None;
    }
    let escaped_path = windows_path.replace('\'', "''");
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms",
        "Add-Type -AssemblyName System.Drawing",
        format!("$path = '{escaped_path}'").as_str(),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()",
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }",
    ]
    .join("; ");
    let output = run_command(
        "powershell.exe",
        &["-NoProfile", "-Command", &script],
        DEFAULT_POWERSHELL_TIMEOUT,
    )
    .await?;
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return None;
    }
    let bytes = std::fs::read(&temp_path).ok();
    let _ = std::fs::remove_file(&temp_path);
    bytes.filter(|bytes| is_supported_image(bytes))
}

/// A unique temp-file suffix: the per-process random hasher seeded by the
/// OS plus the current nanosecond clock.
fn unique_suffix() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// macOS: a JavaScript-for-Automation script reads the pasteboard's PNG data and
/// writes it to a temp file, which we read back and remove. Non-PNG flavors are not read.
async fn read_via_osascript() -> Option<Vec<u8>> {
    let temp_dir = std::env::temp_dir();
    std::fs::create_dir_all(&temp_dir).ok()?;
    let temp_path = temp_dir.join(format!("prime-agent-clip-{}.png", unique_suffix()));
    let path_str = temp_path.to_str()?.to_string();
    let script = format!(
        r"ObjC.import('Cocoa');
const pb = $.NSPasteboard.generalPasteboard;
const types = pb.types.allObjects;
if (!types.containsObject($.NSPasteboardTypePNG)) {{
    'empty'
}} else {{
    const data = pb.dataForType($.NSPasteboardTypePNG);
    const text = $.NSString.alloc.initWithDataEncoding(data, $.NSISOLatin1StringEncoding);
    text.writeToFileAtomicallyEncodingError('{path_str}', false, $.NSISOLatin1StringEncoding, null);
    'ok'
}}"
    );
    let output = run_command(
        "osascript",
        &["-l", "JavaScript", "-e", &script],
        DEFAULT_READ_TIMEOUT,
    )
    .await?;
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return None;
    }
    let bytes = std::fs::read(&temp_path).ok();
    let _ = std::fs::remove_file(&temp_path);
    bytes.filter(|bytes| is_supported_image(bytes))
}

/// Native Windows: the clipboard's raw payloads through user32 (`pa_types::platform`), read and
/// converted on a blocking thread so the event loop keeps painting.
#[cfg(windows)]
async fn read_via_windows_clipboard() -> Result<PastedImage, ImagePasteError> {
    tokio::task::spawn_blocking(|| {
        let formats = pa_types::platform::read_clipboard_image_formats()
            .map_err(|_| ImagePasteError::Unreadable)?;
        pasted_image_from_windows_formats(formats)
    })
    .await
    .map_err(|_| ImagePasteError::Unreadable)?
}

/// The attachment a Windows clipboard read yields: the `PNG` format as is (browsers, Office and
/// the Snipping Tool offer it with real alpha), else `CF_DIBV5` then `CF_DIB` decoded to PNG here,
/// else the first supported image file an Explorer copy names.
#[cfg(any(windows, test))]
fn pasted_image_from_windows_formats(
    formats: pa_types::platform::ClipboardImageFormats,
) -> Result<PastedImage, ImagePasteError> {
    let pa_types::platform::ClipboardImageFormats {
        png,
        dib_v5,
        dib,
        files,
    } = formats;
    let offered_bitmap = png.is_some() || dib_v5.is_some() || dib.is_some();
    if let Some(png) = png {
        if is_supported_image(&png) {
            return pasted_image_from_bytes(&png, ImagePasteSource::Clipboard);
        }
    }
    if let Some(png) = [dib_v5, dib].into_iter().flatten().find_map(|dib| {
        bitmap::decode_dib(&dib)
            .ok()
            .map(|image| bitmap::encode_png(&image))
    }) {
        return pasted_image_from_bytes(&png, ImagePasteSource::Clipboard);
    }
    for path in &files {
        if let Some(image) = load_pasted_image_file(path, ImagePasteSource::ClipboardFile)? {
            return Ok(image);
        }
    }
    Err(if offered_bitmap {
        ImagePasteError::Unreadable
    } else {
        ImagePasteError::NoImage
    })
}

/// Read an image from the system clipboard, if one of the supported types is present.
/// Windows reads the clipboard's raw formats through user32; Wayland tries `wl-paste` then
/// `xclip`; WSL additionally falls through to PowerShell; X11 uses `xclip`; macOS reads through
/// osascript. Termux exposes no clipboard image path.
pub(crate) async fn read_clipboard_image() -> Result<PastedImage, ImagePasteError> {
    // Verification seam (never set by the product): a harness without a display server
    // cannot drive the real clipboard readers, so a fixture file stands in for the
    // clipboard and travels the exact paste path from there.
    if let Some(path) = std::env::var_os("PRIME_AGENT_TEST_CLIPBOARD_IMAGE") {
        let bytes = std::fs::read(path).map_err(|_| ImagePasteError::NoImage)?;
        return pasted_image_from_bytes(&bytes, ImagePasteSource::Clipboard);
    }
    if std::env::var_os("TERMUX_VERSION").is_some() {
        return Err(ImagePasteError::NoImage);
    }
    let bytes = match std::env::consts::OS {
        "windows" => {
            #[cfg(windows)]
            return read_via_windows_clipboard().await;
            #[cfg(not(windows))]
            None
        }
        "linux" => {
            let wsl = is_wsl();
            let wayland = is_wayland_session();
            let mut bytes = None;
            if wayland || wsl {
                bytes = read_via_wl_paste().await;
                if bytes.is_none() {
                    bytes = read_via_xclip().await;
                }
            }
            if bytes.is_none() && wsl {
                bytes = read_via_powershell().await;
            }
            if bytes.is_none() && !wayland {
                bytes = read_via_xclip().await;
            }
            bytes
        }
        "macos" => read_via_osascript().await,
        _ => None,
    };
    pasted_image_from_bytes(
        &bytes.ok_or(ImagePasteError::NoImage)?,
        ImagePasteSource::Clipboard,
    )
}

/// The clipboard's text, for the image-paste key's fallback (Ctrl+V reached the app over a
/// clipboard without an image, so it pastes the text a terminal paste would have): user32 on
/// Windows, `pbpaste` on macOS, `wl-paste` / `xclip` on Linux with PowerShell under WSL. `None`
/// when no reader yields text.
pub(crate) async fn read_clipboard_text() -> Option<String> {
    let bytes = match std::env::consts::OS {
        "windows" => {
            return tokio::task::spawn_blocking(pa_types::platform::read_clipboard_text)
                .await
                .ok()?
                .ok()
                .flatten();
        }
        "macos" => run_command("pbpaste", &[], DEFAULT_READ_TIMEOUT).await,
        "linux" => {
            let mut bytes = None;
            if is_wayland_session() || is_wsl() {
                bytes = run_command("wl-paste", &["--no-newline"], DEFAULT_READ_TIMEOUT).await;
            }
            if bytes.is_none() && is_wsl() {
                bytes = run_command(
                    "powershell.exe",
                    &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
                    DEFAULT_POWERSHELL_TIMEOUT,
                )
                .await;
            }
            if bytes.is_none() {
                bytes = run_command(
                    "xclip",
                    &["-selection", "clipboard", "-o"],
                    DEFAULT_READ_TIMEOUT,
                )
                .await;
            }
            bytes
        }
        _ => None,
    }?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows clipboard read picks the `PNG` format, else the first DIB that decodes (an
    /// unreadable `CF_DIBV5` falls back to `CF_DIB`, converted to PNG), else an Explorer file copy;
    /// a bitmap no reader decodes is unreadable, and an empty clipboard has no image.
    #[test]
    fn the_windows_clipboard_formats_resolve_in_preference_order() {
        use base64::Engine;
        use pa_types::platform::ClipboardImageFormats;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        // A 1x1 bottom-up 24-bit BI_RGB DIB: a BITMAPINFOHEADER, then one BGR pixel padded to 4.
        let mut one_pixel_dib = vec![0u8; 40];
        one_pixel_dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        one_pixel_dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        one_pixel_dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        one_pixel_dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        one_pixel_dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        one_pixel_dib.extend_from_slice(&[0x10, 0x20, 0x30, 0]);
        let converted = bitmap::encode_png(&bitmap::Rgba {
            width: 1,
            height: 1,
            pixels: vec![0x30, 0x20, 0x10, 255],
        });
        let png = b"\x89PNG\r\n\x1a\n-rest".to_vec();
        let dir = tempfile::tempdir().expect("temp dir");
        let copied = dir.path().join("copied.png");
        std::fs::write(&copied, &png).expect("write png");
        let attached = |bytes: &[u8], source| {
            Ok(PastedImage {
                image: ClipboardImage {
                    data: encode(bytes),
                    mime_type: "image/png".to_string(),
                },
                source,
                bytes: bytes.len() as u64,
            })
        };
        let cases = [
            ClipboardImageFormats {
                png: Some(png.clone()),
                dib: Some(one_pixel_dib.clone()),
                ..ClipboardImageFormats::default()
            },
            ClipboardImageFormats {
                dib_v5: Some(vec![124, 0, 0, 0]),
                dib: Some(one_pixel_dib.clone()),
                ..ClipboardImageFormats::default()
            },
            ClipboardImageFormats {
                files: vec![dir.path().join("notes.txt"), copied],
                ..ClipboardImageFormats::default()
            },
            ClipboardImageFormats {
                dib_v5: Some(vec![124, 0, 0, 0]),
                ..ClipboardImageFormats::default()
            },
            ClipboardImageFormats::default(),
        ];
        assert_eq!(
            cases.map(pasted_image_from_windows_formats),
            [
                attached(&png, ImagePasteSource::Clipboard),
                attached(&converted, ImagePasteSource::Clipboard),
                attached(&png, ImagePasteSource::ClipboardFile),
                Err(ImagePasteError::Unreadable),
                Err(ImagePasteError::NoImage),
            ]
        );
    }

    #[test]
    fn base_mime_strips_parameters() {
        assert_eq!(base_mime_type(" image/PNG; charset=utf8 "), "image/png");
    }

    #[test]
    fn supported_type_selection_prefers_png_over_other_images() {
        let types = vec!["text/plain".to_string(), "image/webp".to_string()];
        assert_eq!(
            select_supported_image_mime_type(&types),
            Some("image/webp".to_string())
        );
        let mixed = vec![
            "image/webp".to_string(),
            "image/png".to_string(),
            "image/jpeg".to_string(),
        ];
        assert_eq!(
            select_supported_image_mime_type(&mixed),
            Some("image/png".to_string())
        );
        // An unsupported image type is never selected.
        assert_eq!(
            select_supported_image_mime_type(&["image/bmp".to_string()]),
            None
        );
        assert_eq!(
            select_supported_image_mime_type(&["text/plain".to_string()]),
            None
        );
        assert_eq!(select_supported_image_mime_type(&[]), None);
    }

    #[test]
    fn stdout_lines_trim_and_drop_empty() {
        let stdout = b"image/png\r\ntext/plain\n\n\r\n";
        assert_eq!(stdout_lines(stdout), vec!["image/png", "text/plain"]);
    }

    /// A pasted path attaches only a supported image within the size limit: other files stay text
    /// (`Ok(None)`), and an oversized image reports its size.
    #[test]
    fn a_pasted_image_file_loads_within_the_limit() {
        use base64::Engine;
        let dir = tempfile::tempdir().expect("temp dir");
        let png = b"\x89PNG\r\n\x1a\n-rest".to_vec();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, &png).expect("write png");
        let notes = dir.path().join("notes.png");
        std::fs::write(&notes, b"not an image").expect("write notes");
        let huge = dir.path().join("huge.png");
        let mut huge_bytes = png.clone();
        huge_bytes.resize(
            usize::try_from(MAX_PASTED_IMAGE_BYTES).expect("fits") + 1,
            0,
        );
        std::fs::write(&huge, &huge_bytes).expect("write huge");
        let source = ImagePasteSource::Path;
        assert_eq!(
            [
                load_pasted_image_file(&image, source),
                load_pasted_image_file(&notes, source),
                load_pasted_image_file(&dir.path().join("missing.png"), source),
                load_pasted_image_file(&huge, source),
            ],
            [
                Ok(Some(PastedImage {
                    image: ClipboardImage {
                        data: base64::engine::general_purpose::STANDARD.encode(&png),
                        mime_type: "image/png".to_string(),
                    },
                    source,
                    bytes: png.len() as u64,
                })),
                Ok(None),
                Ok(None),
                Err(ImagePasteError::TooLarge {
                    bytes: MAX_PASTED_IMAGE_BYTES + 1,
                }),
            ]
        );
        assert_eq!(
            ImagePasteError::TooLarge {
                bytes: 30 * 1024 * 1024
            }
            .message(),
            "Image too large: 30.0 MB (limit 20.0 MB)."
        );
        assert_eq!(
            [format_image_size(69), format_image_size(1536 * 1024)],
            ["1 KB".to_string(), "1.5 MB".to_string()]
        );
    }

    #[test]
    fn unique_suffix_is_unique() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
    }
}
