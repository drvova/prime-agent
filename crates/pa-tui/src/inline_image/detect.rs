//! The inline-image protocol choice: which pixel protocol (if any) this terminal renders,
//! decided from the environment hints the terminal exports and the DA1 reply (requested by the
//! keyboard probe on a Unix tty, by the capability query on Windows; see [`super::reports`]),
//! and the cell pixel size the payload is scaled to ([`cell_size`]). The half-block cells are always painted as the
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
//!   arrive, so the half-blocks are the paint there. That host answers DA1 for itself without
//!   sixel, while a host that relays the terminal's own answer (Windows Terminal taking a
//!   console over as the default terminal, which sets no `WT_SESSION`; `WezTerm`'s bundled
//!   pseudoconsole) lists sixel when the terminal draws it.

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
        // Windows Terminal's own tabs carry `WT_SESSION`; a console it took over as the default
        // terminal does not, and neither does another host relaying through its pseudoconsole
        // (WezTerm): their DA1 answer is the signal. The console host answering for itself
        // lists no sixel.
        return if hints.wt_session.is_some() || da1_sixel == Some(true) {
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

/// The cell pixel sizes the terminal reported, per source (each `(width, height)`, `None` when
/// the source gave nothing).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellReports {
    /// The pty's `TIOCGWINSZ` pixel size over its cell count (Unix only; many ptys leave it 0).
    pub(crate) winsize: Option<(u16, u16)>,
    /// The terminal's answer to the `CSI 16 t` query ([`super::reports`]).
    pub(crate) reply: Option<(u16, u16)>,
}

/// The pixel size one cell of `protocol`'s payload is scaled to, or `None` when sixel (which
/// draws at the terminal's own pixel scale) has no size it can trust.
///
/// A reported size wins (the pty's pixel size, else the `CSI 16 t` answer). Windows Terminal
/// draws sixel on a fixed 10x20 virtual cell, scaled to its font, so its sixel cell is known
/// without a report — natively, and from WSL where its pty carries no pixel size. Native
/// Windows and a pinned protocol fall back to that same scale. A sixel terminal that reports
/// nothing else keeps the half-blocks: guessed too large, the picture would overflow its rows
/// and could scroll the screen.
pub(crate) fn cell_size(
    protocol: ImageProtocol,
    hints: &TerminalHints<'_>,
    platform: Platform,
    reports: CellReports,
) -> Option<(u16, u16)> {
    let reported = reports.winsize.or(reports.reply);
    match protocol {
        ImageProtocol::HalfBlock => Some((0, 0)),
        ImageProtocol::Sixel if hints.wt_session.is_some() => Some(super::DEFAULT_CELL_PIXELS),
        ImageProtocol::Sixel => reported.or_else(|| {
            (platform == Platform::Windows || hints.override_protocol.is_some())
                .then_some(super::DEFAULT_CELL_PIXELS)
        }),
        // These terminals scale the PNG into the cells themselves: a wrong size only costs
        // sharpness, never layout.
        ImageProtocol::Kitty | ImageProtocol::Iterm2 => {
            Some(reported.unwrap_or(super::DEFAULT_CELL_PIXELS))
        }
    }
}

/// `protocol`, or the half-blocks when it cannot be scaled (see [`cell_size`]).
pub(crate) fn usable(
    protocol: ImageProtocol,
    hints: &TerminalHints<'_>,
    platform: Platform,
    reports: CellReports,
) -> ImageProtocol {
    match cell_size(protocol, hints, platform, reports) {
        Some(_) => protocol,
        None => ImageProtocol::HalfBlock,
    }
}

/// The platform this binary runs on.
pub(crate) const PLATFORM: Platform = if cfg!(windows) {
    Platform::Windows
} else {
    Platform::Unix
};

/// The environment variables [`TerminalHints`] reads, in field order.
const HINT_VARS: [&str; 14] = [
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
];

/// The process's terminal hints: the environment is read once (it never changes within a
/// process).
pub(crate) fn process_hints() -> TerminalHints<'static> {
    static VALUES: OnceLock<[Option<String>; 14]> = OnceLock::new();
    let values = VALUES.get_or_init(|| HINT_VARS.map(|name| std::env::var(name).ok()));
    let at = |index: usize| values[index].as_deref();
    TerminalHints {
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
    }
}

/// The protocol choice without and with a DA1 sixel answer: the hints never change within a
/// process, so both are decided once.
fn hinted() -> [ImageProtocol; 2] {
    static HINTED: OnceLock<[ImageProtocol; 2]> = OnceLock::new();
    *HINTED.get_or_init(|| {
        let hints = process_hints();
        [
            select_protocol(&hints, PLATFORM, None),
            select_protocol(&hints, PLATFORM, Some(true)),
        ]
    })
}

/// Whether any terminal answer could still pick a pixel protocol here (not in a multiplexer
/// pane, not pinned to the half-blocks): the capability query is only worth sending then.
pub(crate) fn pixel_protocol_possible() -> bool {
    hinted() != [ImageProtocol::HalfBlock; 2]
}

/// The process's protocol: the environment-derived choice, upgraded by a DA1 sixel answer and
/// read live (the answers may arrive after the first frame).
pub(crate) fn active_protocol() -> ImageProtocol {
    let [without_da1, with_da1] = hinted();
    let protocol = match super::reports::sixel_attribute() {
        Some(true) => with_da1,
        Some(false) | None => without_da1,
    };
    match protocol {
        // Only sixel can be unscalable: skip the cell-size read for everything else.
        ImageProtocol::Sixel => usable(
            protocol,
            &process_hints(),
            PLATFORM,
            super::live_cell_reports(),
        ),
        ImageProtocol::Kitty | ImageProtocol::Iterm2 | ImageProtocol::HalfBlock => protocol,
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

    /// One terminal's environment and answers, and what it must be drawn with.
    struct Case {
        name: &'static str,
        hints: TerminalHints<'static>,
        platform: Platform,
        da1_sixel: Option<bool>,
        reports: CellReports,
    }

    /// The protocol a terminal gets and the cell pixel size its payload is scaled to: the
    /// composition `active_protocol` and `cell_pixels` perform over the live answers.
    fn decide(case: &Case) -> (&'static str, ImageProtocol, Option<(u16, u16)>) {
        let selected = select_protocol(&case.hints, case.platform, case.da1_sixel);
        let protocol = usable(selected, &case.hints, case.platform, case.reports);
        let cell = cell_size(protocol, &case.hints, case.platform, case.reports);
        (case.name, protocol, cell)
    }

    #[test]
    fn each_terminal_answer_set_picks_its_protocol_and_cell_size() {
        let unix = |name, hints, da1_sixel, winsize, reply| Case {
            name,
            hints,
            platform: Platform::Unix,
            da1_sixel,
            reports: CellReports { winsize, reply },
        };
        let windows = |name, hints, da1_sixel, reply| Case {
            name,
            hints,
            platform: Platform::Windows,
            da1_sixel,
            reports: CellReports {
                winsize: None,
                reply,
            },
        };
        let kitty = TerminalHints {
            kitty_window_id: Some("1"),
            term: Some("xterm-kitty"),
            ..TerminalHints::default()
        };
        let vscode = TerminalHints {
            term_program: Some("vscode"),
            term: Some("xterm-256color"),
            ..TerminalHints::default()
        };
        let windows_terminal = TerminalHints {
            wt_session: Some("5b1e"),
            ..TerminalHints::default()
        };
        let cases = [
            unix("kitty", kitty, None, Some((9, 18)), None),
            // A pty that carries no pixel size (mosh, some ssh hops): the CSI 16t answer.
            unix("kitty, no winsize", kitty, None, None, Some((18, 36))),
            unix(
                "ghostty",
                TerminalHints {
                    ghostty_resources_dir: Some("/usr/share/ghostty"),
                    ..TerminalHints::default()
                },
                None,
                Some((16, 33)),
                Some((16, 33)),
            ),
            unix(
                "wezterm",
                TerminalHints {
                    wezterm_pane: Some("0"),
                    ..TerminalHints::default()
                },
                Some(true),
                Some((8, 16)),
                None,
            ),
            unix(
                "iterm2",
                TerminalHints {
                    lc_terminal: Some("iTerm2"),
                    ..TerminalHints::default()
                },
                None,
                Some((14, 32)),
                None,
            ),
            unix(
                "foot",
                TerminalHints {
                    term: Some("foot"),
                    ..TerminalHints::default()
                },
                Some(true),
                Some((8, 17)),
                Some((8, 17)),
            ),
            // xterm.js with its image addon: DA1 lists sixel, node-pty carries no pixel size.
            unix("vscode, images on", vscode, Some(true), None, Some((8, 17))),
            unix("vscode, no cell answer", vscode, Some(true), None, None),
            unix("vscode, images off", vscode, Some(false), None, None),
            // WSL under Windows Terminal: the WSL pty carries no pixel size, and Windows
            // Terminal draws sixel on its fixed 10x20 virtual cell whatever the font.
            unix(
                "windows terminal hosting WSL",
                windows_terminal,
                None,
                None,
                None,
            ),
            windows("windows terminal", windows_terminal, None, None),
            // Windows Terminal as the default terminal (a console handoff) sets no WT_SESSION:
            // its DA1 answer is the signal.
            windows(
                "windows terminal, default-terminal handoff",
                TerminalHints::default(),
                Some(true),
                Some((10, 20)),
            ),
            // The console host answering for itself (`ESC [ ? 1 ; 0 c`): conhost, or the
            // inbox pseudoconsole under a host it does not relay to.
            windows("conhost", TerminalHints::default(), Some(false), None),
            // WezTerm on Windows relays through its bundled pseudoconsole.
            windows(
                "wezterm on windows",
                TerminalHints {
                    wezterm_pane: Some("0"),
                    ..TerminalHints::default()
                },
                Some(true),
                Some((9, 19)),
            ),
            unix(
                "tmux inside kitty",
                TerminalHints {
                    tmux: Some("/tmp/tmux-0/default,1,0"),
                    ..kitty
                },
                Some(true),
                Some((9, 18)),
                None,
            ),
            // A pinned protocol is the user's call: it draws at the default scale.
            unix(
                "sixel pinned in tmux",
                TerminalHints {
                    override_protocol: Some("sixel"),
                    tmux: Some("/tmp/tmux-0/default,1,0"),
                    ..TerminalHints::default()
                },
                None,
                None,
                None,
            ),
        ];
        let sixel_default = Some((10, 20));
        assert_eq!(
            cases.iter().map(decide).collect::<Vec<_>>(),
            vec![
                ("kitty", ImageProtocol::Kitty, Some((9, 18))),
                ("kitty, no winsize", ImageProtocol::Kitty, Some((18, 36))),
                ("ghostty", ImageProtocol::Kitty, Some((16, 33))),
                ("wezterm", ImageProtocol::Kitty, Some((8, 16))),
                ("iterm2", ImageProtocol::Iterm2, Some((14, 32))),
                ("foot", ImageProtocol::Sixel, Some((8, 17))),
                ("vscode, images on", ImageProtocol::Sixel, Some((8, 17))),
                (
                    "vscode, no cell answer",
                    ImageProtocol::HalfBlock,
                    Some((0, 0))
                ),
                ("vscode, images off", ImageProtocol::HalfBlock, Some((0, 0))),
                (
                    "windows terminal hosting WSL",
                    ImageProtocol::Sixel,
                    sixel_default
                ),
                ("windows terminal", ImageProtocol::Sixel, sixel_default),
                (
                    "windows terminal, default-terminal handoff",
                    ImageProtocol::Sixel,
                    sixel_default
                ),
                ("conhost", ImageProtocol::HalfBlock, Some((0, 0))),
                ("wezterm on windows", ImageProtocol::Sixel, Some((9, 19))),
                ("tmux inside kitty", ImageProtocol::HalfBlock, Some((0, 0))),
                ("sixel pinned in tmux", ImageProtocol::Sixel, sixel_default),
            ]
        );
    }
}
