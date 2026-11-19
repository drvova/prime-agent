//! The terminal's answers to the inline-image capability query: the DA1 reply's sixel attribute
//! and the `CSI 16 t` cell pixel size. The query goes out once per process at the first surface
//! start ([`take_capability_query`], written by `enhanced_keys::enable` while raw mode holds);
//! the answers arrive on the input path, never awaited:
//! - on a Unix tty the vendored crossterm parser records them as it reads
//!   (`crossterm::terminal::reported_sixel_graphics` / `reported_cell_pixels`);
//! - a reply the reader split, and every reply on the Windows console (which relays a VT reply
//!   as one key record per character), is reassembled by the sequence guard, which hands it to
//!   [`note_reply`].
//!
//! The paint path reads the recorded answers with two atomic loads.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

/// The query on a Unix tty: the cell pixel size (`CSI 16 t`). The keyboard probe already asks
/// for DA1 (`enhanced_keys`), so asking again would only duplicate its reply.
const UNIX_QUERY: &[u8] = b"\x1b[16t";
/// The query on the Windows console: the cell pixel size and DA1. No keyboard probe runs there,
/// so DA1 is asked here; the console host answers for itself where it renders the screen
/// (`ESC [ ? 1 ; 0 c`, no sixel) and relays the terminal's answer under a pseudoconsole.
const WINDOWS_QUERY: &[u8] = b"\x1b[16t\x1b[c";

/// One reply the input path recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reply {
    /// The primary device attributes (`CSI ? <class> ; <attr> ... c`): whether attribute 4
    /// (sixel graphics) is listed.
    DeviceAttributes { sixel: bool },
    /// The character cell size in pixels (`CSI 6 ; <height> ; <width> t`).
    CellPixels { width: u16, height: u16 },
}

/// Parse one complete escape sequence as a capability reply (`None` for anything else).
pub(crate) fn parse_reply(bytes: &[u8]) -> Option<Reply> {
    let body = std::str::from_utf8(bytes.strip_prefix(b"\x1b[")?).ok()?;
    if let Some(attributes) = body
        .strip_prefix('?')
        .and_then(|rest| rest.strip_suffix('c'))
    {
        let mut fields = attributes.split(';');
        fields.next()?.parse::<u16>().ok()?;
        return Some(Reply::DeviceAttributes {
            sixel: fields.any(|attribute| attribute == "4"),
        });
    }
    let mut fields = body.strip_suffix('t')?.split(';');
    if fields.next()? != "6" {
        return None;
    }
    let height = fields.next()?.parse::<u16>().ok()?;
    let width = fields.next()?.parse::<u16>().ok()?;
    (fields.next().is_none() && width > 0 && height > 0)
        .then_some(Reply::CellPixels { width, height })
}

/// The guard-heard DA1 sixel attribute: `0` none yet, `1` without, `2` with sixel.
static DA1_SIXEL: AtomicU8 = AtomicU8::new(0);
/// The guard-heard cell size, `width << 16 | height` (`0` none yet).
static CELL_PIXELS: AtomicU32 = AtomicU32::new(0);
/// The capability query went out (once per process).
static QUERIED: AtomicBool = AtomicBool::new(false);

/// Record `bytes` when they are a capability reply; returns whether they were one.
pub(crate) fn note_reply(bytes: &[u8]) -> bool {
    match parse_reply(bytes) {
        Some(Reply::DeviceAttributes { sixel }) => {
            DA1_SIXEL.store(if sixel { 2 } else { 1 }, Ordering::Relaxed);
            true
        }
        Some(Reply::CellPixels { width, height }) => {
            CELL_PIXELS.store(
                u32::from(width) << 16 | u32::from(height),
                Ordering::Relaxed,
            );
            true
        }
        None => false,
    }
}

/// The DA1 reply's sixel attribute, `None` until a reply arrived.
pub(crate) fn sixel_attribute() -> Option<bool> {
    match DA1_SIXEL.load(Ordering::Relaxed) {
        0 => crossterm::terminal::reported_sixel_graphics(),
        reported => Some(reported == 2),
    }
}

/// The `CSI 16 t` reply's cell size as `(width, height)`, `None` until a reply arrived.
pub(crate) fn cell_pixels() -> Option<(u16, u16)> {
    match CELL_PIXELS.load(Ordering::Relaxed) {
        0 => crossterm::terminal::reported_cell_pixels(),
        packed => Some(((packed >> 16) as u16, (packed & 0xffff) as u16)),
    }
}

/// The capability query to write at the first surface start, once per process; `None` after
/// the first call, and wherever no answer could enable a pixel protocol (a multiplexer pane).
pub(crate) fn take_capability_query() -> Option<&'static [u8]> {
    if !super::detect::pixel_protocol_possible() || QUERIED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(if cfg!(windows) {
        WINDOWS_QUERY
    } else {
        UNIX_QUERY
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_replies_parse_and_everything_else_does_not() {
        let replies = [
            // Windows Terminal 1.22's DA1 and its virtual sixel cell.
            &b"\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c"[..],
            b"\x1b[6;20;10t",
            // The inbox console host answering for itself.
            b"\x1b[?1;0c",
            // A sixel class without attribute 4 in the attribute list proper.
            b"\x1b[?4;6c",
            // The text-area report (`CSI 14 t`), a zero cell, a cursor report, a key.
            b"\x1b[4;540;1200t",
            b"\x1b[6;0;10t",
            b"\x1b[2;1R",
            b"\x1b[A",
        ];
        assert_eq!(
            replies.map(parse_reply),
            [
                Some(Reply::DeviceAttributes { sixel: true }),
                Some(Reply::CellPixels {
                    width: 10,
                    height: 20
                }),
                Some(Reply::DeviceAttributes { sixel: false }),
                Some(Reply::DeviceAttributes { sixel: false }),
                None,
                None,
                None,
                None,
            ]
        );
    }
}
