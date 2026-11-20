//! The per-model action menu (Tab on a highlighted model) and its details view: switch the
//! session to the model, make it the default for new sessions, set its effort, toggle it as a
//! favorite, or read its full metadata with the cached spend and quota. Esc steps back to the
//! list. Everything shown is data the client already holds: nothing here fetches.

#[cfg(test)]
mod tests;

use pa_types::ai::{Model, ModelInput, ModelThinkingLevel};
use pa_types::provider_usage::{ProviderQuota, QuotaState};

use super::facts::{context_cell, money, price_cell};
use super::{ModelPicker, ModelPickerAction, ModelSelectionApplied};
use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{EscVerb, Footer, HintRank, CHROME_INDENT};
use crate::menu_panel::finish_menu_row;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// One row of the action menu. The rows a model offers come from [`ModelPicker::action_rows`]
/// in menu order; a new per-model choice joins as its own variant there, with its label in
/// [`ModelPicker::action_label`] and its Enter arm in [`ModelPicker::activate_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelAction {
    /// Switch this session to the model: the same explicit `set_model` path as Enter on the list.
    Switch,
    /// Make the model the default for new sessions.
    SetDefault,
    /// The model's effort: ←/→ step it; Enter applies it to the session's current model, or
    /// switches to a non-current model at that effort (the row says which).
    Effort,
    /// The stored credential new sessions pin for the model's provider (only for a provider
    /// holding several credentials): ←/→ step through `default` and each named credential, Enter
    /// keeps the choice for `/new`. The running session keeps its own pin: a credential is never
    /// switched under a live session.
    Account,
    /// Add or remove the model from the favorites.
    Favorite,
    /// The model's full metadata, cached spend, and quota.
    Details,
}

/// Which surface the picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerView {
    /// The model list.
    List,
    /// The action menu over one model (an `all_models` position) and its selected row.
    Actions { model: usize, selected: usize },
    /// The details view over one model; Esc returns to its action menu.
    Details { model: usize },
}

/// One named credential a provider holds (the `get_model_catalog` answer's `accountChoices`):
/// the id a new session pins and the user's own label. Never the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountChoice {
    pub id: String,
    pub label: String,
}

/// The picker's cached usage: the spend history's per-model rows and the quota cards from the
/// last `/usage` load, and the clock the open read them at. Empty until `/usage` loads.
#[derive(Debug, Clone, Default)]
pub struct PickerUsage {
    pub spend: Vec<pa_types::provider_usage::ModelSpend>,
    pub quotas: Vec<ProviderQuota>,
    pub now_ms: i64,
}

impl ModelPicker {
    /// The action rows `model` offers, in menu order: effort only for a model with a thinking
    /// surface, the account only for a provider holding several credentials.
    pub(crate) fn action_rows(&self, model: &Model) -> Vec<ModelAction> {
        let mut rows = vec![ModelAction::Switch, ModelAction::SetDefault];
        if !Self::selectable_levels(model).is_empty() {
            rows.push(ModelAction::Effort);
        }
        if self.account_choices.contains_key(&model.provider) {
            rows.push(ModelAction::Account);
        }
        rows.extend([ModelAction::Favorite, ModelAction::Details]);
        rows
    }

    /// Whether `model` is a favorite.
    pub(crate) fn is_favorite(&self, model: &Model) -> bool {
        self.favorites
            .contains(&Self::model_key_provider(&model.provider, &model.id))
    }

    /// Open the action menu over the highlighted model.
    pub(super) fn open_actions(&mut self) {
        if let Some(model) = self.filtered.get(self.selected_index()).copied() {
            self.view = PickerView::Actions { model, selected: 0 };
            self.action_menu_opens += 1;
        }
    }

    /// One key while the action menu or the details view is up.
    pub(super) fn handle_action_key(
        &mut self,
        key: &str,
        kb: &KeybindingsManager,
    ) -> ModelPickerAction {
        match self.view {
            PickerView::List => ModelPickerAction::None,
            PickerView::Details { model } => {
                if kb.matches(key, "tui.select.cancel") || kb.matches(key, "app.model.actions") {
                    self.view = PickerView::Actions { model, selected: 0 };
                }
                ModelPickerAction::None
            }
            PickerView::Actions { model, selected } => {
                let Some(entry) = self.all_models.get(model).cloned() else {
                    self.view = PickerView::List;
                    return ModelPickerAction::None;
                };
                let rows = self.action_rows(&entry);
                if kb.matches(key, "tui.select.cancel") || kb.matches(key, "app.model.actions") {
                    self.view = PickerView::List;
                    return ModelPickerAction::None;
                }
                if kb.matches(key, "tui.select.up") {
                    let selected = selected.checked_sub(1).unwrap_or(rows.len() - 1);
                    self.view = PickerView::Actions { model, selected };
                    return ModelPickerAction::None;
                }
                if kb.matches(key, "tui.select.down") {
                    let selected = (selected + 1) % rows.len();
                    self.view = PickerView::Actions { model, selected };
                    return ModelPickerAction::None;
                }
                let row = rows.get(selected).copied().unwrap_or(ModelAction::Switch);
                for (binding, direction) in
                    [("tui.editor.cursorLeft", -1), ("tui.editor.cursorRight", 1)]
                {
                    if !kb.matches(key, binding) {
                        continue;
                    }
                    match row {
                        ModelAction::Effort => {
                            self.adjust_effort(&entry, direction);
                        }
                        ModelAction::Account => {
                            let count = self
                                .account_choices
                                .get(&entry.provider)
                                .map_or(0, Vec::len)
                                + 1;
                            let current = self.account_position(&entry.provider);
                            let next = (current + count).wrapping_add_signed(direction) % count;
                            self.account_picks.insert(entry.provider.clone(), next);
                        }
                        ModelAction::Switch
                        | ModelAction::SetDefault
                        | ModelAction::Favorite
                        | ModelAction::Details => {}
                    }
                    return ModelPickerAction::None;
                }
                if kb.matches(key, "tui.select.confirm") {
                    return self.activate_action(model, &entry, row);
                }
                ModelPickerAction::None
            }
        }
    }

    /// Enter on one action row.
    fn activate_action(
        &mut self,
        model: usize,
        entry: &Model,
        row: ModelAction,
    ) -> ModelPickerAction {
        let key = Self::model_key_provider(&entry.provider, &entry.id);
        match row {
            ModelAction::Switch => self.apply(entry),
            ModelAction::SetDefault => {
                self.default_sets += 1;
                ModelPickerAction::SetDefault {
                    provider: entry.provider.clone(),
                    model_id: entry.id.clone(),
                }
            }
            ModelAction::Effort => {
                let Some(level) = self.effort_of(entry) else {
                    return ModelPickerAction::None;
                };
                self.effort_sets += 1;
                if self.is_current(entry) {
                    return ModelPickerAction::SetEffort(level.wire_name().to_string());
                }
                ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
                    provider: entry.provider.clone(),
                    model_id: entry.id.clone(),
                    effort: Some(level.wire_name().to_string()),
                }))
            }
            ModelAction::Account => {
                self.account_sets += 1;
                ModelPickerAction::SetAccount {
                    provider: entry.provider.clone(),
                    account: self.account_choice(&entry.provider).cloned(),
                }
            }
            ModelAction::Favorite => {
                match self.favorites.iter().position(|favorite| *favorite == key) {
                    Some(position) => {
                        self.favorites.remove(position);
                    }
                    None => self.favorites.push(key),
                }
                self.favorite_toggles += 1;
                self.refilter_keeping(self.selected_key());
                ModelPickerAction::FavoritesChanged(self.favorites.clone())
            }
            ModelAction::Details => {
                self.details_opens += 1;
                self.view = PickerView::Details { model };
                ModelPickerAction::None
            }
        }
    }

    /// One action row's label.
    fn action_label(&self, entry: &Model, row: ModelAction) -> String {
        match row {
            ModelAction::Switch if self.is_current(entry) => {
                "Switch this session to it (current)".to_string()
            }
            ModelAction::Switch => "Switch this session to it".to_string(),
            ModelAction::SetDefault => "Set as default for new sessions".to_string(),
            ModelAction::Effort => {
                let level = self
                    .effort_of(entry)
                    .map_or("off", ModelThinkingLevel::wire_name);
                if self.is_current(entry) {
                    format!("Effort \u{2190} {level} \u{2192}")
                } else {
                    format!("Switch at effort \u{2190} {level} \u{2192}")
                }
            }
            ModelAction::Account => format!(
                "Account for new sessions \u{2190} {} \u{2192}",
                self.account_choice(&entry.provider)
                    .map_or("default", |choice| choice.label.as_str())
            ),
            ModelAction::Favorite if self.is_favorite(entry) => "Remove from favorites".to_string(),
            ModelAction::Favorite => "Add to favorites".to_string(),
            ModelAction::Details => "Details".to_string(),
        }
    }

    /// The account row's position for `provider`: 0 is `default`, `n` the `n`th named credential.
    /// It starts on the account this session serves the provider with.
    fn account_position(&self, provider: &str) -> usize {
        if let Some(position) = self.account_picks.get(provider) {
            return *position;
        }
        let session = self.accounts.get(provider);
        self.account_choices
            .get(provider)
            .and_then(|choices| {
                choices
                    .iter()
                    .position(|choice| Some(&choice.label) == session)
            })
            .map_or(0, |position| position + 1)
    }

    /// The named credential the account row stands on for `provider` (`None`: `default`).
    fn account_choice(&self, provider: &str) -> Option<&AccountChoice> {
        let position = self.account_position(provider).checked_sub(1)?;
        self.account_choices.get(provider)?.get(position)
    }

    /// The spend row for `entry` from the cached history, or why there is none.
    fn spend_text(&self, entry: &Model) -> String {
        match self
            .usage
            .spend
            .iter()
            .find(|spend| spend.provider == entry.provider && spend.model == entry.id)
        {
            Some(spend) => format!(
                "today {} \u{b7} 7 days {} \u{b7} all time {} ({} requests)",
                money_cost(spend.today.cost),
                money_cost(spend.last_7_days.cost),
                money_cost(spend.all_time.cost),
                spend.all_time.requests
            ),
            None if self.usage.spend.is_empty() => "not loaded (open /usage)".to_string(),
            None => "none recorded".to_string(),
        }
    }

    /// The quota rows for `entry`'s provider from the cached cards: each window with its reset.
    fn quota_text(&self, entry: &Model) -> String {
        let Some(card) = self.usage.quotas.iter().find(|card| {
            card.provider == entry.provider
                && card.account.as_ref() == self.accounts.get(&entry.provider)
        }) else {
            return "not loaded (open /usage)".to_string();
        };
        let now_ms = self.usage.now_ms;
        match &card.state {
            QuotaState::Windows { windows } => {
                let shown: Vec<String> = windows
                    .iter()
                    .filter_map(|window| {
                        let label =
                            crate::usage_page::window_label(window, window.used_fraction()?);
                        Some(match window.resets_at_ms {
                            Some(at) if at > now_ms => format!(
                                "{label} \u{b7} resets in {}",
                                crate::usage_page::countdown(at - now_ms)
                            ),
                            Some(_) => format!("{label} \u{b7} reset"),
                            None => label,
                        })
                    })
                    .collect();
                if shown.is_empty() {
                    "no limit windows".to_string()
                } else {
                    shown.join("; ")
                }
            }
            QuotaState::RateLimited { .. } => "rate limited".to_string(),
            QuotaState::Failed { .. } => "unavailable".to_string(),
        }
    }

    /// The action menu's or the details view's frame.
    pub(super) fn render_overlay(
        &self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
    ) -> Option<Vec<Line>> {
        let (model, selected) = match self.view {
            PickerView::List => return None,
            PickerView::Actions { model, selected } => (model, Some(selected)),
            PickerView::Details { model } => (model, None),
        };
        let entry = self.all_models.get(model)?;
        let mut lines = vec![crate::menu_chrome::title_row(
            theme,
            width,
            &format!("Model \u{b7} {}", entry.name),
        )];
        let muted = |text: String| theme.fg_span(ThemeColor::Muted, text);
        let subtitle = format!(
            "{CHROME_INDENT}{}/{} \u{b7} {} \u{b7} {} context",
            entry.provider,
            entry.id,
            price_cell(self.class_of(entry), entry),
            context_cell(entry.context_window)
        );
        lines.push(crate::width::truncate_line(
            &vec![muted(subtitle)],
            width,
            "\u{2026}",
        ));
        lines.push(Vec::new());
        if let Some(selected) = selected {
            for (index, row) in self.action_rows(entry).into_iter().enumerate() {
                let is_selected = index == selected;
                let marker = if is_selected { "\u{203a} " } else { "  " };
                let line: Line = vec![
                    theme.fg_span(ThemeColor::Accent, marker),
                    Span::raw(self.action_label(entry, row)),
                ];
                lines.push(finish_menu_row(theme, &line, width, is_selected));
            }
            lines.push(Vec::new());
            lines.extend(
                Footer::new()
                    .bound(
                        kb,
                        &["tui.select.up", "tui.select.down"],
                        "action",
                        HintRank::Useful,
                    )
                    .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
                    .esc(kb, EscVerb::Back)
                    .rows(theme, width),
            );
        } else {
            for (label, value) in self.detail_fields(entry) {
                let line: Line = vec![
                    Span::raw(CHROME_INDENT),
                    muted(format!("{label:<10}")),
                    Span::raw(value),
                ];
                lines.push(crate::width::truncate_line(&line, width, "\u{2026}"));
            }
            lines.push(Vec::new());
            lines.extend(Footer::new().esc(kb, EscVerb::Back).rows(theme, width));
        }
        lines.push(Vec::new());
        Some(lines)
    }

    /// The details view's label/value rows.
    fn detail_fields(&self, entry: &Model) -> Vec<(&'static str, String)> {
        let inputs: Vec<&str> = entry
            .input
            .iter()
            .map(|input| match input {
                ModelInput::Text => "text",
                ModelInput::Image => "image",
            })
            .collect();
        let levels: Vec<&str> = Self::selectable_levels(entry)
            .into_iter()
            .map(ModelThinkingLevel::wire_name)
            .collect();
        let cost = &entry.cost;
        vec![
            ("api", entry.api.clone()),
            (
                "context",
                format!(
                    "{} \u{b7} max output {}",
                    context_cell(entry.context_window),
                    context_cell(entry.max_tokens)
                ),
            ),
            ("input", inputs.join(", ")),
            (
                "effort",
                if levels.is_empty() {
                    "none".to_string()
                } else {
                    levels.join(", ")
                },
            ),
            (
                "price",
                format!(
                    "{} in \u{b7} {} out \u{b7} {} cache read \u{b7} {} cache write per 1M",
                    money(cost.input.as_f64()),
                    money(cost.output.as_f64()),
                    money(cost.cache_read.as_f64()),
                    money(cost.cache_write.as_f64())
                ),
            ),
            ("billing", price_cell(self.class_of(entry), entry)),
            ("spend", self.spend_text(entry)),
            ("quota", self.quota_text(entry)),
        ]
    }
}

/// A recorded dollar amount, to the cent (`$0.42`); a sub-cent spend is `<$0.01`.
fn money_cost(cost: f64) -> String {
    if !cost.is_finite() || cost <= 0.0 {
        return "$0".to_string();
    }
    if cost < 0.01 {
        return "<$0.01".to_string();
    }
    format!("${cost:.2}")
}
