//! The picker's display rows: the browse order (the grouped view — recent models on top, then
//! one group per provider — or one flat list by name, price, or context) and the right-aligned
//! columns each model row shows, sized once per visible window so the columns line up.

use std::cmp::Ordering;

use pa_types::ai::Model;

use super::facts::{capability_cell, context_cell, price_cell, sort_price};
use super::filters::SortKey;
use super::sort::natural_cmp;
use super::{provider_label, ModelPicker};
use crate::width::str_width;

/// One display row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListRow {
    /// A group header: the recent models, or one provider (its label, and whether it needs a
    /// sign-in).
    Header { label: String, sign_in: SignIn },
    /// The model at this filtered position.
    Model(usize),
}

/// Whether a provider group's models need a sign-in before they can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignIn {
    Ready,
    Required,
}

/// The recent models' group header.
pub(crate) const RECENT_HEADER: &str = "recent";

/// The narrowest name cell the columns leave (the marker and name before the effort cluster).
const NAME_MIN_WIDTH: usize = 18;

/// The gap between two columns.
const COLUMN_GAP: &str = "  ";

impl ModelPicker {
    /// The browse order over the filtered `candidates` (`all_models` positions in catalog order):
    /// the filtered list and its display rows.
    pub(super) fn browse_order(&self, candidates: Vec<usize>) -> (Vec<usize>, Vec<ListRow>) {
        if self.grouped() {
            return self.grouped_order(candidates);
        }
        let mut ordered = candidates;
        let model = |index: &usize| &self.all_models[*index];
        let tie = |a: &usize, b: &usize| self.key_at(*a).cmp(&self.key_at(*b));
        match self.sort {
            // The scoped list keeps the session's own order.
            SortKey::Recent => {}
            SortKey::Name => ordered.sort_by(|a, b| {
                natural_cmp(&model(a).name, &model(b).name).then_with(|| tie(a, b))
            }),
            SortKey::Price => ordered.sort_by(|a, b| {
                let price = |index: &usize| sort_price(self.class_of(model(index)), model(index));
                match (price(a), price(b)) {
                    (Some(left), Some(right)) => left.total_cmp(&right),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                }
                .then_with(|| tie(a, b))
            }),
            SortKey::Context => ordered.sort_by(|a, b| {
                model(b)
                    .context_window
                    .cmp(&model(a).context_window)
                    .then_with(|| tie(a, b))
            }),
        }
        let rows = (0..ordered.len()).map(ListRow::Model).collect();
        (ordered, rows)
    }

    /// The grouped view: the current and recent models under the recent header (current first,
    /// then newest use), then each provider's remaining models under its own header, providers in
    /// catalog order (signed-in first).
    fn grouped_order(&self, candidates: Vec<usize>) -> (Vec<usize>, Vec<ListRow>) {
        let pinned_rank = |index: usize| -> Option<usize> {
            let model = &self.all_models[index];
            if self.is_current(model) {
                return Some(0);
            }
            self.recent_rank
                .get(&Self::model_key_provider(&model.provider, &model.id))
                .map(|rank| rank + 1)
        };
        let (mut pinned, rest): (Vec<usize>, Vec<usize>) = candidates
            .into_iter()
            .partition(|index| pinned_rank(*index).is_some());
        pinned.sort_by_key(|index| pinned_rank(*index));
        let mut groups: Vec<(String, SignIn, Vec<usize>)> = Vec::new();
        for index in rest {
            let model = &self.all_models[index];
            let label = provider_label(model);
            match groups.iter_mut().find(|(group, _, _)| group == label) {
                Some((_, _, members)) => members.push(index),
                None => {
                    let sign_in = if self.is_configured(model) {
                        SignIn::Ready
                    } else {
                        SignIn::Required
                    };
                    groups.push((label.to_string(), sign_in, vec![index]));
                }
            }
        }
        let mut filtered = Vec::new();
        let mut rows = Vec::new();
        let mut push_group = |label: String, sign_in: SignIn, members: Vec<usize>| {
            rows.push(ListRow::Header { label, sign_in });
            for index in members {
                rows.push(ListRow::Model(filtered.len()));
                filtered.push(index);
            }
        };
        if !pinned.is_empty() {
            push_group(RECENT_HEADER.to_string(), SignIn::Ready, pinned);
        }
        for (label, sign_in, members) in groups {
            push_group(label, sign_in, members);
        }
        (filtered, rows)
    }
}

/// The right-aligned columns of one visible window, sized to its widest cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowColumns {
    capabilities: bool,
    context_width: usize,
    price_width: usize,
    quota_width: usize,
    provider_width: usize,
    sign_in_width: usize,
}

const SIGN_IN_TEXT: &str = "sign in";

impl RowColumns {
    /// Size the columns for `models` at `row_width`. Columns join while they fit beside the
    /// narrowest name cell, in priority order: price, context, provider (flat views only: the
    /// grouped view names the provider in its header), sign-in, capabilities, quota.
    pub(crate) fn fit(picker: &ModelPicker, models: &[&Model], row_width: usize) -> Self {
        let widest = |cell: &dyn Fn(&Model) -> String| {
            models
                .iter()
                .map(|model| str_width(&cell(model)))
                .max()
                .unwrap_or(0)
        };
        let flat = !picker.grouped();
        let wanted = [
            widest(&|model| price_cell(picker.class_of(model), model)),
            widest(&|model| context_cell(model.context_window)),
            if flat {
                widest(&|model| provider_label(model).to_string())
            } else {
                0
            },
            if flat && models.iter().any(|model| !picker.is_configured(model)) {
                str_width(SIGN_IN_TEXT)
            } else {
                0
            },
            3,
            widest(&|model| picker.quota_label(model).unwrap_or_default().to_string()),
        ];
        let budget = row_width.saturating_sub(2 + 2 + NAME_MIN_WIDTH);
        let mut kept = [0usize; 6];
        let mut used = 0usize;
        for (slot, width) in wanted.iter().enumerate() {
            if *width == 0 {
                continue;
            }
            let cost = width + if used == 0 { 0 } else { COLUMN_GAP.len() };
            if used + cost > budget {
                break;
            }
            used += cost;
            kept[slot] = *width;
        }
        let [price_width, context_width, provider_width, sign_in_width, capabilities, quota_width] =
            kept;
        Self {
            capabilities: capabilities > 0,
            context_width,
            price_width,
            quota_width,
            provider_width,
            sign_in_width,
        }
    }

    /// One model's columns as one padded cell: capabilities, context (right-aligned), price,
    /// quota, provider, sign-in — every row of the window the same width.
    pub(crate) fn cell(&self, picker: &ModelPicker, model: &Model) -> String {
        let mut cells: Vec<String> = Vec::new();
        if self.capabilities {
            cells.push(capability_cell(model));
        }
        if self.context_width > 0 {
            let text = context_cell(model.context_window);
            let pad = self.context_width.saturating_sub(str_width(&text));
            cells.push(format!("{}{text}", " ".repeat(pad)));
        }
        let left = |text: &str, width: usize| crate::width::pad_cell(text, width);
        if self.price_width > 0 {
            cells.push(left(
                &price_cell(picker.class_of(model), model),
                self.price_width,
            ));
        }
        if self.quota_width > 0 {
            cells.push(left(
                picker.quota_label(model).unwrap_or_default(),
                self.quota_width,
            ));
        }
        if self.provider_width > 0 {
            cells.push(left(provider_label(model), self.provider_width));
        }
        if self.sign_in_width > 0 {
            let text = if picker.is_configured(model) {
                ""
            } else {
                SIGN_IN_TEXT
            };
            cells.push(left(text, self.sign_in_width));
        }
        cells.join(COLUMN_GAP)
    }

    /// The width every row's cell takes.
    pub(crate) fn width(&self) -> usize {
        let widths = [
            if self.capabilities { 3 } else { 0 },
            self.context_width,
            self.price_width,
            self.quota_width,
            self.provider_width,
            self.sign_in_width,
        ];
        let shown: Vec<usize> = widths.into_iter().filter(|width| *width > 0).collect();
        shown.iter().sum::<usize>() + COLUMN_GAP.len() * shown.len().saturating_sub(1)
    }
}
