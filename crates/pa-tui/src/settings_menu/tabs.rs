//! The `/settings` menu's tabs: the grouping of the settings rows and the tab strip that navigates
//! them — one `N Name` strip row under the search field, the active tab white bold (the operator's
//! 2026-09-28 selection ruling).

use super::SettingsMenuRow;
use crate::menu_chrome::ELLIPSIS;
use crate::theme::{Theme, ThemeColor};
use crate::width::{line_width, truncate_line};
use crate::{Line, Span};

struct TabLayout {
    name: &'static str,
    ids: &'static [&'static str],
}

/// The settings tabs: General (session behavior), Models (model-side choices), Display (what the UI
/// renders), Editor (the input editor), Agents (the agent tree and its services).
const TAB_LAYOUT: &[TabLayout] = &[
    TabLayout {
        name: "General",
        ids: &[
            "autocompact",
            "steering-mode",
            "follow-up-mode",
            "quiet-startup",
            "warnings",
            "autocompact-threshold",
        ],
    },
    TabLayout {
        name: "Models",
        ids: &["thinking", "transport", "default-service-tier"],
    },
    TabLayout {
        name: "Display",
        ids: &[
            "theme",
            "notify-on-turn-end",
            "terminal-progress",
            "clear-on-shrink",
            "show-images",
            "auto-resize-images",
            "block-images",
            "mermaid-rendering",
        ],
    },
    TabLayout {
        name: "Editor",
        ids: &[
            "editor-padding",
            "autocomplete-max-visible",
            "show-hardware-cursor",
            "emoji-shortcodes",
        ],
    },
    TabLayout {
        name: "Agents",
        ids: &[
            "skill-commands",
            "builtin-skills",
            "idle-eviction-minutes",
            "tree-filter-mode",
            "harness-refinements",
            "web-search-provider",
        ],
    },
];

/// Resolve the layout against the menu's rows: each tab keeps the indices of its rows. The layout
/// is the single source of the grouping, so a non-empty row set must be covered exactly once both
/// ways — a mismatch is a programming error that panics here; an empty row set carries no tabs.
pub(crate) fn row_indices(rows: &[SettingsMenuRow]) -> Vec<(&'static str, Vec<usize>)> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut covered = vec![false; rows.len()];
    let tabs = TAB_LAYOUT
        .iter()
        .map(|tab| {
            (
                tab.name,
                tab.ids
                    .iter()
                    .map(|id| {
                        let index = rows
                            .iter()
                            .position(|row| row.id == *id)
                            .unwrap_or_else(|| {
                                panic!("the settings tab layout names {id}, which no settings row provides")
                            });
                        covered[index] = true;
                        index
                    })
                    .collect(),
            )
        })
        .collect();
    if let Some((_, row)) = rows.iter().enumerate().find(|(index, _)| !covered[*index]) {
        panic!("the settings row {} rides no tab", row.id);
    }
    tabs
}

/// Whether the strip shows each tab's match count (while a query is active).
#[derive(Debug, Clone, Copy)]
pub(crate) enum StripCounts<'a> {
    Hidden,
    /// One match count per tab, in tab order.
    Matches(&'a [usize]),
}

/// The tab strip: one `N Name` per tab — the number dim, the name muted, the active tab white bold
/// (the theme's text color, not the dock's background band — the operator's 2026-09-28 selection
/// ruling). The numbers are the digit keys that jump straight to the tab.
pub(crate) fn strip_row(
    theme: &Theme,
    width: usize,
    names: &[&'static str],
    active: usize,
    counts: StripCounts<'_>,
) -> Line {
    let tabs: Vec<Line> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut tab = vec![
                theme.fg_span(ThemeColor::Dim, (index + 1).to_string()),
                Span::raw(" "),
                if index == active {
                    theme.bold(theme.fg_span(ThemeColor::Text, name.to_string()))
                } else {
                    theme.fg_span(ThemeColor::Muted, name.to_string())
                },
            ];
            match counts {
                StripCounts::Hidden => {}
                StripCounts::Matches(counts) => {
                    let count = counts.get(index).copied().unwrap_or(0);
                    let color = if count > 0 {
                        ThemeColor::Accent
                    } else {
                        ThemeColor::Dim
                    };
                    tab.push(Span::raw(" "));
                    tab.push(theme.fg_span(color, count.to_string()));
                }
            }
            tab
        })
        .collect();
    // A strip wider than the pane windows over whole tabs around the active one, and a dim `…`
    // marks each side that hides tabs, so a narrow pane never reads as if the tabs it shows were
    // all there are.
    let Some(newest) = tabs.len().checked_sub(1) else {
        return vec![Span::raw("  ")];
    };
    let active = active.min(newest);
    let window_width = |first: usize, last: usize| -> usize {
        2 + if first > 0 { 2 } else { 0 }
            + tabs[first..=last]
                .iter()
                .map(|tab| line_width(tab) + 4)
                .sum::<usize>()
            - 4
            + if last < newest { 2 } else { 0 }
    };
    let first = (0..=active)
        .find(|first| window_width(*first, active) <= width)
        .unwrap_or(active);
    let last = (active..=newest)
        .rev()
        .find(|last| window_width(first, *last) <= width)
        .unwrap_or(active);
    let mut line: Line = vec![Span::raw("  ")];
    if first > 0 {
        line.push(theme.fg_span(ThemeColor::Dim, format!("{ELLIPSIS} ")));
    }
    for (index, tab) in tabs.iter().enumerate().take(last + 1).skip(first) {
        if index > first {
            line.push(Span::raw("    "));
        }
        line.extend(tab.iter().cloned());
    }
    if last < newest {
        line.push(theme.fg_span(ThemeColor::Dim, format!(" {ELLIPSIS}")));
    }
    truncate_line(&line, width, ELLIPSIS)
}

#[cfg(test)]
mod tabs_tests {
    use super::*;

    fn theme() -> crate::theme::Theme {
        crate::theme::Theme::builtin("prime", crate::theme::ColorMode::Color256)
    }

    #[test]
    fn the_layout_names_each_settings_id_once() {
        let names: Vec<&str> = TAB_LAYOUT
            .iter()
            .flat_map(|tab| tab.ids.iter().copied())
            .collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn the_strip_lists_the_tabs_and_marks_the_active_one() {
        let line = strip_row(&theme(), 80, &["General", "Models"], 1, StripCounts::Hidden);
        let text: String = line.iter().map(|span| span.content.as_str()).collect();
        assert_eq!(text, "  1 General    2 Models");
        let active = line
            .iter()
            .find(|span| span.content == "Models")
            .expect("the active tab renders");
        assert_eq!(
            active.style,
            theme()
                .fg_style(ThemeColor::Text)
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
        let inactive = line
            .iter()
            .find(|span| span.content == "General")
            .expect("an inactive tab renders");
        assert_eq!(inactive.style, theme().fg_style(ThemeColor::Muted));
    }

    #[test]
    fn the_strip_truncates_to_the_frame_width() {
        let line = strip_row(
            &theme(),
            12,
            &["General", "Models", "Display"],
            0,
            StripCounts::Hidden,
        );
        let text: String = line.iter().map(|span| span.content.as_str()).collect();
        assert_eq!(text, "  1 General\u{2026}");
    }

    /// A pane too narrow for every tab keeps the active tab on the strip and marks each side
    /// that hides tabs.
    #[test]
    fn a_narrow_strip_keeps_the_active_tab_and_marks_the_hidden_ones() {
        let names = ["General", "Models", "Display", "Editor", "Agents"];
        let strip = |active: usize| -> String {
            strip_row(&theme(), 40, &names, active, StripCounts::Hidden)
                .iter()
                .map(|span| span.content.as_str())
                .collect()
        };
        assert_eq!(
            [strip(0), strip(3), strip(4)],
            [
                "  1 General    2 Models    3 Display \u{2026}",
                "  \u{2026} 2 Models    3 Display    4 Editor \u{2026}",
                "  \u{2026} 3 Display    4 Editor    5 Agents",
            ]
            .map(str::to_string)
        );
    }

    proptest::proptest! {
        // The strip fits every pane width and, given room for one tab between two marks, always
        // shows the active tab.
        #[test]
        fn the_strip_fits_the_pane_and_keeps_the_active_tab(width in 0usize..120, active in 0usize..5) {
            let names = ["General", "Models", "Display", "Editor", "Agents"];
            let line = strip_row(&theme(), width, &names, active, StripCounts::Hidden);
            proptest::prop_assert!(line_width(&line) <= width, "{line:?}");
            if width >= 15 {
                proptest::prop_assert!(
                    line.iter().any(|span| span.content == names[active]),
                    "{line:?}"
                );
            }
        }
    }
}
