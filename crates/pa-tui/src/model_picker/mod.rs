//! The `/model` inline selector: a bordered "Search models" field and a filter-chip row over
//! `›`-marker rows — the current-model mark, the name with its effort squares, and right-aligned
//! columns (capability glyphs, context window, price or billing class, the provider's cached
//! quota) — grouped by provider under the recent models, a price-detail block, and the key hint.
//! The daemon supplies the catalog (bundled fallback) and each model's billing mode; the quota is
//! the last cached `/usage` card. This module owns ordering, grouping, filtering, sorting, effort
//! state, and the inline geometry; nothing here fetches.

mod render;

use std::collections::{HashMap, HashSet};

use pa_types::ai::{
    clamp_thinking_level, get_supported_thinking_levels, Model, ModelServing, ModelThinkingLevel,
    PRIME_INFERENCE_PROVIDER_ID,
};
use pa_types::billing::BillingMode;

use crate::keybindings::KeybindingsManager;
use crate::search_input::SearchInput;
use crate::theme::Theme;

/// The session's current model, matched against the catalog (the key is provider plus id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentModel {
    pub provider: String,
    pub model_id: String,
}

/// The model Enter applies, plus the effort the user edited (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectionApplied {
    pub provider: String,
    pub model_id: String,
    pub effort: Option<String>,
}

/// One key press while the picker is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPickerAction {
    /// Enter on a model: the caller applies it.
    Apply(Box<ModelSelectionApplied>),
    /// Esc, Ctrl+C, or back: close without applying.
    Cancel,
    /// Navigation, filtering, or effort editing only.
    None,
    /// The scope key toggled the picker's list (the picker stays mounted).
    ScopeToggled { scoped: bool },
}

/// The outcome of dispatching `/model [search]`.
#[derive(Debug)]
pub(crate) enum ModelCommandOutcome {
    /// Open the picker over the catalog.
    Open(Box<ModelPicker>),
}

/// The catalog snapshot plus the client state the picker needs.
#[derive(Debug, Default)]
pub struct ModelPickerOptions {
    /// The full catalog (bundled or daemon-refreshed).
    pub models: Vec<Model>,
    /// The session's model, checked `current` and leading the list.
    pub current: Option<CurrentModel>,
    /// Providers with configured auth (`configuredProviders`).
    pub configured_providers: HashSet<String>,
    /// The settings recent-model list (`provider/id` keys, newest first).
    pub recent_models: Vec<String>,
    /// The session's scoped models as `provider/id` keys: the picker opens on them when non-empty,
    /// else the full catalog; the keys resolve against the loaded catalog (a missing entry is not
    /// listed).
    pub scoped_models: Vec<String>,
    /// The effort a fresh selection starts from (TS `thinkingLevel`).
    pub thinking_level: Option<ModelThinkingLevel>,
    /// The viewport height the list sizes itself against (already the `getRows` value).
    pub viewport_rows: usize,
    /// Each model's billing mode (`provider/id` keys) from the daemon's catalog response; a model
    /// without one is classed by its catalog rates.
    pub billing: HashMap<String, BillingMode>,
    /// The tightest cached quota window per provider id (`5h 42%`), from the last `/usage` cards.
    pub quota_labels: HashMap<String, String>,
}

mod facts;
mod filters;
mod rows;
mod scope;
mod search;
mod sort;

use facts::{billing_class, BillingClass};
use filters::{Chip, PickerFilters, SortKey};
use rows::ListRow;
use scope::ModelScope;
use search::{score_model_search, SearchMatch};
use sort::{natural_cmp, version_desc, version_key};

/// How one picker run ended (the adoption event's `outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPickerOutcome {
    /// Enter applied a model.
    Switched,
    /// The picker closed without applying.
    Closed,
}

impl ModelPickerOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ModelPickerOutcome::Switched => "switched",
            ModelPickerOutcome::Closed => "closed",
        }
    }
}

/// The adoption event's primitives (`tui model picker closed`): counts and enums only, never a
/// model, a provider, or the search text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPickerSummary {
    pub outcome: ModelPickerOutcome,
    /// Filter-chip key presses.
    pub chip_change_count: u64,
    /// Sort key presses.
    pub sort_change_count: u64,
    /// Whether any chip was active when the picker closed.
    pub filters_active: bool,
    /// The sort the picker closed on (`recent`, `name`, `price`, `context`).
    pub sort: &'static str,
}

#[cfg(test)]
mod tests;

/// Effort-column layout for the visible window (TS `EffortLayout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortLayout {
    name_column: usize,
    square_slots: usize,
    gap: usize,
    label_width: usize,
    show_label: bool,
    show_cluster: bool,
}

/// One picker over the model catalog.
#[derive(Debug)]
pub struct ModelPicker {
    all_models: Vec<Model>,
    current: Option<CurrentModel>,
    configured_providers: HashSet<String>,
    recent_rank: HashMap<String, usize>,
    /// The effort a fresh selection starts from; `None` resolves to "off" per model.
    initial_thinking_level: Option<ModelThinkingLevel>,
    /// The viewport row budget (TS `getRows`).
    viewport_rows: usize,
    search: SearchInput,
    filtered: Vec<usize>,
    selected: usize,
    /// True once the user moves into the list; left/right then adjust the highlighted
    /// model's effort instead of the search cursor.
    navigated_into_list: bool,
    /// Resolved effort per model key, seeded for every catalog entry with a thinking surface.
    effort_levels: HashMap<String, ModelThinkingLevel>,
    /// Models whose effort the user edited (Enter passes the effort only for these).
    edited_effort: HashSet<String>,
    render_width: usize,
    visible_items: usize,
    last_query: String,
    /// The session's scoped list as `provider/id` keys; empty is the unscoped picker.
    scoped_models: Vec<String>,
    /// The scoped entries' positions in `all_models`, in the scoped list's own order
    /// (the scoped view keeps the session's scope order, not the catalog's sorted one).
    scoped_positions: Vec<usize>,
    /// The active list: `Scoped` while the session holds scoped models, else the full catalog.
    scope: ModelScope,
    /// The version runs parsed from each `all_models` entry's id, indexed alike
    /// (the search sort's recency tier: the ids' versions stand in for release recency).
    version_keys: Vec<Vec<String>>,
    /// Each model's billing mode from the catalog response (`provider/id` keys).
    billing: HashMap<String, BillingMode>,
    /// The tightest cached quota window per provider id.
    quota_labels: HashMap<String, String>,
    /// The filter chips.
    filters: PickerFilters,
    /// The browse order.
    sort: SortKey,
    /// The display rows: group headers and the filtered models (by filtered position).
    rows: Vec<ListRow>,
    /// The browse list's row count (the height a search holds).
    browse_rows: usize,
    /// Chip and sort key presses this run (the adoption event).
    chip_changes: u64,
    sort_changes: u64,
}

impl ModelPicker {
    #[must_use]
    pub fn new(options: ModelPickerOptions) -> Self {
        let mut picker = ModelPicker {
            all_models: Vec::new(),
            current: options.current,
            configured_providers: options.configured_providers,
            recent_rank: options
                .recent_models
                .iter()
                .enumerate()
                .map(|(index, key)| (key.clone(), index))
                .collect(),
            initial_thinking_level: options.thinking_level,
            viewport_rows: options.viewport_rows,
            search: SearchInput::new(),
            filtered: Vec::new(),
            selected: 0,
            navigated_into_list: false,
            effort_levels: HashMap::new(),
            edited_effort: HashSet::new(),
            render_width: 80,
            visible_items: 8,
            last_query: String::new(),
            scoped_models: options.scoped_models,
            scoped_positions: Vec::new(),
            scope: ModelScope::All,
            version_keys: Vec::new(),
            billing: options.billing,
            quota_labels: options.quota_labels,
            filters: PickerFilters::default(),
            sort: SortKey::Recent,
            rows: Vec::new(),
            browse_rows: 0,
            chip_changes: 0,
            sort_changes: 0,
        };
        picker.load_models(options.models);
        picker.resolve_scope_positions();
        if picker.has_scoped_models() {
            picker.scope = ModelScope::Scoped;
        }
        let query = picker.search.value().to_string();
        picker.filter_models(&query);
        picker.select_current_or_top();
        picker
    }

    #[must_use]
    pub fn query(&self) -> &str {
        self.search.value()
    }

    /// Replace the catalog snapshot (the daemon refresh landing): re-sort, re-filter with the live
    /// query, and keep the selection on the same model when it survived the refresh.
    pub fn update_state(
        &mut self,
        current: Option<CurrentModel>,
        models: Vec<Model>,
        configured_providers: HashSet<String>,
        billing: HashMap<String, BillingMode>,
    ) {
        self.current = current;
        self.configured_providers = configured_providers;
        self.billing = billing;
        let selected_key = self.selected_key();
        self.load_models(models);
        // The refresh re-resolves the scoped keys against it: an entry missing
        // from the loaded catalog is not listed, and one the refresh brings appears.
        self.resolve_scope_positions();
        self.refilter_keeping(selected_key);
    }

    /// The selected model's `provider/id` key.
    fn selected_key(&self) -> Option<String> {
        self.selected_model()
            .map(|model| Self::model_key_provider(&model.provider, &model.id))
    }

    /// Re-filter with the live query and keep the selection on `key` when it is still listed.
    fn refilter_keeping(&mut self, key: Option<String>) {
        let query = self.search.value().to_string();
        self.filter_models(&query);
        if let Some(key) = key {
            if let Some(index) = self
                .filtered
                .iter()
                .position(|&index| self.key_at(index) == key)
            {
                self.selected = index;
            }
        }
    }

    /// The adoption event's primitives for this run, ended with `outcome`.
    #[must_use]
    pub fn summary(&self, outcome: ModelPickerOutcome) -> ModelPickerSummary {
        ModelPickerSummary {
            outcome,
            chip_change_count: self.chip_changes,
            sort_change_count: self.sort_changes,
            filters_active: self.filters != PickerFilters::default(),
            sort: self.sort.label(),
        }
    }

    /// One key id. Cancel keys close without applying; Enter applies the selection (plus the
    /// user-edited effort); everything else navigates, edits the filter, or adjusts effort.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> ModelPickerAction {
        // The full-screen selector loop treats Ctrl+C as process exit; the in-chat
        // overlay only cancels, like the TS model selector.
        if key == "ctrl+c" {
            return ModelPickerAction::Cancel;
        }
        // The scope toggle goes first: it toggles only with scoped entries,
        // re-filters, and re-selects the current model, else the top.
        if kb.matches(key, "app.model.toggleScope")
            || kb.matches_option_composed(key, "app.model.toggleScope")
        {
            if self.has_scoped_models() {
                self.scope = match self.scope {
                    ModelScope::All => ModelScope::Scoped,
                    ModelScope::Scoped => ModelScope::All,
                };
                let query = self.search.value().to_string();
                self.filter_models(&query);
                self.select_current_or_top();
                return ModelPickerAction::ScopeToggled {
                    scoped: self.scoped_side(),
                };
            }
            return ModelPickerAction::None;
        }
        for (binding, chip) in [
            ("app.model.filterProvider", Chip::Provider),
            ("app.model.filterBilling", Chip::Billing),
            ("app.model.filterCapability", Chip::Capability),
            ("app.model.filterContext", Chip::Context),
        ] {
            if kb.matches(key, binding) {
                let providers = self.active_providers();
                self.filters.cycle(chip, &providers);
                self.chip_changes += 1;
                self.refilter_keeping(self.selected_key());
                return ModelPickerAction::None;
            }
        }
        if kb.matches(key, "app.model.sort") {
            self.sort = self.sort.next();
            self.sort_changes += 1;
            self.refilter_keeping(self.selected_key());
            return ModelPickerAction::None;
        }
        if self.search.value().is_empty() || self.navigated_into_list {
            for (binding, direction) in
                [("tui.editor.cursorLeft", -1), ("tui.editor.cursorRight", 1)]
            {
                if kb.matches(key, binding) {
                    if let Some(model) = self.selected_model().cloned() {
                        if self.adjust_effort(&model, direction) {
                            return ModelPickerAction::None;
                        }
                    }
                }
            }
        }
        if kb.matches(key, "tui.select.up") {
            let count = self.filtered.len();
            if count > 0 {
                self.navigated_into_list = true;
                self.selected = if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                };
            }
            return ModelPickerAction::None;
        }
        if kb.matches(key, "tui.select.down") {
            let count = self.filtered.len();
            if count > 0 {
                self.navigated_into_list = true;
                self.selected = if self.selected == count - 1 {
                    0
                } else {
                    self.selected + 1
                };
            }
            return ModelPickerAction::None;
        }
        if kb.matches(key, "tui.select.pageUp") || kb.matches(key, "tui.select.pageDown") {
            let direction = if kb.matches(key, "tui.select.pageUp") {
                -(self.visible_items as isize)
            } else {
                self.visible_items as isize
            };
            self.navigated_into_list = true;
            let count = self.filtered.len();
            if count > 0 {
                let target = self.selected as isize + direction;
                self.selected = target.clamp(0, count as isize - 1) as usize;
            }
            return ModelPickerAction::None;
        }
        if kb.matches(key, "tui.select.confirm") {
            return self.confirm();
        }
        if kb.matches(key, "tui.select.cancel") {
            // Esc clears a non-empty query first; the next Esc closes.
            if self.search.value().is_empty() {
                return ModelPickerAction::Cancel;
            }
            self.search.prefill("");
            self.navigated_into_list = false;
            self.filter_models("");
            return ModelPickerAction::None;
        }
        if self.should_treat_as_back(kb, key) {
            return ModelPickerAction::Cancel;
        }
        let previous = self.search.value().to_string();
        self.search.handle_key(key, kb);
        if self.search.value() != previous {
            self.navigated_into_list = false;
            let query = self.search.value().to_string();
            self.filter_models(&query);
        }
        ModelPickerAction::None
    }

    pub fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
    ) -> Vec<crate::Line> {
        render::render(self, theme, width, kb)
    }

    /// The inline list layout for the current width. The frame's trailing blank row (the operator's
    /// 2026-09-24 spacing directive) rides the reserved rows: the list shrinks first on a
    /// height-limited terminal, never the frame's own head.
    pub(crate) fn list_layout(&self) -> usize {
        self.list_layout_over(self.rows.len())
    }

    /// The list layout over `rows` candidate rows: the filtered list for the frame, the whole
    /// active list for the height a search holds.
    pub(crate) fn list_layout_over(&self, rows: usize) -> usize {
        let detail_rows = self.detail_rows();
        crate::menu_panel::menu_list_layout(
            Some(self.viewport_rows),
            8,
            rows,
            // The frame's fixed rows (the title, the search field, the chip row, the Enter line,
            // the footer, the trailing blank) plus the scope row when the session has scoped
            // models.
            7 + usize::from(self.has_scoped_models()) + detail_rows,
            // The scrollbar rides the rows' right edge, so overflow costs no row.
            0,
        )
    }

    pub(crate) fn detail_rows(&self) -> usize {
        let detail_rows = if self.render_width >= 58 { 4 } else { 5 };
        // The fixed floor is the search field (3), the hint (1), and the trailing blank (1): the
        // detail block needs that plus its own rows to render at all.
        if self.viewport_rows >= 5 + detail_rows {
            detail_rows
        } else {
            0
        }
    }

    pub(crate) fn set_render_width(&mut self, width: usize) {
        self.render_width = width;
    }

    pub(crate) fn set_visible_items(&mut self, visible: usize) {
        self.visible_items = visible;
    }

    #[must_use]
    pub fn visible_items(&self) -> usize {
        self.visible_items
    }

    pub(crate) fn selected_model(&self) -> Option<&Model> {
        self.all_models.get(*self.filtered.get(self.selected)?)
    }

    pub(crate) fn filtered_index(&self, position: usize) -> Option<usize> {
        self.filtered.get(position).copied()
    }

    pub(crate) fn filtered_len(&self) -> usize {
        self.filtered.len()
    }

    /// The display rows the list window shows (`rows` positions): the selected model's row
    /// centered where the list allows.
    pub(crate) fn display_window(&self) -> (usize, usize) {
        let max_visible = self.visible_items.max(1);
        let selected_row = self.selected_row();
        let start = selected_row
            .saturating_sub(max_visible / 2)
            .min(self.rows.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.rows.len());
        (start, end)
    }

    /// The display row holding the selected model.
    fn selected_row(&self) -> usize {
        let selected = self.selected_index();
        self.rows
            .iter()
            .position(|row| *row == ListRow::Model(selected))
            .unwrap_or(0)
    }

    pub(crate) fn display_rows(&self) -> &[ListRow] {
        &self.rows
    }

    /// The browse list's row count: the height a search holds the list at.
    pub(crate) fn browse_row_count(&self) -> usize {
        self.browse_rows
    }

    /// The frame rows above the list (the click surface's chrome): the title, the scope row when
    /// scoped, the bordered search field, and the chip row.
    pub(crate) fn rows_above_list(&self) -> usize {
        5 + usize::from(self.has_scoped_models())
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected.min(self.filtered.len().saturating_sub(1))
    }

    /// Move the selection to the model on one display row: a group header or a row past the list
    /// keeps the selection; a click lands the user in the list, so arrows adjust the clicked row's
    /// effort.
    pub(crate) fn select_display_row(&mut self, row: usize) {
        if let Some(ListRow::Model(position)) = self.rows.get(row) {
            self.selected = *position;
            self.navigated_into_list = true;
        }
    }

    /// The active list's providers in display order (the provider chip's cycle).
    fn active_providers(&self) -> Vec<String> {
        let mut providers: Vec<String> = Vec::new();
        for index in self.active_indices() {
            let provider = &self.all_models[index].provider;
            if !providers.contains(provider) {
                providers.push(provider.clone());
            }
        }
        providers
    }

    /// One model's billing class.
    pub(crate) fn class_of(&self, model: &Model) -> BillingClass {
        billing_class(
            self.billing
                .get(&Self::model_key_provider(&model.provider, &model.id)),
            model,
        )
    }

    pub(crate) fn filters(&self) -> &PickerFilters {
        &self.filters
    }

    pub(crate) fn sort_key(&self) -> SortKey {
        self.sort
    }

    pub(crate) fn quota_label(&self, model: &Model) -> Option<&str> {
        self.quota_labels.get(&model.provider).map(String::as_str)
    }

    /// Whether the browse list groups by provider under the recent models: the whole catalog,
    /// no query, the recent sort.
    pub(crate) fn grouped(&self) -> bool {
        matches!(self.scope, ModelScope::All)
            && self.search.value().trim().is_empty()
            && self.sort == SortKey::Recent
    }

    /// The provider-sorted catalog (TS `sortModels`).
    fn load_models(&mut self, models: Vec<Model>) {
        let mut models = models;
        models.sort_by(|a, b| self.compare(a, b));
        self.all_models = models;
        self.version_keys = self
            .all_models
            .iter()
            .map(|model| version_key(&model.id))
            .collect();
        self.resolve_effort_defaults();
        let current_index = self
            .all_models
            .iter()
            .position(|model| self.is_current(model));
        match current_index {
            Some(index) => self.selected = index,
            None => {
                self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            }
        }
    }

    fn compare(&self, a: &Model, b: &Model) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        // Configured providers first.
        let configured = |model: &Model| self.configured_providers.contains(&model.provider);
        match (configured(b), configured(a)) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        // Signed-in Prime Inference pinned above the rest.
        let pinned =
            |model: &Model| model.provider == PRIME_INFERENCE_PROVIDER_ID && configured(model);
        match (pinned(b), pinned(a)) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        // The current model leads.
        let is_current = |model: &Model| self.is_current(model);
        match (is_current(a), is_current(b)) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // Then the recent-use rank.
        let rank = |model: &Model| {
            self.recent_rank
                .get(&Self::model_key_provider(&model.provider, &model.id))
                .copied()
                .unwrap_or(usize::MAX)
        };
        let (a_rank, b_rank) = (rank(a), rank(b));
        if a_rank != b_rank {
            return a_rank.cmp(&b_rank);
        }
        // Then the provider name.
        if a.provider != b.provider {
            return a.provider.cmp(&b.provider);
        }
        // Featured models of the same provider lead.
        let featured = |model: &Model| model.featured == Some(true);
        match (featured(a), featured(b)) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // Finally the model id, numeric-aware.
        natural_cmp(&a.id, &b.id)
    }

    /// Rebuild the filtered view: a non-empty query scores every model and orders the matches by
    /// configured provider, pin, match quality, score, version descending, currency, recent rank,
    /// and key. The selection resets to the top only when the query changed.
    fn filter_models(&mut self, query: &str) {
        let query_changed = query != self.last_query;
        self.last_query = query.to_string();
        let candidates: Vec<usize> = self
            .active_indices()
            .into_iter()
            .filter(|&index| {
                let model = &self.all_models[index];
                self.filters.admits(model, self.class_of(model))
            })
            .collect();
        if query.trim().is_empty() {
            let (filtered, rows) = self.browse_order(candidates);
            self.filtered = filtered;
            self.rows = rows;
            self.browse_rows = self.rows.len();
        } else {
            if self.browse_rows == 0 {
                // A prefilled search holds the list at the candidates' height.
                self.browse_rows = candidates.len();
            }
            let mut matches: Vec<(usize, SearchMatch)> = candidates
                .into_iter()
                .filter_map(|index| {
                    score_model_search(&self.all_models[index], query).map(|match_| (index, match_))
                })
                .collect();
            let configured = |model: &Model| self.configured_providers.contains(&model.provider);
            let pinned =
                |model: &Model| model.provider == PRIME_INFERENCE_PROVIDER_ID && configured(model);
            matches.sort_by(|(a_index, a_match), (b_index, b_match)| {
                use std::cmp::Ordering;
                let a = &self.all_models[*a_index];
                let b = &self.all_models[*b_index];
                match (configured(b), configured(a)) {
                    (true, false) => return Ordering::Greater,
                    (false, true) => return Ordering::Less,
                    _ => {}
                }
                match (pinned(b), pinned(a)) {
                    (true, false) => return Ordering::Greater,
                    (false, true) => return Ordering::Less,
                    _ => {}
                }
                if a_match.quality != b_match.quality {
                    return a_match.quality.cmp(&b_match.quality);
                }
                if a_match.score.partial_cmp(&b_match.score) != Some(Ordering::Equal) {
                    return a_match
                        .score
                        .partial_cmp(&b_match.score)
                        .unwrap_or(Ordering::Equal);
                }
                // The version tier: newer releases first (the version runs parsed from
                // the ids stand in for release recency).
                let order =
                    version_desc(&self.version_keys[*a_index], &self.version_keys[*b_index]);
                if order != Ordering::Equal {
                    return order;
                }
                match (self.is_current(b), self.is_current(a)) {
                    (true, false) => return Ordering::Greater,
                    (false, true) => return Ordering::Less,
                    _ => {}
                }
                let (a_rank, b_rank) = (
                    self.recent_rank
                        .get(&Self::model_key_provider(&a.provider, &a.id))
                        .copied()
                        .unwrap_or(usize::MAX),
                    self.recent_rank
                        .get(&Self::model_key_provider(&b.provider, &b.id))
                        .copied()
                        .unwrap_or(usize::MAX),
                );
                if a_rank != b_rank {
                    return a_rank.cmp(&b_rank);
                }
                natural_cmp(
                    &Self::model_key_provider(&a.provider, &a.id),
                    &Self::model_key_provider(&b.provider, &b.id),
                )
            });
            self.filtered = matches.into_iter().map(|(index, _)| index).collect();
            self.rows = (0..self.filtered.len()).map(ListRow::Model).collect();
        }
        self.selected = if query_changed {
            0
        } else {
            self.selected.min(self.filtered.len().saturating_sub(1))
        };
        self.visible_items = self.list_layout();
    }

    fn should_treat_as_back(&self, kb: &KeybindingsManager, key: &str) -> bool {
        // Left arrow acts like Esc only when the search cursor sits at the
        // field's start.
        kb.matches(key, "app.modal.back") && self.search.cursor() == 0
    }

    fn confirm(&self) -> ModelPickerAction {
        let Some(model) = self.selected_model() else {
            return ModelPickerAction::None;
        };
        let key = Self::model_key_provider(&model.provider, &model.id);
        let effort = if self.edited_effort.contains(&key) {
            self.effort_levels
                .get(&key)
                .map(|level| level.wire_name().to_string())
        } else {
            None
        };
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            effort,
        }))
    }

    /// The selectable effort levels of a model; an off-only model has none.
    pub(crate) fn selectable_levels(model: &Model) -> Vec<ModelThinkingLevel> {
        let levels = get_supported_thinking_levels(model);
        if levels.len() == 1 && levels[0] == ModelThinkingLevel::Off {
            return Vec::new();
        }
        levels
    }

    /// Seed the default effort for every model with a thinking surface (resolved
    /// eagerly so rendering stays pure).
    fn resolve_effort_defaults(&mut self) {
        let initial = self
            .initial_thinking_level
            .unwrap_or(ModelThinkingLevel::Off);
        for model in &self.all_models {
            let levels = Self::selectable_levels(model);
            if levels.is_empty() {
                continue;
            }
            let key = Self::model_key_provider(&model.provider, &model.id);
            if self.effort_levels.contains_key(&key) {
                continue;
            }
            let stored = self.effort_levels.get(&key).copied();
            let level = match stored {
                Some(level) if levels.contains(&level) => level,
                _ => {
                    let level = if levels.contains(&initial) {
                        initial
                    } else {
                        clamp_thinking_level(model, initial)
                    };
                    if levels.contains(&level) {
                        level
                    } else {
                        levels[0]
                    }
                }
            };
            self.effort_levels.insert(key, level);
        }
    }

    pub(crate) fn effort_of(&self, model: &Model) -> Option<ModelThinkingLevel> {
        if Self::selectable_levels(model).is_empty() {
            return None;
        }
        self.effort_levels
            .get(&Self::model_key_provider(&model.provider, &model.id))
            .copied()
    }

    /// Move the model's effort one level (`direction` ±1, wrapping); returns whether the effort
    /// changed.
    fn adjust_effort(&mut self, model: &Model, direction: isize) -> bool {
        let levels = Self::selectable_levels(model);
        if levels.is_empty() {
            return false;
        }
        let key = Self::model_key_provider(&model.provider, &model.id);
        let current = self.effort_of(model).unwrap_or(levels[0]);
        let index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        let len = levels.len() as isize;
        let next = (((index as isize + direction) % len) + len) % len;
        let next_level = levels[next as usize];
        self.effort_levels.insert(key, next_level);
        self.edited_effort
            .insert(Self::model_key_provider(&model.provider, &model.id));
        true
    }

    pub(crate) fn is_current(&self, model: &Model) -> bool {
        self.current.as_ref().is_some_and(|current| {
            current.provider == model.provider && current.model_id == model.id
        })
    }

    pub(crate) fn is_configured(&self, model: &Model) -> bool {
        self.configured_providers.contains(&model.provider)
    }

    pub(crate) fn model_at(&self, index: usize) -> Option<&Model> {
        self.all_models.get(index)
    }

    fn key_at(&self, index: usize) -> String {
        self.all_models
            .get(index)
            .map(|model| Self::model_key_provider(&model.provider, &model.id))
            .unwrap_or_default()
    }

    pub(crate) fn model_key_provider(provider: &str, id: &str) -> String {
        format!("{provider}/{id}")
    }

    pub(crate) fn search_cursor(&self) -> usize {
        self.search.cursor()
    }

    /// The effort cluster's layout over the window's `models` in a `width`-wide row, beside a
    /// `trailing_width`-wide column cell.
    pub(crate) fn effort_layout(
        &self,
        models: &[&Model],
        width: usize,
        trailing_width: usize,
    ) -> EffortLayout {
        /// Wide detail columns must fit "Cached input".
        const EFFORT_NAME_COLUMN_MAX: usize = 30;
        const EFFORT_NAME_COLUMN_MIN: usize = 12;
        const ARROWS_AND_GAPS: usize = 6;

        let empty = EffortLayout {
            name_column: 0,
            square_slots: 0,
            gap: 0,
            label_width: 0,
            show_label: false,
            show_cluster: false,
        };
        let reasoning: Vec<&Model> = models
            .iter()
            .copied()
            .filter(|model| !Self::selectable_levels(model).is_empty())
            .collect();
        if reasoning.is_empty() {
            return empty;
        }
        // The row's marker cell (`● `) sits before the name.
        let available = width
            .saturating_sub(2 + MARK_WIDTH + trailing_width + 2)
            .max(1);

        let max_name_column = reasoning
            .iter()
            .map(|model| crate::width::str_width(&model.name))
            .max()
            .unwrap_or(0)
            .min(EFFORT_NAME_COLUMN_MAX);
        let square_slots = reasoning
            .iter()
            .map(|model| {
                Self::selectable_levels(model)
                    .iter()
                    .filter(|level| **level != ModelThinkingLevel::Off)
                    .count()
            })
            .max()
            .unwrap_or(0);
        let cluster_width = square_slots;
        // Fixed label cell sized to the longest supported level name, so
        // changing the level never changes the cluster span.
        let label_width = reasoning
            .iter()
            .flat_map(|model| Self::selectable_levels(model))
            .map(|level| crate::width::str_width(level.wire_name()))
            .max()
            .unwrap_or(0);
        // Sit the cluster near the row's horizontal center, clamped between
        // the name column and the trailing zone.
        let place = |name_column: usize, show_label: bool| {
            // The arrows, their spaces, and the space after the squares (plus the label cell).
            let span = cluster_width + if show_label { label_width + 5 } else { 5 };
            let desired = (width / 2)
                .saturating_sub(span / 2)
                .saturating_sub(2 + MARK_WIDTH + name_column);
            let gap = desired
                .min(available.saturating_sub(name_column + span))
                .max(1);
            EffortLayout {
                name_column,
                square_slots,
                gap,
                label_width,
                show_label,
                show_cluster: true,
            }
        };
        if max_name_column + cluster_width + label_width + ARROWS_AND_GAPS <= available {
            return place(max_name_column, true);
        }
        let label_name_column =
            available.saturating_sub(cluster_width + label_width + ARROWS_AND_GAPS);
        if label_name_column >= EFFORT_NAME_COLUMN_MIN {
            return place(max_name_column.min(label_name_column), true);
        }
        if max_name_column + cluster_width + ARROWS_AND_GAPS <= available {
            return place(max_name_column, false);
        }
        let cluster_name_column = available.saturating_sub(cluster_width + ARROWS_AND_GAPS);
        if cluster_name_column >= EFFORT_NAME_COLUMN_MIN {
            return place(max_name_column.min(cluster_name_column), false);
        }
        empty
    }
}

/// The current-model mark's cell before each row's name (`● `).
pub(crate) const MARK_WIDTH: usize = 2;

/// The provider a row displays (and search matches): Prime Inference routes
/// split into their serving class (the live catalog labels each route hosted
/// or gateway); unlabeled routes and every other provider show the provider id.
pub(crate) fn provider_label(model: &Model) -> &str {
    match (model.provider.as_str(), model.serving) {
        (PRIME_INFERENCE_PROVIDER_ID, Some(ModelServing::Hosted)) => "prime-hosted",
        (PRIME_INFERENCE_PROVIDER_ID, Some(ModelServing::Gateway)) => "prime-gateway",
        _ => model.provider.as_str(),
    }
}

impl ModelPicker {
    /// Dispatch `/model [search]`: open the picker with `current` checked and `search` prefilled;
    /// TS always opens the menu — an empty catalog renders the empty panel, never a note.
    pub(crate) fn open(options: ModelPickerOptions, search: &str) -> ModelCommandOutcome {
        let mut picker = ModelPicker::new(options);
        let search = search.trim();
        if !search.is_empty() {
            picker.set_query(search);
        }
        ModelCommandOutcome::Open(Box::new(picker))
    }

    /// A bracketed paste into the search field (newlines stripped, tabs expanded).
    pub fn paste(&mut self, text: &str) {
        let previous = self.search.value().to_string();
        self.search.paste(text);
        if self.search.value() != previous {
            self.navigated_into_list = false;
            let query = self.search.value().to_string();
            self.filter_models(&query);
        }
    }

    /// Prefill the filter (`/model <search>`), the caret at the search's end so
    /// typing extends it.
    pub fn set_query(&mut self, query: &str) {
        self.search.prefill(query);
        self.filter_models(query);
    }
}
