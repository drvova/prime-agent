//! The shared menu chrome: the title row every picker and panel opens with, and the ONE footer
//! grammar every picker, panel, and pane closes with — `key verb` hints joined by ` · `, keys read
//! from the effective keybindings, lowest-priority hints dropped whole when the row is too narrow.

use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line};
use crate::{Line, Span};

/// The column every chrome row starts at: the menu rows' text column after the `› ` marker, so
/// the title, the rows' labels, and the footer share one left edge.
pub(crate) const CHROME_INDENT: &str = "  ";

const SEPARATOR: &str = " \u{b7} ";

/// The mark a row that was cut to fit ends with: chrome text never stops mid-word unmarked.
pub(crate) const ELLIPSIS: &str = "\u{2026}";

/// The title row: the surface's name in the accent colour at the chrome indent (`Model`,
/// `Settings › Theme`, `Session tree`), truncated to the frame width.
pub(crate) fn title_row(theme: &Theme, width: usize, title: &str) -> Line {
    let line = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(ThemeColor::Accent, title.to_string()),
    ];
    truncate_line(&line, width, ELLIPSIS)
}

/// Hold a searchable list's height while a query is active: the rows pushed since `list_start`
/// (list rows, scroll row, empty state) are padded with blank rows up to `held`, the height the
/// unfiltered list renders — which is the height it had when the query started, since a query
/// only ever starts from the full list. Filtering then never moves the rows around the list.
pub(crate) fn hold_list_height(lines: &mut Vec<Line>, list_start: usize, held: usize) {
    let used = lines.len().saturating_sub(list_start);
    lines.extend(std::iter::repeat_with(Vec::new).take(held.saturating_sub(used)));
}

/// The list's empty state: with no query the surface's own text says what is empty; a query that
/// hides every row says so muted, with the key that clears it (`no match for “x” · esc clears`).
pub(crate) fn empty_state_row(
    theme: &Theme,
    width: usize,
    kb: &KeybindingsManager,
    query: &str,
    empty: &str,
) -> Line {
    let text = if query.is_empty() {
        empty.to_string()
    } else {
        let no_match = format!("no match for \u{201c}{query}\u{201d}");
        match kb.first_key("tui.select.cancel") {
            Some(key) => {
                let key = format_key_text(&key).to_lowercase();
                format!("{no_match}{SEPARATOR}{key} clears")
            }
            None => no_match,
        }
    };
    let line = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(ThemeColor::Muted, text),
    ];
    truncate_line(&line, width, ELLIPSIS)
}

/// A surface's failure row: `Error: <message>` in the error tone at the chrome indent, ending in
/// `…` when the frame is too narrow for it, so a cut message never reads as the whole one.
pub(crate) fn error_row(theme: &Theme, width: usize, message: &str) -> Line {
    let line = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(ThemeColor::Error, format!("Error: {message}")),
    ];
    truncate_line(&line, width, ELLIPSIS)
}

/// How much a footer hint matters when the row is too narrow: the lowest rank drops first, and
/// within a rank the rightmost hint drops first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HintRank {
    /// Discoverability extras (filters, jumps, toggles) that the surface works without.
    Extra,
    /// Navigation and the surface's secondary actions.
    Useful,
    /// The confirm and close keys: the last hints standing.
    Core,
}

/// What Esc does on the surface: it closes a top-level surface and goes back out of a submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscVerb {
    Close,
    Back,
}

impl EscVerb {
    fn label(self) -> &'static str {
        match self {
            EscVerb::Close => "close",
            EscVerb::Back => "back",
        }
    }
}

/// One footer hint: the formatted key label and the verb it performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hint {
    pub key: String,
    pub verb: String,
    pub rank: HintRank,
}

impl Hint {
    fn width(&self) -> usize {
        str_width(&self.key) + 1 + str_width(&self.verb)
    }
}

/// A surface's footer: an optional leading notice (a status the surface reports in its own tone)
/// followed by the hints in reading order. Built per frame from the effective keybindings, so a
/// user override moves the hint with the handler and an unbound action never advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Footer {
    enter: Option<String>,
    notice: Option<(ThemeColor, String)>,
    hints: Vec<Hint>,
}

impl Footer {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A hint over the first effective key of each binding, joined with `/` (`↑/↓ navigate`); a
    /// binding with no key drops out, and the hint drops when none is bound.
    #[must_use]
    pub(crate) fn bound(
        self,
        kb: &KeybindingsManager,
        bindings: &[&str],
        verb: &str,
        rank: HintRank,
    ) -> Self {
        let keys: Vec<String> = bindings
            .iter()
            .filter_map(|binding| kb.first_key(binding))
            .collect();
        self.keys(keys, verb, rank)
    }

    /// A hint over raw key ids: a binding's whole key set, or a key the surface matches literally
    /// (Tab, Space, the digit row). The ids are formatted for display and joined with `/`; an
    /// empty set drops the hint.
    #[must_use]
    pub(crate) fn keys<I>(mut self, keys: I, verb: &str, rank: HintRank) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let labels: Vec<String> = keys
            .into_iter()
            .map(|key| format_key_text(key.as_ref()))
            .filter(|label| !label.is_empty())
            .collect();
        if !labels.is_empty() {
            self.hints.push(Hint {
                key: labels.join("/"),
                verb: verb.to_string(),
                rank,
            });
        }
        self
    }

    /// The list-navigation hint every picker shares: `↑/↓ navigate` over the effective
    /// `tui.select.up`/`tui.select.down` keys, at the rank navigation drops at.
    #[must_use]
    pub(crate) fn navigate(self, kb: &KeybindingsManager) -> Self {
        self.bound(
            kb,
            &["tui.select.up", "tui.select.down"],
            "navigate",
            HintRank::Useful,
        )
    }

    /// A dock pane's list footer (the bash and heartbeat panes): navigate and open while the
    /// list has rows, the close keys always. An empty list advertises no key that does nothing,
    /// and an override that empties a binding drops its key.
    #[must_use]
    pub(crate) fn pane_list(kb: &KeybindingsManager, rows: usize) -> Self {
        let footer = if rows == 0 {
            Self::new()
        } else {
            Self::new().navigate(kb).bound(
                kb,
                &["tui.select.confirm", "app.modal.open"],
                "open",
                HintRank::Core,
            )
        };
        footer.bound(
            kb,
            &["app.modal.back", "tui.select.cancel"],
            "close",
            HintRank::Core,
        )
    }

    /// The Esc hint over `tui.select.cancel`: `close` on a top-level surface, `back` in a
    /// submenu.
    #[must_use]
    pub(crate) fn esc(self, kb: &KeybindingsManager, verb: EscVerb) -> Self {
        self.bound(kb, &["tui.select.cancel"], verb.label(), HintRank::Core)
    }

    /// The Enter line a surface opts into: what the confirm key does for the selected row
    /// (`enter switches to Claude Opus 4.8`), rendered muted directly above the footer by
    /// [`Footer::rows`]. No action (an empty list) or an unbound confirm key renders no line.
    #[must_use]
    pub(crate) fn enter(mut self, kb: &KeybindingsManager, action: Option<&str>) -> Self {
        self.enter = action
            .zip(kb.first_key("tui.select.confirm"))
            .map(|(action, key)| format!("{} {action}", format_key_text(&key).to_lowercase()));
        self
    }

    /// The Enter line (when the surface set one) over the footer row.
    #[must_use]
    pub(crate) fn rows(&self, theme: &Theme, width: usize) -> Vec<Line> {
        let mut rows = Vec::with_capacity(2);
        if let Some(enter) = &self.enter {
            rows.push(truncate_line(
                &vec![
                    Span::raw(CHROME_INDENT),
                    theme.fg_span(ThemeColor::Muted, enter.clone()),
                ],
                width,
                "\u{2026}",
            ));
        }
        rows.push(self.line(theme, width));
        rows
    }

    /// A leading status in its own tone (a copy result, an armed confirm): it never drops, and
    /// the hints that no longer fit beside it drop first.
    #[must_use]
    pub(crate) fn notice(mut self, color: ThemeColor, text: &str) -> Self {
        self.notice = Some((color, text.to_string()));
        self
    }

    /// The hints that fit in `budget` columns, in reading order: while the joined hints overflow,
    /// the lowest-ranked hint (the rightmost among equals) drops whole — never cut mid-word.
    #[must_use]
    pub(crate) fn fitted(&self, budget: usize) -> Vec<Hint> {
        let mut kept: Vec<&Hint> = self.hints.iter().collect();
        let joined = |hints: &[&Hint]| {
            hints.iter().map(|hint| hint.width()).sum::<usize>()
                + str_width(SEPARATOR) * hints.len().saturating_sub(1)
        };
        while !kept.is_empty() && joined(&kept) > budget {
            let lowest = kept
                .iter()
                .map(|hint| hint.rank)
                .min()
                .unwrap_or(HintRank::Core);
            if let Some(position) = kept.iter().rposition(|hint| hint.rank == lowest) {
                kept.remove(position);
            }
        }
        kept.into_iter().cloned().collect()
    }

    /// The footer row at the chrome indent: the notice in its tone, then each hint's key in the
    /// muted tone and its verb and separators dim.
    #[must_use]
    pub(crate) fn line(&self, theme: &Theme, width: usize) -> Line {
        let mut line: Line = vec![Span::raw(CHROME_INDENT)];
        let mut budget = width.saturating_sub(str_width(CHROME_INDENT));
        if let Some((color, text)) = &self.notice {
            line.push(theme.fg_span(*color, text.clone()));
            budget = budget.saturating_sub(str_width(text) + str_width(SEPARATOR));
        }
        let hints = self.fitted(budget);
        for (index, hint) in hints.iter().enumerate() {
            if index > 0 || self.notice.is_some() {
                line.push(theme.fg_span(ThemeColor::Dim, SEPARATOR));
            }
            line.push(theme.fg_span(ThemeColor::Muted, hint.key.clone()));
            line.push(theme.fg_span(ThemeColor::Dim, format!(" {}", hint.verb)));
        }
        truncate_line(&line, width, "")
    }
}

#[cfg(test)]
mod panel_footers_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{KeybindingsConfig, KeybindingsManager};
    use crate::theme::ColorMode;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn text(line: &Line) -> String {
        line.iter().map(|span| span.content.as_str()).collect()
    }

    fn hint(key: &str, verb: &str, rank: HintRank) -> Hint {
        Hint {
            key: key.to_string(),
            verb: verb.to_string(),
            rank,
        }
    }

    fn picker_footer(kb: &KeybindingsManager) -> Footer {
        Footer::new()
            .navigate(kb)
            .bound(
                kb,
                &["tui.select.top", "tui.select.bottom"],
                "jump",
                HintRank::Extra,
            )
            .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
            .esc(kb, EscVerb::Close)
    }

    /// A narrow row drops the lowest-ranked hints whole, the extras first, then navigation; the
    /// confirm and close keys stand last.
    #[test]
    fn narrow_rows_drop_the_lowest_ranked_hints_whole() {
        let kb = KeybindingsManager::new();
        let footer = picker_footer(&kb);
        assert_eq!(
            footer.fitted(80),
            vec![
                hint("\u{2191}/\u{2193}", "navigate", HintRank::Useful),
                hint("Home/End", "jump", HintRank::Extra),
                hint("Enter", "select", HintRank::Core),
                hint("Esc", "close", HintRank::Core),
            ]
        );
        assert_eq!(
            footer.fitted(40),
            vec![
                hint("\u{2191}/\u{2193}", "navigate", HintRank::Useful),
                hint("Enter", "select", HintRank::Core),
                hint("Esc", "close", HintRank::Core),
            ]
        );
        assert_eq!(
            footer.fitted(24),
            vec![
                hint("Enter", "select", HintRank::Core),
                hint("Esc", "close", HintRank::Core),
            ]
        );
        assert_eq!(
            footer.fitted(12),
            vec![hint("Enter", "select", HintRank::Core)]
        );
        assert_eq!(
            text(&footer.line(&theme(), 32)),
            "  Enter select \u{b7} Esc close"
        );
    }

    /// The keys come from the effective bindings: a rebound key shows the rebound key, and an
    /// unbound action drops its hint.
    #[test]
    fn hints_render_the_rebound_key_and_drop_unbound_actions() {
        let mut bindings = KeybindingsConfig::new();
        bindings.insert("tui.select.confirm".to_string(), vec!["ctrl+j".to_string()]);
        bindings.insert("tui.select.top".to_string(), Vec::new());
        bindings.insert("tui.select.bottom".to_string(), Vec::new());
        let kb = KeybindingsManager::with_user_bindings(bindings);
        assert_eq!(
            text(&picker_footer(&kb).line(&theme(), 80)),
            "  \u{2191}/\u{2193} navigate \u{b7} Ctrl+J select \u{b7} Esc close"
        );
    }

    /// The two empty states: the surface's own text with no query; the muted no-match line
    /// naming the clear key once a query hides every row.
    #[test]
    fn empty_states_tell_empty_from_no_match() {
        let kb = KeybindingsManager::new();
        assert_eq!(
            text(&empty_state_row(
                &theme(),
                80,
                &kb,
                "",
                "No user messages found"
            )),
            "  No user messages found"
        );
        assert_eq!(
            text(&empty_state_row(
                &theme(),
                80,
                &kb,
                "zq",
                "No user messages found"
            )),
            "  no match for \u{201c}zq\u{201d} \u{b7} esc clears"
        );
    }

    /// The Enter line rides above the footer only when the surface names an action.
    #[test]
    fn the_enter_line_rides_above_the_footer() {
        let kb = KeybindingsManager::new();
        let footer = Footer::new().esc(&kb, EscVerb::Close);
        let rows = footer
            .clone()
            .enter(&kb, Some("forks from this message"))
            .rows(&theme(), 80);
        assert_eq!(
            rows.iter().map(text).collect::<Vec<_>>(),
            vec!["  enter forks from this message", "  Esc close"]
        );
        assert_eq!(footer.enter(&kb, None).rows(&theme(), 80).len(), 1);
    }

    /// Chrome text cut to the frame ends in the shared mark: a title, an empty state and an
    /// error never stop mid-word unmarked, and none is wider than the frame.
    #[test]
    fn cut_chrome_rows_end_in_the_shared_mark() {
        let kb = KeybindingsManager::new();
        assert_eq!(
            [
                text(&title_row(&theme(), 14, "Settings \u{203a} Theme")),
                text(&empty_state_row(
                    &theme(),
                    14,
                    &kb,
                    "",
                    "No user messages found"
                )),
                text(&error_row(&theme(), 14, "kill failed: no such process")),
                text(&error_row(&theme(), 80, "kill failed")),
            ],
            [
                "  Settings \u{203a} \u{2026}",
                "  No user mes\u{2026}",
                "  Error: kill\u{2026}",
                "  Error: kill failed",
            ]
            .map(str::to_string)
        );
    }

    /// A dock pane's list offers its arrows and open key only over rows; an empty list keeps
    /// the close keys alone.
    #[test]
    fn an_empty_pane_list_offers_only_its_way_out() {
        let kb = KeybindingsManager::new();
        assert_eq!(
            [
                text(&Footer::pane_list(&kb, 3).line(&theme(), 80)),
                text(&Footer::pane_list(&kb, 0).line(&theme(), 80)),
            ],
            [
                "  \u{2191}/\u{2193} navigate \u{b7} Enter/\u{2192} open \u{b7} \u{2190}/Esc close",
                "  \u{2190}/Esc close",
            ]
            .map(str::to_string)
        );
    }

    /// Esc reads `close` on a top-level surface and `back` in a submenu.
    #[test]
    fn esc_reads_close_or_back() {
        let kb = KeybindingsManager::new();
        assert_eq!(
            Footer::new().esc(&kb, EscVerb::Close).fitted(80),
            vec![hint("Esc", "close", HintRank::Core)]
        );
        assert_eq!(
            Footer::new().esc(&kb, EscVerb::Back).fitted(80),
            vec![hint("Esc", "back", HintRank::Core)]
        );
    }
}
