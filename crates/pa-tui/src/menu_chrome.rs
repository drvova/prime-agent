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

/// The title row: the surface's name in the accent colour at the chrome indent (`Model`,
/// `Settings › Theme`, `Session tree`), truncated to the frame width.
pub(crate) fn title_row(theme: &Theme, width: usize, title: &str) -> Line {
    let line = vec![
        Span::raw(CHROME_INDENT),
        theme.fg_span(ThemeColor::Accent, title.to_string()),
    ];
    truncate_line(&line, width, "")
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

    /// The Esc hint over `tui.select.cancel`: `close` on a top-level surface, `back` in a
    /// submenu.
    #[must_use]
    pub(crate) fn esc(self, kb: &KeybindingsManager, verb: EscVerb) -> Self {
        self.bound(kb, &["tui.select.cancel"], verb.label(), HintRank::Core)
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
            .bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "navigate",
                HintRank::Useful,
            )
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
