//! The inline-image protocol choice: which pixel protocol (if any) this terminal renders,
//! decided from the environment hints the terminal exports and the DA1 reply the keyboard
//! probe already requests (`enhanced_keys`). The half-block cells are always painted as the
//! base layer, so the choice only decides what draws OVER them: a protocol a terminal ignores
//! leaves the half-blocks visible, while one it does not parse would print as text — the rule
//! is therefore to name a protocol only where the terminal is known to consume it.
//!
//! The per-terminal facts, from each terminal's own source (cited in the lane's PR):
//! - kitty (`KITTY_WINDOW_ID`, `TERM=xterm-kitty`), Ghostty (`GHOSTTY_RESOURCES_DIR`,
//!   `TERM=xterm-ghostty`) and `WezTerm` (`WEZTERM_PANE`) implement the kitty graphics protocol.
//! - iTerm2 (`ITERM_SESSION_ID`, `LC_TERMINAL=iTerm2`) and mintty (`TERM_PROGRAM=mintty`)
//!   implement the iTerm2 `OSC 1337 ; File=` inline image.
//! - Warp implements kitty graphics on macOS/Linux only; its Windows build (and WSL shells it
//!   hosts) does not.
//! - Windows Terminal sets `WT_SESSION` (forwarded into WSL through `WSLENV`), renders sixel,
//!   and its `ConPTY` passes the application's VT stream through to it unmodified; a Windows
//!   Terminal older than sixel support parses the DCS string and drops it, leaving the
//!   half-blocks.
//! - foot (`TERM=foot`) and mlterm (`MLTERM`) render sixel; any other terminal whose DA1
//!   reply lists attribute 4 (xterm built with sixel, Konsole, contour, ...) does too.
//! - A multiplexer (tmux, screen, zellij) owns the grid: its pane only forwards graphics with
//!   explicit passthrough configuration, so the half-blocks are the only safe paint there.
//! - A native Windows console program under any other host (Git Bash's mintty, conhost)
//!   reaches it through the inbox console host, which re-renders the screen: graphics never
//!   arrive, so the half-blocks are the paint there.

use std::sync::OnceLock;

/// The pixel protocol that draws over an inline image's half-block cells, or the half-block
/// cells alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImageProtocol {
    /// The kitty graphics protocol (APC `G`): transmitted once by id, placed per frame.
    Kitty,
    /// The iTerm2 inline image (`OSC 1337 ; File=`): cell-bound, re-sent when its cells repaint.
    Iterm2,
    /// DEC sixel graphics (DCS `q`): cell-bound, re-sent when its cells repaint.
    Sixel,
    /// No pixel protocol: the Unicode half-block cells are the image.
    HalfBlock,
}

impl ImageProtocol {
    /// The telemetry / override name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::HalfBlock => "half_block",
        }
    }

    /// Whether the terminal stores this protocol's pixels in the cells they cover, so a cell
    /// write over them erases the picture (sixel, iTerm2) — the kitty layer sits above the
    /// cells instead.
    pub(crate) fn cell_bound(self) -> bool {
        match self {
            Self::Iterm2 | Self::Sixel => true,
            Self::Kitty | Self::HalfBlock => false,
        }
    }
}

/// The platform the binary runs on: a native Windows console program reaches its terminal
/// through a console host, everything else through a pty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    Unix,
}

/// The environment hints the protocol choice reads (each the variable's value, when set).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TerminalHints<'a> {
    pub(crate) override_protocol: Option<&'a str>,
    pub(crate) term: Option<&'a str>,
    pub(crate) term_program: Option<&'a str>,
    pub(crate) lc_terminal: Option<&'a str>,
    pub(crate) kitty_window_id: Option<&'a str>,
    pub(crate) ghostty_resources_dir: Option<&'a str>,
    pub(crate) wezterm_pane: Option<&'a str>,
    pub(crate) iterm_session_id: Option<&'a str>,
    pub(crate) wt_session: Option<&'a str>,
    pub(crate) mlterm: Option<&'a str>,
    pub(crate) wsl_distro_name: Option<&'a str>,
    pub(crate) tmux: Option<&'a str>,
    pub(crate) sty: Option<&'a str>,
    pub(crate) zellij: Option<&'a str>,
}

/// The environment variable that pins the protocol: `kitty`, `iterm2`, `sixel`, or
/// `halfblock` (an unknown value pins the half-blocks).
pub(crate) const OVERRIDE_VAR: &str = "PRIME_AGENT_IMAGE_PROTOCOL";

/// Choose the protocol for one terminal. `da1_sixel` is the DA1 reply's sixel attribute
/// (`None` until a reply arrived).
pub(crate) fn select_protocol(
    hints: &TerminalHints<'_>,
    platform: Platform,
    da1_sixel: Option<bool>,
) -> ImageProtocol {
    if let Some(name) = hints.override_protocol {
        return match name.trim().to_ascii_lowercase().as_str() {
            "kitty" => ImageProtocol::Kitty,
            "iterm2" | "iterm" => ImageProtocol::Iterm2,
            "sixel" => ImageProtocol::Sixel,
            _ => ImageProtocol::HalfBlock,
        };
    }
    let term = hints.term.unwrap_or_default().to_ascii_lowercase();
    let program = hints.term_program.unwrap_or_default().to_ascii_lowercase();
    if hints.tmux.is_some()
        || hints.sty.is_some()
        || hints.zellij.is_some()
        || term.starts_with("tmux")
        || term.starts_with("screen")
    {
        return ImageProtocol::HalfBlock;
    }
    if platform == Platform::Windows {
        return if hints.wt_session.is_some() {
            ImageProtocol::Sixel
        } else {
            ImageProtocol::HalfBlock
        };
    }
    if program == "warpterminal" {
        return if hints.wsl_distro_name.is_some() {
            ImageProtocol::HalfBlock
        } else {
            ImageProtocol::Kitty
        };
    }
    if hints.kitty_window_id.is_some()
        || program == "kitty"
        || term == "xterm-kitty"
        || hints.ghostty_resources_dir.is_some()
        || program == "ghostty"
        || term.contains("ghostty")
        || hints.wezterm_pane.is_some()
        || program == "wezterm"
    {
        return ImageProtocol::Kitty;
    }
    if hints.iterm_session_id.is_some()
        || program == "iterm.app"
        || hints
            .lc_terminal
            .is_some_and(|value| value.eq_ignore_ascii_case("iterm2"))
        || program == "mintty"
    {
        return ImageProtocol::Iterm2;
    }
    if hints.wt_session.is_some()
        || term == "foot"
        || term.starts_with("foot-")
        || hints.mlterm.is_some()
        || term.starts_with("mlterm")
        || da1_sixel == Some(true)
    {
        return ImageProtocol::Sixel;
    }
    ImageProtocol::HalfBlock
}

/// The process's environment-derived protocol: the hints never change within a process, and
/// the DA1 answer is read live (the probe may answer after the first frame).
pub(crate) fn active_protocol() -> ImageProtocol {
    static HINTED: OnceLock<[ImageProtocol; 2]> = OnceLock::new();
    let [without_da1, with_da1] = *HINTED.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        let values: Vec<Option<String>> = [
            OVERRIDE_VAR,
            "TERM",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "ITERM_SESSION_ID",
            "WT_SESSION",
            "MLTERM",
            "WSL_DISTRO_NAME",
            "TMUX",
            "STY",
            "ZELLIJ",
        ]
        .into_iter()
        .map(var)
        .collect();
        let at = |index: usize| values[index].as_deref();
        let hints = TerminalHints {
            override_protocol: at(0),
            term: at(1),
            term_program: at(2),
            lc_terminal: at(3),
            kitty_window_id: at(4),
            ghostty_resources_dir: at(5),
            wezterm_pane: at(6),
            iterm_session_id: at(7),
            wt_session: at(8),
            mlterm: at(9),
            wsl_distro_name: at(10),
            tmux: at(11),
            sty: at(12),
            zellij: at(13),
        };
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Unix
        };
        [
            select_protocol(&hints, platform, None),
            select_protocol(&hints, platform, Some(true)),
        ]
    });
    let protocol = match crossterm::terminal::reported_sixel_graphics() {
        Some(true) => with_da1,
        Some(false) | None => without_da1,
    };
    // Sixel draws at the terminal's own pixel scale: without a known cell size the picture
    // would not fill (or would overflow) its reserved rows.
    match protocol {
        ImageProtocol::Sixel if super::cell_pixels(protocol).is_none() => ImageProtocol::HalfBlock,
        ImageProtocol::Kitty
        | ImageProtocol::Iterm2
        | ImageProtocol::Sixel
        | ImageProtocol::HalfBlock => protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(hints: TerminalHints<'_>) -> ImageProtocol {
        select_protocol(&hints, Platform::Unix, None)
    }

    #[test]
    fn each_terminal_gets_the_protocol_its_source_implements() {
        let cases: Vec<(&str, TerminalHints<'_>, ImageProtocol)> = vec![
            (
                "kitty",
                TerminalHints {
                    kitty_window_id: Some("1"),
                    term: Some("xterm-kitty"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Kitty,
            ),
            (
                "ghostty",
                TerminalHints {
                    ghostty_resources_dir: Some("/usr/share/ghostty"),
                    term: Some("xterm-ghostty"),
                    term_program: Some("ghostty"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Kitty,
            ),
            (
                "wezterm",
                TerminalHints {
                    wezterm_pane: Some("0"),
                    term_program: Some("WezTerm"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Kitty,
            ),
            (
                "warp (macOS/Linux)",
                TerminalHints {
                    term_program: Some("WarpTerminal"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Kitty,
            ),
            (
                "warp hosting WSL",
                TerminalHints {
                    term_program: Some("WarpTerminal"),
                    wsl_distro_name: Some("Ubuntu"),
                    ..TerminalHints::default()
                },
                ImageProtocol::HalfBlock,
            ),
            (
                "iterm2 over ssh",
                TerminalHints {
                    lc_terminal: Some("iTerm2"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Iterm2,
            ),
            (
                "mintty (wsltty, cygwin)",
                TerminalHints {
                    term_program: Some("mintty"),
                    term: Some("xterm"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Iterm2,
            ),
            (
                "windows terminal hosting WSL",
                TerminalHints {
                    wt_session: Some("5b1e"),
                    term: Some("xterm-256color"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Sixel,
            ),
            (
                "foot",
                TerminalHints {
                    term: Some("foot"),
                    ..TerminalHints::default()
                },
                ImageProtocol::Sixel,
            ),
            (
                "tmux inside kitty",
                TerminalHints {
                    tmux: Some("/tmp/tmux-0/default,1,0"),
                    kitty_window_id: Some("1"),
                    term: Some("tmux-256color"),
                    ..TerminalHints::default()
                },
                ImageProtocol::HalfBlock,
            ),
            (
                "an unknown xterm-256color",
                TerminalHints {
                    term: Some("xterm-256color"),
                    ..TerminalHints::default()
                },
                ImageProtocol::HalfBlock,
            ),
        ];
        let picked: Vec<(&str, ImageProtocol)> = cases
            .iter()
            .map(|(name, hints, _)| (*name, pick(*hints)))
            .collect();
        let expected: Vec<(&str, ImageProtocol)> = cases
            .iter()
            .map(|(name, _, protocol)| (*name, *protocol))
            .collect();
        assert_eq!(picked, expected);
    }

    #[test]
    fn a_native_windows_console_program_paints_sixel_only_under_windows_terminal() {
        let windows_terminal = TerminalHints {
            wt_session: Some("5b1e"),
            ..TerminalHints::default()
        };
        // Git Bash's mintty advertises OSC 1337 to its own programs, but a native console
        // program reaches it through the inbox console host: the sequence never arrives.
        let git_bash = TerminalHints {
            term_program: Some("mintty"),
            term: Some("xterm"),
            ..TerminalHints::default()
        };
        let warp = TerminalHints {
            term_program: Some("WarpTerminal"),
            ..TerminalHints::default()
        };
        let ghostty_like = TerminalHints {
            kitty_window_id: Some("1"),
            ..TerminalHints::default()
        };
        assert_eq!(
            [windows_terminal, git_bash, warp, ghostty_like].map(|hints| select_protocol(
                &hints,
                Platform::Windows,
                None
            )),
            [
                ImageProtocol::Sixel,
                ImageProtocol::HalfBlock,
                ImageProtocol::HalfBlock,
                ImageProtocol::HalfBlock,
            ]
        );
    }

    #[test]
    fn a_da1_sixel_attribute_upgrades_an_unknown_terminal_and_the_override_pins() {
        let xterm = TerminalHints {
            term: Some("xterm-256color"),
            ..TerminalHints::default()
        };
        assert_eq!(
            [None, Some(false), Some(true)].map(|da1| select_protocol(&xterm, Platform::Unix, da1)),
            [
                ImageProtocol::HalfBlock,
                ImageProtocol::HalfBlock,
                ImageProtocol::Sixel
            ]
        );
        let pinned = |value| TerminalHints {
            override_protocol: Some(value),
            tmux: Some("x"),
            ..TerminalHints::default()
        };
        assert_eq!(
            ["kitty", "ITERM2", "sixel", "halfblock", "bogus"].map(|value| select_protocol(
                &pinned(value),
                Platform::Windows,
                None
            )),
            [
                ImageProtocol::Kitty,
                ImageProtocol::Iterm2,
                ImageProtocol::Sixel,
                ImageProtocol::HalfBlock,
                ImageProtocol::HalfBlock,
            ]
        );
    }
}
