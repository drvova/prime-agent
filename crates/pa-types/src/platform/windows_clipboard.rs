//! The Windows clipboard's image payloads, read through user32 for the TUI's image paste. pa-tui
//! opts into the workspace `unsafe_code` forbid, so the FFI lives behind this wall (pinned Win32
//! constants and hand-declared externs, no windows-sys dependency - the same policy as the console
//! and named-pipe walls). The bytes come back raw: decoding a bitmap is the caller's concern.

use std::path::PathBuf;

/// The image payloads the clipboard offers, each copied out while the clipboard was open. Fields
/// the clipboard does not offer stay empty; the DIB pair is read only without a `png`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardImageFormats {
    /// The registered `PNG` format (browsers, Office, the Snipping Tool): a PNG file's bytes.
    pub png: Option<Vec<u8>>,
    /// `CF_DIBV5`: a packed `BITMAPV5HEADER` bitmap (alpha-capable). The system synthesizes it
    /// from a `CF_BITMAP`/`CF_DIB` copy.
    pub dib_v5: Option<Vec<u8>>,
    /// `CF_DIB`: a packed `BITMAPINFOHEADER` bitmap, the fallback for a V5 payload a decoder
    /// cannot read.
    pub dib: Option<Vec<u8>>,
    /// `CF_HDROP`: the files an Explorer copy names.
    pub files: Vec<PathBuf>,
}

/// Read the clipboard's image payloads. Off Windows every field is empty (other platforms read
/// their clipboards through their own tools).
///
/// # Errors
/// The clipboard stays locked by another program after a short retry window.
pub fn read_clipboard_image_formats() -> std::io::Result<ClipboardImageFormats> {
    #[cfg(windows)]
    {
        win32::read()
    }
    #[cfg(not(windows))]
    {
        Ok(ClipboardImageFormats::default())
    }
}

/// The clipboard's text (`CF_UNICODETEXT`), for the image-paste key's text fallback: `None` when
/// the clipboard holds no text, and always off Windows.
///
/// # Errors
/// The clipboard stays locked by another program after a short retry window.
pub fn read_clipboard_text() -> std::io::Result<Option<String>> {
    #[cfg(windows)]
    {
        win32::read_text()
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

#[cfg(windows)]
mod win32 {
    // The Win32 header names stay verbatim: the pinned-constants wall mirrors them.
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::ClipboardImageFormats;

    /// `winuser.h` `CF_DIB`.
    const CF_DIB: u32 = 8;
    /// `winuser.h` `CF_UNICODETEXT`.
    const CF_UNICODETEXT: u32 = 13;
    /// `winuser.h` `CF_HDROP`.
    const CF_HDROP: u32 = 15;
    /// `winuser.h` `CF_DIBV5`.
    const CF_DIBV5: u32 = 17;
    /// `shellapi.h` `DragQueryFileW`'s "count the files" index.
    const DRAG_QUERY_COUNT: u32 = 0xFFFF_FFFF;
    /// `OpenClipboard` fails while another program holds the clipboard: retry this many times.
    const OPEN_ATTEMPTS: u32 = 10;
    /// The pause between `OpenClipboard` attempts.
    const OPEN_RETRY_DELAY: Duration = Duration::from_millis(20);

    type Handle = *mut c_void;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: Handle) -> i32;
        fn CloseClipboard() -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> Handle;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }

    extern "system" {
        fn GlobalLock(memory: Handle) -> *mut c_void;
        fn GlobalUnlock(memory: Handle) -> i32;
        fn GlobalSize(memory: Handle) -> usize;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn DragQueryFileW(drop: Handle, index: u32, file: *mut u16, len: u32) -> u32;
    }

    /// The open clipboard; closing it on drop keeps every early return balanced.
    struct OpenClipboardGuard;

    // The readers take `&self` only as proof the clipboard is open: Win32 keys the open clipboard
    // to the thread, not to a handle, so the receiver carries no data.
    #[allow(clippy::unused_self)]
    impl OpenClipboardGuard {
        fn open() -> std::io::Result<Self> {
            for attempt in 0..OPEN_ATTEMPTS {
                // SAFETY: a null owner opens the clipboard for this thread; no pointer is passed.
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Ok(Self);
                }
                if attempt + 1 < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_RETRY_DELAY);
                }
            }
            Err(std::io::Error::last_os_error())
        }

        /// A copy of one format's global-memory block, `None` when the clipboard lacks it.
        fn bytes(&self, format: u32) -> Option<Vec<u8>> {
            // SAFETY: the clipboard is open (`self`); the handle belongs to the clipboard and is
            // only locked, read within `GlobalSize`, and unlocked - never freed.
            unsafe {
                if IsClipboardFormatAvailable(format) == 0 {
                    return None;
                }
                let handle = GetClipboardData(format);
                if handle.is_null() {
                    return None;
                }
                let size = GlobalSize(handle);
                let data = GlobalLock(handle);
                if data.is_null() {
                    return None;
                }
                let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
                GlobalUnlock(handle);
                (!bytes.is_empty()).then_some(bytes)
            }
        }

        /// The paths a `CF_HDROP` block names.
        fn files(&self) -> Vec<PathBuf> {
            // SAFETY: the clipboard is open (`self`); `DragQueryFileW` writes at most `len`
            // UTF-16 units (terminator included) into the buffer sized for it.
            unsafe {
                if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                    return Vec::new();
                }
                let drop = GetClipboardData(CF_HDROP);
                if drop.is_null() {
                    return Vec::new();
                }
                let count = DragQueryFileW(drop, DRAG_QUERY_COUNT, std::ptr::null_mut(), 0);
                (0..count)
                    .filter_map(|index| {
                        let len = DragQueryFileW(drop, index, std::ptr::null_mut(), 0);
                        let mut buffer = vec![0u16; len as usize + 1];
                        let written = DragQueryFileW(drop, index, buffer.as_mut_ptr(), len + 1);
                        (written > 0).then(|| {
                            buffer.truncate(written as usize);
                            PathBuf::from(std::ffi::OsString::from_wide(&buffer))
                        })
                    })
                    .collect()
            }
        }
    }

    impl Drop for OpenClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: balances the successful `OpenClipboard` in `open`.
            unsafe {
                CloseClipboard();
            }
        }
    }

    /// The registered `PNG` clipboard format's id (the same id in every process of a session).
    fn png_format() -> u32 {
        let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    pub(super) fn read() -> std::io::Result<ClipboardImageFormats> {
        let png_format = png_format();
        let clipboard = OpenClipboardGuard::open()?;
        let png = (png_format != 0)
            .then(|| clipboard.bytes(png_format))
            .flatten();
        let (dib_v5, dib) = if png.is_some() {
            (None, None)
        } else {
            (clipboard.bytes(CF_DIBV5), clipboard.bytes(CF_DIB))
        };
        Ok(ClipboardImageFormats {
            png,
            dib_v5,
            dib,
            files: clipboard.files(),
        })
    }

    pub(super) fn read_text() -> std::io::Result<Option<String>> {
        let clipboard = OpenClipboardGuard::open()?;
        Ok(clipboard.bytes(CF_UNICODETEXT).map(|bytes| {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|&unit| unit != 0)
                .collect();
            String::from_utf16_lossy(&units)
        }))
    }
}
