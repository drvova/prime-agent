//! The `/settings` inline menu: a tab strip under the bordered search field, each tab a list of
//! label/value rows the arrows and Enter/Space cycle (the operator's 2026-09-28 rebind: tabs move
//! on Tab/number keys only), Enter opens submenus, type-to-search per tab. The caller owns the row
//! data and change actions; this module navigation and rendering.

mod tabs;

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{empty_state_row, title_row, EscVerb, Footer, HintRank};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::wrap_text;

/// The reasoning-level descriptions the TS thinking submenu lists.
fn thinking_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning",
        "low" => "Light reasoning",
        "medium" => "Moderate reasoning",
        "high" => "Deep reasoning",
        "xhigh" => "Very deep reasoning",
        "max" => "Maximum reasoning",
        _ => "",
    }
}

/// A submenu a row opens with Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSubmenu {
    /// "Thinking Level" (the session's available levels).
    Thinking { levels: Vec<String> },
    /// "Theme" (the registered themes; selection previews).
    Theme { themes: Vec<String> },
    /// "Warnings" (the single warning toggle as its own list).
    Warnings,
    /// "Default Service Tier" (TS `SERVICE_TIER_OPTIONS`).
    ServiceTier,
}

/// The service-tier descriptions the TS settings submenu lists.
#[must_use]
pub fn service_tier_description(tier: &str) -> &'static str {
    match tier {
        "default" => "Standard processing",
        "flex" => "Cheaper, slower, may hit capacity limits",
        "priority" => "Faster, more expensive (fast mode)",
        "auto" => "Provider picks the tier",
        _ => "",
    }
}

/// The settings-row and autocomplete choice order.
pub const SERVICE_TIER_CHOICES: [&str; 4] = ["default", "flex", "priority", "auto"];

/// When a saved change takes effect: the hint row says so after each change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    Now,
    NextTurn,
    NewSessions,
    AfterRestart,
}

impl Applies {
    fn hint(self) -> &'static str {
        match self {
            Applies::Now => "Saved \u{b7} applies now",
            Applies::NextTurn => "Saved \u{b7} from the next turn",
            Applies::NewSessions => "Saved \u{b7} in new sessions",
            Applies::AfterRestart => "Saved \u{b7} after restart",
        }
    }
}

/// How long the hint row keeps the `Saved` notice after a change. The run loop arms a wake at
/// its expiry (see [`SettingsMenu::saved_notice`]), so an idle screen repaints it away.
const SAVED_NOTICE: std::time::Duration = std::time::Duration::from_secs(3);

/// One settings row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsMenuRow {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// The displayed current value (right side).
    pub current: String,
    /// The values Enter/Space cycles through (`None` = submenu row).
    pub values: Option<Vec<String>>,
    pub submenu: Option<SettingsSubmenu>,
    /// When a change to this row takes effect.
    pub applies: Applies,
}

/// One key press while the menu is open. The theme submenu's live preview
/// and its Esc restore come through as their own actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsMenuAction {
    None,
    /// Esc from the top level.
    Cancel,
    /// Enter/Space changed a row (or a submenu selected a value).
    Change {
        id: &'static str,
        value: String,
    },
    /// The theme submenu's selection moved: preview the theme live.
    PreviewTheme {
        name: String,
    },
    /// Delete put a changed row back to its default value.
    Reset {
        id: &'static str,
        value: String,
    },
    /// Esc inside a submenu closed it; the menu itself stays open.
    SubmenuClosed,
    /// The theme submenu closed with Esc: restore the row's theme.
    RestoreTheme {
        name: String,
    },
}

/// The open submenu's own selection state: `selected` is an option index, `filtered` the option
/// indices the search keeps.
#[derive(Debug)]
struct SubmenuState {
    row: usize,
    kind: SettingsSubmenu,
    selected: usize,
    search: SearchInput,
    filtered: Vec<usize>,
}

impl SubmenuState {
    fn new(row: usize, kind: SettingsSubmenu, selected: usize) -> Self {
        let filtered = (0..submenu_labels(&kind).len()).collect();
        SubmenuState {
            row,
            kind,
            selected,
            search: SearchInput::new(),
            filtered,
        }
    }

    /// Re-filter the options (case-insensitive substring over the label); the selection stays
    /// when it survives, else lands on the first match.
    fn refilter(&mut self) {
        let query = self.search.value().to_lowercase();
        self.filtered = submenu_labels(&self.kind)
            .iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        if !self.filtered.contains(&self.selected) {
            if let Some(&first) = self.filtered.first() {
                self.selected = first;
            }
        }
    }

    /// Step the selection through the filtered options, wrapping at the ends.
    fn step(&mut self, delta: isize) {
        let count = self.filtered.len();
        if count == 0 {
            return;
        }
        let position = self
            .filtered
            .iter()
            .position(|&index| index == self.selected)
            .unwrap_or(0);
        let next = (position as isize + delta).rem_euclid(count as isize) as usize;
        self.selected = self.filtered[next];
    }
}

/// One submenu's option labels, in display order.
fn submenu_labels(kind: &SettingsSubmenu) -> Vec<String> {
    match kind {
        SettingsSubmenu::Thinking { levels } => levels.clone(),
        SettingsSubmenu::Theme { themes } => themes.clone(),
        SettingsSubmenu::Warnings => vec![String::from("Anthropic extra usage")],
        SettingsSubmenu::ServiceTier => SERVICE_TIER_CHOICES
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

/// The settings menu: the strip rides under the search field, each tab
/// owns its own search input, filtered window, and selection.
#[derive(Debug)]
pub struct SettingsMenu {
    rows: Vec<SettingsMenuRow>,
    tabs: Vec<SettingsTab>,
    tab: usize,
    sub: Option<SubmenuState>,
    max_visible: usize,
    /// Each row's default value by id: a row whose value differs is tinted, and Delete resets it.
    defaults: std::collections::HashMap<&'static str, String>,
    /// The last change: when it was saved and when it takes effect.
    saved: Option<(std::time::Instant, Applies)>,
    /// The last changed row and the value it had before, so a refused save can put it back.
    last_change: Option<(usize, String)>,
    /// The refused save the detail area reports under its row.
    refusal: Option<Refusal>,
}

/// One refused save: the row it belongs to and the reason the save gave.
#[derive(Debug)]
struct Refusal {
    row: usize,
    reason: String,
}

/// One tab: the row indices it groups with its search input, filtered window, and selection. A
/// switch carries the active query over (the search is shared across tabs), and coming back to a
/// tab under the same query restores where the user was.
#[derive(Debug)]
struct SettingsTab {
    name: &'static str,
    rows: Vec<usize>,
    filtered: Vec<usize>,
    search: SearchInput,
    selected: usize,
}

/// The TS settings-menu rows in the TS order, with the caller-assembled
/// current values.
pub fn settings_menu_rows(current: &SettingsCurrentValues) -> Vec<SettingsMenuRow> {
    let bool_value = || vec!["true".to_string(), "false".to_string()];
    let mut idle_values: Vec<String> = ["off"]
        .iter()
        .map(ToString::to_string)
        .chain([30, 60, 90, 180, 360].iter().map(ToString::to_string))
        .collect();
    if let Ok(minutes) = current.idle_eviction_minutes.parse::<u32>() {
        if minutes > 0 && !idle_values.contains(&minutes.to_string()) {
            idle_values.push(minutes.to_string());
            idle_values[1..].sort_by_key(|value| value.parse::<u32>().unwrap_or(0));
        }
    }
    let percent =
        if pa_types::compaction::is_valid_threshold_percent(current.compaction_threshold_percent) {
            current.compaction_threshold_percent
        } else {
            95.0
        };
    let mut thresholds: Vec<f64> = (5..=100).step_by(5).map(f64::from).collect();
    if !thresholds.contains(&percent) {
        thresholds.push(percent);
        thresholds.sort_by(f64::total_cmp);
    }
    vec![
        SettingsMenuRow {
            id: "autocompact-threshold",
            label: "Auto-compact threshold (%)",
            description: "Context percentage (1–100); retained history and model output budgets bound the trigger",
            current: percent.to_string(),
            values: Some(thresholds.into_iter().map(|value| value.to_string()).collect()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "autocompact",
            label: "Auto-compact",
            description: "Automatically compact context when it gets too large",
            current: current.autocompact.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "show-images",
            label: "Show image metadata",
            description: "Show image type and dimensions in terminal",
            current: current.show_images.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "auto-resize-images",
            label: "Auto-resize images",
            description: "Resize large images to 2000x2000 max for better model compatibility",
            current: current.auto_resize_images.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "block-images",
            label: "Block images",
            description: "Prevent images from being sent to LLM providers",
            current: current.block_images.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "skill-commands",
            label: "Skill commands",
            description: "Register skills as /skill:name commands",
            current: current.skill_commands.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "harness-refinements",
            label: "Harness refinements",
            description: "Create and load harness refinements; prior replies and summaries retain their influence",
            current: current.harness_refinements.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "web-search-provider",
            label: "Web search provider",
            description: "Service the websearch skill queries in new sessions; add its API key via /mcp",
            current: current.web_search_provider.clone(),
            values: Some(vec!["serper".into(), "eco".into()]),
            submenu: None,
            applies: Applies::NewSessions,
        },
        SettingsMenuRow {
            id: "builtin-skills",
            label: "Built-in skills",
            description: "Load built-in skills shipped with prime-agent (takes effect after reload)",
            current: current.builtin_skills.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::AfterRestart,
        },
        SettingsMenuRow {
            id: "show-hardware-cursor",
            label: "Show hardware cursor",
            description: "Show the terminal cursor while still positioning it for IME support",
            current: current.hardware_cursor.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "editor-padding",
            label: "Editor padding",
            description: "Horizontal padding for input editor (0-3)",
            current: current.editor_padding.to_string(),
            values: Some(vec!["0".into(), "1".into(), "2".into(), "3".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "autocomplete-max-visible",
            label: "Autocomplete max items",
            description: "Max visible items in autocomplete dropdown (3-20)",
            current: current.autocomplete_max_visible.to_string(),
            values: Some(vec!["3".into(), "5".into(), "8".into(), "10".into(), "15".into(), "20".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "emoji-shortcodes",
            label: "Emoji shortcodes",
            description: "Suggest emoji when typing :name in the editor",
            current: current.emoji_shortcodes.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "clear-on-shrink",
            label: "Clear on shrink",
            description: "Clear empty rows when content shrinks (may cause flicker)",
            current: current.clear_on_shrink.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "terminal-progress",
            label: "Terminal progress",
            description: "Show OSC 9;4 progress indicators in the terminal tab bar",
            current: current.terminal_progress.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "idle-eviction-minutes",
            label: "Idle worker eviction",
            description: "Stop fully idle agent trees after this many minutes (global daemon policy)",
            current: current.idle_eviction_minutes.clone(),
            values: Some(idle_values),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "steering-mode",
            label: "Steering mode",
            description: "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            current: current.steering_mode.clone(),
            values: Some(vec!["one-at-a-time".into(), "all".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "follow-up-mode",
            label: "Follow-up mode",
            description: "Alt+Enter queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            current: current.follow_up_mode.clone(),
            values: Some(vec!["one-at-a-time".into(), "all".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "transport",
            label: "Transport",
            description: "Preferred transport for providers that support multiple transports",
            current: current.transport.clone(),
            values: Some(vec!["sse".into(), "websocket".into(), "websocket-cached".into(), "auto".into()]),
            submenu: None,
            applies: Applies::NextTurn,
        },
        SettingsMenuRow {
            id: "default-service-tier",
            label: "Default service tier",
            description: "Service tier for new sessions; applies to the current session when the model supports it",
            current: current.default_service_tier.clone(),
            values: None,
            submenu: Some(SettingsSubmenu::ServiceTier),
            applies: Applies::NewSessions,
        },
        SettingsMenuRow {
            id: "mermaid-rendering",
            label: "Mermaid diagrams",
            description: "Render Mermaid code blocks as Unicode diagrams",
            current: current.mermaid.clone(),
            values: Some(vec!["off".into(), "final".into(), "streaming".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "quiet-startup",
            label: "Quiet startup",
            description: "Disable verbose printing at startup",
            current: current.quiet_startup.to_string(),
            values: Some(bool_value()),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "tree-filter-mode",
            label: "Tree filter mode",
            description: "Default filter when opening /tree",
            current: current.tree_filter_mode.clone(),
            values: Some(vec!["default".into(), "no-tools".into(), "user-only".into(), "labeled-only".into(), "all".into()]),
            submenu: None,
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "warnings",
            label: "Warnings",
            description: "Enable or disable individual warnings",
            current: "configure".to_string(),
            values: None,
            submenu: Some(SettingsSubmenu::Warnings),
            applies: Applies::Now,
        },
        SettingsMenuRow {
            id: "thinking",
            label: "Thinking level",
            description: "Reasoning depth for thinking-capable models",
            current: current.thinking_level.clone().unwrap_or_default(),
            values: None,
            submenu: Some(SettingsSubmenu::Thinking {
                levels: current.available_thinking_levels.clone(),
            }),
            applies: Applies::NextTurn,
        },
        SettingsMenuRow {
            id: "theme",
            label: "Theme",
            description: "Color theme for the interface",
            current: current.theme.clone(),
            values: None,
            submenu: Some(SettingsSubmenu::Theme {
                themes: current.available_themes.clone(),
            }),
            applies: Applies::Now,
        },
    ]
}

/// The row values the menu opens with: the daemon state plus the settings
/// seam reads.
#[derive(Debug, Clone, Default)]
pub struct SettingsCurrentValues {
    pub autocompact: bool,
    pub compaction_threshold_percent: f64,
    pub show_images: bool,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub skill_commands: bool,
    pub builtin_skills: bool,
    pub harness_refinements: bool,
    pub web_search_provider: String,
    pub hardware_cursor: bool,
    pub emoji_shortcodes: bool,
    pub editor_padding: u64,
    pub autocomplete_max_visible: u64,
    pub clear_on_shrink: bool,
    pub terminal_progress: bool,
    pub idle_eviction_minutes: String,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub transport: String,
    pub default_service_tier: String,
    pub mermaid: String,
    pub quiet_startup: bool,
    pub tree_filter_mode: String,
    pub warnings_anthropic_extra_usage: bool,
    pub thinking_level: Option<String>,
    pub available_thinking_levels: Vec<String>,
    pub theme: String,
    pub available_themes: Vec<String>,
}

impl SettingsCurrentValues {
    /// The settings' own defaults (the values a fresh install reads): the menu tints rows that
    /// differ and Delete resets to them. Session-owned values with no fixed default (thinking,
    /// transport) stay empty, so they never tint.
    #[must_use]
    pub(crate) fn defaults() -> Self {
        SettingsCurrentValues {
            autocompact: true,
            compaction_threshold_percent: 95.0,
            show_images: true,
            auto_resize_images: true,
            block_images: false,
            skill_commands: true,
            builtin_skills: true,
            harness_refinements: true,
            web_search_provider: "serper".to_string(),
            hardware_cursor: false,
            emoji_shortcodes: true,
            editor_padding: 0,
            autocomplete_max_visible: 5,
            clear_on_shrink: false,
            terminal_progress: false,
            idle_eviction_minutes: "90".to_string(),
            steering_mode: "all".to_string(),
            follow_up_mode: "one-at-a-time".to_string(),
            transport: String::new(),
            default_service_tier: "default".to_string(),
            mermaid: "streaming".to_string(),
            quiet_startup: false,
            tree_filter_mode: "user-only".to_string(),
            warnings_anthropic_extra_usage: true,
            thinking_level: None,
            available_thinking_levels: Vec::new(),
            theme: "prime".to_string(),
            available_themes: Vec::new(),
        }
    }
}

impl SettingsMenu {
    /// Compare the rows against these defaults: a differing row is tinted, Delete resets it.
    #[must_use]
    pub(crate) fn with_defaults(mut self, defaults: &SettingsCurrentValues) -> Self {
        self.defaults = settings_menu_rows(defaults)
            .into_iter()
            .filter(|row| !row.current.is_empty())
            .map(|row| (row.id, row.current))
            .collect();
        self
    }

    /// The row's default when it differs from the row's current value.
    fn changed_default(&self, row: &SettingsMenuRow) -> Option<&str> {
        self.defaults
            .get(row.id)
            .map(String::as_str)
            .filter(|default| !row.current.is_empty() && *default != row.current)
    }

    /// The live `Saved` notice: when it expires and when the change applies; `None` once the
    /// window has passed. The run loop wakes at the expiry so the notice clears on an idle screen.
    #[must_use]
    pub(crate) fn saved_notice(&self) -> Option<(std::time::Instant, Applies)> {
        self.saved
            .map(|(at, applies)| (at + SAVED_NOTICE, applies))
            .filter(|(expiry, _)| *expiry > std::time::Instant::now())
    }

    /// Drop a `Saved` notice whose window has passed; `true` when one went, so the caller
    /// repaints it away (the toasts' `prune_expired` contract).
    pub(crate) fn prune_saved_notice(&mut self, now: std::time::Instant) -> bool {
        let expired = self
            .saved
            .is_some_and(|(at, _)| now.duration_since(at) >= SAVED_NOTICE);
        if expired {
            self.saved = None;
        }
        expired
    }

    /// Note a change for the hint row, keeping the row's previous value until the save answers.
    fn mark_saved(&mut self, row_index: usize, previous: String) {
        self.saved = Some((std::time::Instant::now(), self.rows[row_index].applies));
        self.last_change = Some((row_index, previous));
        self.refusal = None;
    }

    /// The last change's save was refused: the row gets its previous value back, the `Saved`
    /// notice drops, and the reason shows in the detail area while that row is selected.
    pub fn refuse_last_change(&mut self, reason: &str) {
        let Some((row, previous)) = self.last_change.take() else {
            return;
        };
        self.rows[row].current = previous;
        self.saved = None;
        self.refusal = Some(Refusal {
            row,
            reason: reason.to_string(),
        });
    }

    /// Delete on a changed row: put it back to its default (a no-op on a default row).
    fn reset_selected(&mut self) -> SettingsMenuAction {
        let Some(&row_index) = self
            .tabs
            .get(self.tab)
            .and_then(|tab| tab.filtered.get(tab.selected))
        else {
            return SettingsMenuAction::None;
        };
        let Some(default) = self
            .changed_default(&self.rows[row_index])
            .map(str::to_string)
        else {
            return SettingsMenuAction::None;
        };
        let previous = std::mem::replace(&mut self.rows[row_index].current, default.clone());
        self.mark_saved(row_index, previous);
        SettingsMenuAction::Reset {
            id: self.rows[row_index].id,
            value: default,
        }
    }

    #[must_use]
    pub fn new(rows: Vec<SettingsMenuRow>) -> Self {
        let tabs = tabs::row_indices(&rows)
            .into_iter()
            .map(|(name, rows)| SettingsTab {
                name,
                filtered: rows.clone(),
                rows,
                search: SearchInput::new(),
                selected: 0,
            })
            .collect();
        SettingsMenu {
            tabs,
            tab: 0,
            rows,
            sub: None,
            defaults: std::collections::HashMap::new(),
            saved: None,
            last_change: None,
            refusal: None,
            max_visible: 10,
        }
    }

    fn active_mut(&mut self) -> &mut SettingsTab {
        &mut self.tabs[self.tab]
    }

    /// One key id: the submenu, when open, takes it first.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> SettingsMenuAction {
        if let Some(mut sub) = self.sub.take() {
            let action = self.handle_submenu_key(&mut sub, key, kb);
            // A value select, a theme restore, or the submenu's Esc closes the
            // submenu; a plain navigation or live preview keeps it open.
            let closed = matches!(
                action,
                SettingsMenuAction::Change { .. }
                    | SettingsMenuAction::RestoreTheme { .. }
                    | SettingsMenuAction::SubmenuClosed
            );
            if !closed {
                self.sub = Some(sub);
            }
            return action;
        }
        // An empty row set carries no tabs: only the close keys act.
        if self.tabs.is_empty() {
            if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
                return SettingsMenuAction::Cancel;
            }
            return SettingsMenuAction::None;
        }
        // Tab switching (the operator's 2026-09-28 rebind: the tabs move with Tab and the number
        // keys ONLY, freeing the arrows for value cycling): digits jump straight to their tab while
        // the search field is empty (an active query takes digits as search text).
        match key {
            "shift+tab" => {
                self.switch_tab((self.tab + self.tabs.len() - 1) % self.tabs.len());
                return SettingsMenuAction::None;
            }
            "tab" => {
                self.switch_tab((self.tab + 1) % self.tabs.len());
                return SettingsMenuAction::None;
            }
            _ => {}
        }
        // The arrows cycle the focused setting's value in place (a submenu row
        // has no inline values, so they no-op there).
        if key == "left" {
            return self.cycle_selected(-1);
        }
        if key == "right" {
            return self.cycle_selected(1);
        }
        if self.tabs[self.tab].search.value().is_empty() {
            if let [character] = key.chars().collect::<Vec<char>>()[..] {
                if let Some(tab) = character
                    .to_digit(10)
                    .filter(|digit| *digit > 0)
                    .map(|digit| digit as usize - 1)
                    .filter(|tab| *tab < self.tabs.len())
                {
                    self.switch_tab(tab);
                    return SettingsMenuAction::None;
                }
            }
        }
        // Delete resets the selected row while the query is empty (a query takes it as an edit).
        if key == "delete" && self.tabs[self.tab].search.value().is_empty() {
            return self.reset_selected();
        }
        if kb.matches(key, "tui.select.up") {
            let tab = self.active_mut();
            if !tab.filtered.is_empty() {
                tab.selected = if tab.selected == 0 {
                    tab.filtered.len() - 1
                } else {
                    tab.selected - 1
                };
            }
            return SettingsMenuAction::None;
        }
        if kb.matches(key, "tui.select.down") {
            let tab = self.active_mut();
            if !tab.filtered.is_empty() {
                tab.selected = (tab.selected + 1) % tab.filtered.len();
            }
            return SettingsMenuAction::None;
        }
        if key == "space" || kb.matches(key, "tui.select.confirm") {
            return self.activate_selected();
        }
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
            // Esc clears the active tab's query first; the next Esc closes.
            if self.tabs[self.tab].search.value().is_empty() {
                return SettingsMenuAction::Cancel;
            }
            self.active_mut().search.prefill("");
            self.apply_filter();
            return SettingsMenuAction::None;
        }
        // TS `SettingsList.handleInput`: Space stays the row activation
        // above; every other key id goes whole to the active tab's search
        // input.
        let tab = self.active_mut();
        let previous = tab.search.value().to_string();
        tab.search.handle_key(key, kb);
        // The filter re-runs only when the query changed (the config
        // selector's rule): a caret-only key keeps the selection.
        let changed = tab.search.value() != previous;
        if changed {
            self.apply_filter();
        }
        SettingsMenuAction::None
    }

    /// One bracketed paste into the active tab's search field (TS routes
    /// the raw paste data to the `Input`): the sanitize strips the spaces
    /// (Space stays the row activation), and the filter re-runs when the
    /// query changed.
    pub fn paste(&mut self, text: &str) {
        // The submenu owns the frame while open (its key dispatch takes
        // every key first): a paste never edits the hidden parent list.
        if self.sub.is_some() {
            return;
        }
        // An empty row set carries no tabs (the menu renders its empty
        // state): nothing to paste into.
        if self.tabs.is_empty() {
            return;
        }
        let sanitized = text.replace(' ', "");
        let tab = self.active_mut();
        let previous = tab.search.value().to_string();
        tab.search.paste(&sanitized);
        let changed = tab.search.value() != previous;
        if changed {
            self.apply_filter();
        }
    }

    /// Enter/Space on the selection (TS `activateItem`): submenus open;
    /// value rows cycle to the next value.
    fn activate_selected(&mut self) -> SettingsMenuAction {
        let Some(tab) = self.tabs.get(self.tab) else {
            return SettingsMenuAction::None;
        };
        let Some(&row_index) = tab.filtered.get(tab.selected) else {
            return SettingsMenuAction::None;
        };
        let row = &mut self.rows[row_index];
        if let Some(kind) = row.submenu.clone() {
            // The tier submenu preselects the row's current tier.
            let selected = match &kind {
                SettingsSubmenu::ServiceTier => SERVICE_TIER_CHOICES
                    .iter()
                    .position(|tier| *tier == row.current)
                    .unwrap_or(0),
                _ => 0,
            };
            self.sub = Some(SubmenuState::new(row_index, kind, selected));
            return SettingsMenuAction::None;
        }
        self.cycle_selected(1)
    }

    /// Cycle the selected row's value by `delta` steps, wrapping at the list's ends, and report the
    /// change. A submenu row (no inline values) and a missing selection no-op.
    fn cycle_selected(&mut self, delta: isize) -> SettingsMenuAction {
        let Some(tab) = self.tabs.get(self.tab) else {
            return SettingsMenuAction::None;
        };
        let Some(&row_index) = tab.filtered.get(tab.selected) else {
            return SettingsMenuAction::None;
        };
        let row = &mut self.rows[row_index];
        let Some(values) = &row.values else {
            return SettingsMenuAction::None;
        };
        let count = values.len();
        let index = values
            .iter()
            .position(|value| *value == row.current)
            .map_or(0, |index| {
                (index as isize + delta).rem_euclid(count as isize) as usize
            });
        let value = values[index].clone();
        let previous = std::mem::replace(&mut row.current, value.clone());
        let id = row.id;
        self.mark_saved(row_index, previous);
        SettingsMenuAction::Change { id, value }
    }

    /// One key inside an open submenu: Enter selects, Esc goes back.
    fn handle_submenu_key(
        &mut self,
        sub: &mut SubmenuState,
        key: &str,
        kb: &KeybindingsManager,
    ) -> SettingsMenuAction {
        if kb.matches(key, "tui.select.up") || kb.matches(key, "tui.select.down") {
            if sub.filtered.is_empty() {
                return SettingsMenuAction::None;
            }
            sub.step(if kb.matches(key, "tui.select.up") {
                -1
            } else {
                1
            });
            return Self::submenu_selection_change(sub);
        }
        if kb.matches(key, "tui.select.confirm") || key == "space" {
            // A query that hides every option leaves nothing to select.
            if sub.filtered.is_empty() {
                return SettingsMenuAction::None;
            }
            let row_id = self.rows[sub.row].id;
            let value = match &sub.kind {
                SettingsSubmenu::Thinking { levels } => {
                    levels
                        .get(sub.selected)
                        .cloned()
                        .map(|level| SettingsMenuAction::Change {
                            id: row_id,
                            value: level,
                        })
                }
                SettingsSubmenu::Theme { themes } => {
                    themes
                        .get(sub.selected)
                        .cloned()
                        .map(|name| SettingsMenuAction::Change {
                            id: row_id,
                            value: name,
                        })
                }
                SettingsSubmenu::Warnings => Some(SettingsMenuAction::Change {
                    id: "warnings-anthropic-extra-usage",
                    value: if sub.selected == 0 {
                        "true".into()
                    } else {
                        "false".into()
                    },
                }),
                SettingsSubmenu::ServiceTier => SERVICE_TIER_CHOICES
                    .get(sub.selected)
                    .copied()
                    .map(|tier| SettingsMenuAction::Change {
                        id: row_id,
                        value: tier.to_string(),
                    }),
            };
            // A value select closes the submenu and updates the row's displayed value.
            if let Some(action) = value {
                if let SettingsMenuAction::Change { value, .. } = &action {
                    let previous =
                        std::mem::replace(&mut self.rows[sub.row].current, value.clone());
                    self.mark_saved(sub.row, previous);
                }
                return action;
            }
            return SettingsMenuAction::None;
        }
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
            // Esc clears a non-empty query first; the next Esc goes back.
            if !sub.search.value().is_empty() {
                sub.search.prefill("");
                let previous = sub.selected;
                sub.refilter();
                if sub.selected != previous {
                    return Self::submenu_selection_change(sub);
                }
                return SettingsMenuAction::None;
            }
            // Theme: Esc restores the row's theme; every submenu just goes back.
            return self.submenu_cancel(sub);
        }
        // Every other key edits the submenu's search.
        let previous = (sub.search.value().to_string(), sub.selected);
        sub.search.handle_key(key, kb);
        if sub.search.value() != previous.0 {
            sub.refilter();
            if sub.selected != previous.1 {
                return Self::submenu_selection_change(sub);
            }
        }
        SettingsMenuAction::None
    }

    /// The selection-change side effect: only the theme submenu previews live.
    fn submenu_selection_change(sub: &SubmenuState) -> SettingsMenuAction {
        match &sub.kind {
            SettingsSubmenu::Theme { themes } => themes
                .get(sub.selected)
                .map_or(SettingsMenuAction::None, |name| {
                    SettingsMenuAction::PreviewTheme { name: name.clone() }
                }),
            _ => SettingsMenuAction::None,
        }
    }

    /// The submenu's Esc behavior: the theme submenu restores the theme the
    /// row opened with.
    fn submenu_cancel(&self, sub: &SubmenuState) -> SettingsMenuAction {
        match &sub.kind {
            SettingsSubmenu::Theme { .. } => SettingsMenuAction::RestoreTheme {
                name: self.rows[sub.row].current.clone(),
            },
            // Every other submenu just goes back; the menu itself stays open.
            _ => SettingsMenuAction::SubmenuClosed,
        }
    }

    /// The active search query: the open submenu's, else the active tab's.
    #[must_use]
    pub fn query(&self) -> &str {
        match (&self.sub, self.tabs.get(self.tab)) {
            (Some(sub), _) => sub.search.value(),
            (None, Some(tab)) => tab.search.value(),
            (None, None) => "",
        }
    }

    /// Switch to a tab, carrying the query: the search is shared across tabs while it is
    /// active, so the strip's per-tab match counts are what the next tab shows. A tab whose query
    /// already matches keeps its selection.
    fn switch_tab(&mut self, tab: usize) {
        let query = self.tabs[self.tab].search.value().to_string();
        self.tab = tab;
        if self.tabs[tab].search.value() != query {
            self.active_mut().search.prefill(&query);
            self.apply_filter();
        }
    }

    /// Re-filter the active tab's rows (fuzzy over the label): a fresh query
    /// lands the tab's selection on its first match.
    fn apply_filter(&mut self) {
        let query = self.tabs[self.tab].search.value().to_string();
        let filtered = self.matching_rows(&self.tabs[self.tab].rows, &query);
        let tab = &mut self.tabs[self.tab];
        tab.filtered = filtered;
        tab.selected = 0;
    }

    /// The row indices of `rows` a query keeps (fuzzy over the label), in match order; an empty
    /// query keeps them all.
    fn matching_rows(&self, rows: &[usize], query: &str) -> Vec<usize> {
        if query.is_empty() {
            return rows.to_vec();
        }
        let candidates: Vec<SettingsMenuRow> =
            rows.iter().map(|&index| self.rows[index].clone()).collect();
        crate::fuzzy::fuzzy_filter(&candidates, query, |row| row.label.to_string())
            .iter()
            .map(|row| {
                rows.iter()
                    .find(|&index| &self.rows[*index] == row)
                    .copied()
                    .expect("fuzzy keeps row values")
            })
            .collect()
    }

    /// Render the shared menu panel over the settings rows; a submenu replaces
    /// the whole list.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<crate::Line> {
        if let Some(sub) = &self.sub {
            return Self::render_submenu(sub, theme, width, kb);
        }
        let mut lines: Vec<crate::Line> = vec![title_row(theme, width, "Settings")];
        if self.rows.is_empty() {
            lines.push(crate::menu_panel::no_match_row(
                theme,
                width,
                "No settings available",
            ));
            lines.push(Footer::new().esc(kb, EscVerb::Close).line(theme, width));
            return lines;
        }
        let tab = &self.tabs[self.tab];
        lines.extend(crate::menu_panel::search_field_lines(
            theme,
            width,
            tab.search.value(),
            tab.search.cursor(),
            true,
            "Search settings",
        ));
        // The tab strip sits under the search field with a blank row on either
        // side (the operator's 2026-09-28 spacing pass).
        lines.push(Vec::new());
        let names: Vec<&'static str> = self.tabs.iter().map(|tab| tab.name).collect();
        // While a query is active every tab shows how many of its rows the query matches.
        let query = tab.search.value();
        let counts: Vec<usize> = self
            .tabs
            .iter()
            .map(|other| self.matching_rows(&other.rows, query).len())
            .collect();
        let counts = if query.is_empty() {
            tabs::StripCounts::Hidden
        } else {
            tabs::StripCounts::Matches(&counts)
        };
        lines.push(tabs::strip_row(theme, width, &names, self.tab, counts));
        lines.push(Vec::new());
        // The list keeps its unfiltered height while searching, so the rows below never move.
        let list_start = lines.len();
        let held =
            tab.rows.len().min(self.max_visible) + usize::from(tab.rows.len() > self.max_visible);
        let footer = self.footer(kb);
        // The detail area keeps the height of the tab's tallest description (the minimal hold; a
        // detail-pane rework may replace it), so neither moving to a shorter description,
        // starting a query, nor a no-match query moves the rows below.
        let description_width = width.saturating_sub(4);
        let held_description = tab
            .rows
            .iter()
            .map(|&row_index| self.rows[row_index].description)
            .filter(|description| !description.is_empty())
            .map(|description| wrap_text(description, description_width).len())
            .max();
        if tab.filtered.is_empty() {
            lines.push(empty_state_row(
                theme,
                width,
                kb,
                tab.search.value(),
                "No settings in this tab",
            ));
            crate::menu_chrome::hold_list_height(&mut lines, list_start, held);
            // A query that hides every row still holds the rows a match would show: the detail
            // block and the Enter line.
            if !query.is_empty() {
                if let Some(rows) = held_description {
                    lines.extend(std::iter::repeat_with(Vec::new).take(rows + 1));
                    lines.push(crate::menu_panel::rule_row(theme, width));
                }
                if kb.first_key("tui.select.confirm").is_some() {
                    lines.push(Vec::new());
                }
            }
            lines.push(footer.line(theme, width));
            return lines;
        }
        let start = tab
            .selected
            .saturating_sub(self.max_visible / 2)
            .min(tab.filtered.len().saturating_sub(self.max_visible));
        let end = (start + self.max_visible).min(tab.filtered.len());
        for (position, &row_index) in tab.filtered[start..end].iter().enumerate() {
            let position = start + position;
            let row = &self.rows[row_index];
            let selected = position == tab.selected;
            // A value changed from its default is tinted on both label and value.
            let (label, value) = if self.changed_default(row).is_some() {
                (
                    theme.fg_span(ThemeColor::Warning, row.label),
                    crate::menu_panel::MenuSegment::themed(ThemeColor::Warning, &row.current),
                )
            } else {
                (
                    crate::Span::raw(row.label),
                    crate::menu_panel::MenuSegment::muted(&row.current),
                )
            };
            lines.push(crate::menu_panel::menu_row(
                theme,
                width,
                vec![label],
                &[value],
                selected,
            ));
        }
        if start > 0 || end < tab.filtered.len() {
            lines.push(crate::menu_panel::scroll_row(
                theme,
                width,
                tab.selected + 1,
                tab.filtered.len(),
            ));
        }
        crate::menu_chrome::hold_list_height(&mut lines, list_start, held);
        if let Some(&row_index) = tab.filtered.get(tab.selected) {
            let row = &self.rows[row_index];
            if !row.description.is_empty() || held_description.is_some() {
                // A shorter description pads above the block, so the block, its rule, and the
                // Enter line stay anchored to the footer. A refused save shows its reason in
                // place of the description, in the error tone, within the same rows.
                let (text, color) = match self
                    .refusal
                    .as_ref()
                    .filter(|refusal| refusal.row == row_index)
                {
                    Some(refusal) => (format!("\u{2717} {}", refusal.reason), ThemeColor::Error),
                    None => (row.description.to_string(), ThemeColor::Dim),
                };
                let rows = held_description
                    .unwrap_or(0)
                    .max(wrap_text(row.description, description_width).len())
                    .max(1);
                let mut description = wrap_text(&text, description_width);
                description.truncate(rows);
                let padding = rows.saturating_sub(description.len());
                lines.extend(std::iter::repeat_with(Vec::new).take(padding));
                lines.push(Vec::new());
                for line in description {
                    let plain: String = line.iter().map(|span| span.content.as_str()).collect();
                    lines.push(crate::width::truncate_line(
                        &vec![
                            crate::Span::raw("  ".to_string()),
                            crate::Span::styled(plain, theme.fg_style(color)),
                        ],
                        width,
                        "",
                    ));
                }
                // The separator rule closes the detail block (the operator's 2026-09-28
                // directive).
                lines.push(crate::menu_panel::rule_row(theme, width));
            }
        }
        // The Enter line: a submenu row opens, a value row steps to its next value.
        let enter = tab.filtered.get(tab.selected).map(|&row_index| {
            let row = &self.rows[row_index];
            match &row.values {
                Some(values) => {
                    let next = values
                        .iter()
                        .position(|value| *value == row.current)
                        .map_or(0, |index| (index + 1) % values.len());
                    format!(
                        "sets {} to {}",
                        row.label.to_lowercase(),
                        values.get(next).map_or("", String::as_str)
                    )
                }
                None => format!("opens {}", row.label),
            }
        });
        lines.extend(footer.enter(kb, enter.as_deref()).rows(theme, width));
        lines
    }

    /// The menu's footer: Tab, the digit row, the arrows, Space, and Delete are literal keys the
    /// menu always handles; an unbound Enter or Esc drops its hint. For three seconds after a
    /// change the row leads with `Saved` and when the change takes effect; a changed selected row
    /// offers Delete to reset it.
    fn footer(&self, kb: &KeybindingsManager) -> Footer {
        let tabs = self.tabs.len();
        let mut footer = Footer::new();
        if let Some((_, applies)) = self.saved_notice() {
            footer = footer.notice(ThemeColor::Success, applies.hint());
        }
        let selected_changed = self
            .tabs
            .get(self.tab)
            .and_then(|tab| tab.filtered.get(tab.selected))
            .is_some_and(|&row_index| self.changed_default(&self.rows[row_index]).is_some());
        footer = footer
            .bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "navigate",
                HintRank::Useful,
            )
            .keys(["left", "right"], "change", HintRank::Useful);
        if selected_changed {
            footer = footer.keys(["delete"], "reset", HintRank::Useful);
        }
        footer
            .keys(
                ["tab".to_string(), format!("1-{tabs}")],
                "tabs",
                HintRank::Extra,
            )
            .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
            .esc(kb, EscVerb::Close)
    }

    /// The submenu render: accent title, muted description, the shared menu
    /// rows, and the back hint.
    fn render_submenu(
        sub: &SubmenuState,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
    ) -> Vec<crate::Line> {
        let (title, description, options): (&str, &str, Vec<(String, Option<&'static str>)>) =
            match &sub.kind {
                SettingsSubmenu::Thinking { levels } => (
                    "Thinking Level",
                    "Select reasoning depth for thinking-capable models",
                    levels
                        .iter()
                        .map(|level| (level.clone(), Some(thinking_description(level))))
                        .collect(),
                ),
                SettingsSubmenu::Theme { themes } => (
                    "Theme",
                    "Select color theme",
                    themes.iter().map(|name| (name.clone(), None)).collect(),
                ),
                SettingsSubmenu::Warnings => (
                    "Warnings",
                    "Enable or disable individual warnings",
                    vec![(String::from("Anthropic extra usage"), None)],
                ),
                SettingsSubmenu::ServiceTier => (
                    "Default Service Tier",
                    "Service tier for new sessions; applies to the current session when the model supports it",
                    SERVICE_TIER_CHOICES
                        .iter()
                        .map(|tier| (tier.to_string(), Some(service_tier_description(tier))))
                        .collect(),
                ),
            };
        let mut lines: Vec<crate::Line> = Vec::new();
        // The list view's full-width top rule stays over the submenu too (the
        // operator's 2026-09-28 regression pin).
        lines.push(crate::menu_panel::rule_row(theme, width));
        // The setting's name and description carry the list rows' padding-x (the
        // operator's 2026-09-28 regression pin).
        lines.push(title_row(
            theme,
            width,
            &format!("Settings \u{203a} {title}"),
        ));
        if !description.is_empty() {
            lines.push(Vec::new());
            lines.push(crate::width::truncate_line(
                &vec![
                    crate::Span::raw("  ".to_string()),
                    theme.fg_span(ThemeColor::Muted, description.to_string()),
                ],
                width,
                "",
            ));
        }
        lines.extend(crate::menu_panel::search_field_lines(
            theme,
            width,
            sub.search.value(),
            sub.search.cursor(),
            /*focused*/ true,
            "Search",
        ));
        let visible_options: Vec<(usize, &(String, Option<&'static str>))> = sub
            .filtered
            .iter()
            .filter_map(|&index| options.get(index).map(|option| (index, option)))
            .collect();
        let selected_position = visible_options
            .iter()
            .position(|(index, _)| *index == sub.selected)
            .unwrap_or(0);
        let max_visible = visible_options.len().min(10);
        let start = selected_position
            .saturating_sub(max_visible / 2)
            .min(visible_options.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(visible_options.len());
        if visible_options.is_empty() {
            lines.push(empty_state_row(
                theme,
                width,
                kb,
                sub.search.value(),
                "No options available",
            ));
        }
        for (index, (value, description)) in visible_options[start..end].iter().copied() {
            let selected = index == sub.selected;
            let trailing: Vec<crate::menu_panel::MenuSegment> = description
                .map(|description| vec![crate::menu_panel::MenuSegment::muted(description)])
                .unwrap_or_default();
            lines.push(crate::menu_panel::menu_row(
                theme,
                width,
                vec![crate::Span::raw(value.clone())],
                &trailing,
                selected,
            ));
        }
        lines.push(Vec::new());
        let enter = visible_options
            .iter()
            .find(|(index, _)| *index == sub.selected)
            .map(|(_, (value, _))| format!("sets {} to {value}", title.to_lowercase()));
        lines.extend(
            Footer::new()
                .enter(kb, enter.as_deref())
                .bound(
                    kb,
                    &["tui.select.up", "tui.select.down"],
                    "navigate",
                    HintRank::Useful,
                )
                .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
                .esc(kb, EscVerb::Back)
                .rows(theme, width),
        );
        lines
    }
}

#[cfg(test)]
mod menu_tests;
