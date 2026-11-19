//! The terminal's answers to the inline-image capability query: the DA1 reply's sixel attribute
//! and the `CSI 16 t` cell pixel size. The query is lazy: it is decided when the first inline
//! image is about to render ([`request_query`], from the live frame's row resolve), and only
//! where a pixel protocol is a candidate for this terminal (its environment or DA1 answer names
//! one) while the cell pixel size is still unknown (no pty pixel size, no fixed Windows
//! Terminal sixel cell) — and on native Windows, where Windows Terminal as the default
//! terminal advertises nothing, whenever no protocol is known yet (DA1 is asked then). On a
//! Unix tty a terminal or harness that advertises nothing is never asked, and no startup or
//! first-paint path writes it anywhere. Until an answer arrives the image keeps its
//! half-blocks; a recorded answer wakes a repaint, which switches it. The frame paint writes a queued query after its own
//! bytes ([`take_pending_query`]); the answers arrive on the input path, never awaited:
//! - on a Unix tty the vendored crossterm parser records them as it reads
//!   (`crossterm::terminal::reported_sixel_graphics` / `reported_cell_pixels`);
//! - a reply the reader split, and every reply on the Windows console (which relays a VT reply
//!   as one key record per character), is reassembled by the sequence guard, which hands it to
//!   [`note_reply`].
//!
//! The paint path reads the recorded answers with atomic loads.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use super::detect::{select_protocol, CellReports, ImageProtocol, Platform, TerminalHints};

/// The cell pixel size query (`CSI 16 t`). On a Unix tty the keyboard probe already asks for
/// DA1 (`enhanced_keys`).
const CELL_QUERY: &[u8] = b"\x1b[16t";
/// The cell pixel size and DA1, for the Windows console where no keyboard probe asks DA1 and
/// only the terminal's own answer can confirm sixel.
const CELL_AND_DA1_QUERY: &[u8] = b"\x1b[16t\x1b[c";

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
            // The shown images may switch protocol or scale: repaint them.
            super::store::wake();
            true
        }
        Some(Reply::CellPixels { width, height }) => {
            CELL_PIXELS.store(
                u32::from(width) << 16 | u32::from(height),
                Ordering::Relaxed,
            );
            super::store::wake();
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

/// The query a terminal needs before its inline images can draw at its pixel scale, or `None`
/// when nothing is to be asked: no pixel protocol is a candidate (a multiplexer pane, a plain
/// terminal or harness that advertises nothing), or the cell size is already known.
pub(crate) fn query_needed(
    hints: &TerminalHints<'_>,
    platform: Platform,
    da1_sixel: Option<bool>,
    reports: CellReports,
) -> Option<&'static [u8]> {
    let selected = select_protocol(hints, platform, da1_sixel);
    // On native Windows the environment names Windows Terminal only in its own tabs: a console
    // it took over as the default terminal, and `WezTerm`'s bundled pseudoconsole, advertise
    // nothing. With no protocol known yet, the first image asks DA1 (the console host answers
    // for itself without sixel where graphics cannot arrive). A pinned protocol or a
    // multiplexer is the answer already.
    let unconfirmed = platform == Platform::Windows
        && selected == ImageProtocol::HalfBlock
        && da1_sixel.is_none()
        && hints.override_protocol.is_none()
        && hints.tmux.is_none()
        && hints.sty.is_none()
        && hints.zellij.is_none();
    if selected == ImageProtocol::HalfBlock && !unconfirmed {
        return None;
    }
    let fixed_cell = selected == ImageProtocol::Sixel && hints.wt_session.is_some();
    let known = fixed_cell || reports.winsize.or(reports.reply).is_some();
    match (unconfirmed, known) {
        (true, _) => Some(CELL_AND_DA1_QUERY),
        (false, true) => None,
        (false, false) => Some(CELL_QUERY),
    }
}

/// The queued query: `0` none, `1` the cell size, `2` the cell size and DA1.
static PENDING: AtomicU8 = AtomicU8::new(0);

/// Decide the query for an inline image about to render (the live frame calls this): queued
/// once per process, when [`query_needed`] asks for one.
pub(crate) fn request_query() {
    if QUERIED.load(Ordering::Relaxed) {
        return;
    }
    let reports = super::live_cell_reports();
    let needed = query_needed(
        &super::detect::process_hints(),
        super::detect::PLATFORM,
        sixel_attribute(),
        reports,
    );
    if let Some(query) = needed {
        if !QUERIED.swap(true, Ordering::SeqCst) {
            PENDING.store(
                if query == CELL_AND_DA1_QUERY { 2 } else { 1 },
                Ordering::SeqCst,
            );
        }
    }
}

/// The queued query's bytes for the frame paint to write (taken once), and only while raw mode
/// holds: a cooked tty would echo the answer onto the screen.
pub(crate) fn take_pending_query() -> Option<&'static [u8]> {
    if PENDING.load(Ordering::Relaxed) == 0
        || !crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
    {
        return None;
    }
    match PENDING.swap(0, Ordering::SeqCst) {
        1 => Some(CELL_QUERY),
        2 => Some(CELL_AND_DA1_QUERY),
        _ => None,
    }
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

    /// The lifecycle PTY harness shape (`TERM=xterm-256color`, no pixel size, a DA1 answer
    /// without sixel or none) and every other terminal that advertises nothing is never asked;
    /// a candidate whose cell size is already known is not asked either.
    #[test]
    fn only_a_pixel_candidate_with_an_unknown_cell_size_is_asked() {
        let none = CellReports::default();
        let winsize = CellReports {
            winsize: Some((9, 18)),
            reply: None,
        };
        let plain = TerminalHints {
            term: Some("xterm-256color"),
            ..TerminalHints::default()
        };
        let kitty = TerminalHints {
            kitty_window_id: Some("1"),
            ..TerminalHints::default()
        };
        let windows_terminal = TerminalHints {
            wt_session: Some("5b1e"),
            ..TerminalHints::default()
        };
        let wezterm = TerminalHints {
            wezterm_pane: Some("0"),
            ..TerminalHints::default()
        };
        let tmux = TerminalHints {
            tmux: Some("/tmp/tmux-0/default,1,0"),
            ..kitty
        };
        let cases = [
            ("harness, no DA1", plain, Platform::Unix, None, none),
            (
                "harness, DA1 without sixel",
                plain,
                Platform::Unix,
                Some(false),
                none,
            ),
            // The console host already answered DA1 for itself: no sixel, nothing more to ask.
            (
                "conhost",
                TerminalHints::default(),
                Platform::Windows,
                Some(false),
                none,
            ),
            ("tmux inside kitty", tmux, Platform::Unix, Some(true), none),
            (
                "kitty with a pixel size",
                kitty,
                Platform::Unix,
                None,
                winsize,
            ),
            (
                "windows terminal hosting WSL",
                windows_terminal,
                Platform::Unix,
                None,
                none,
            ),
            (
                "windows terminal",
                windows_terminal,
                Platform::Windows,
                None,
                none,
            ),
            ("kitty over mosh", kitty, Platform::Unix, None, none),
            (
                "vscode with images",
                plain,
                Platform::Unix,
                Some(true),
                none,
            ),
            ("wezterm on windows", wezterm, Platform::Windows, None, none),
            // Windows Terminal as the default terminal: nothing advertised, DA1 not yet heard.
            (
                "native windows, nothing advertised",
                TerminalHints::default(),
                Platform::Windows,
                None,
                none,
            ),
            (
                "native windows, DA1 heard with sixel",
                TerminalHints::default(),
                Platform::Windows,
                Some(true),
                none,
            ),
        ];
        assert_eq!(
            cases.map(|(name, hints, platform, da1, reports)| (
                name,
                query_needed(&hints, platform, da1, reports)
            )),
            [
                ("harness, no DA1", None),
                ("harness, DA1 without sixel", None),
                ("conhost", None),
                ("tmux inside kitty", None),
                ("kitty with a pixel size", None),
                ("windows terminal hosting WSL", None),
                ("windows terminal", None),
                ("kitty over mosh", Some(CELL_QUERY)),
                ("vscode with images", Some(CELL_QUERY)),
                ("wezterm on windows", Some(CELL_AND_DA1_QUERY)),
                (
                    "native windows, nothing advertised",
                    Some(CELL_AND_DA1_QUERY)
                ),
                ("native windows, DA1 heard with sixel", Some(CELL_QUERY)),
            ]
        );
    }
}
