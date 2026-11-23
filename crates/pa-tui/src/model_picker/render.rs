//! The inline panel frame of the `/model` picker: bordered search field, the filter-chip row,
//! group headers over the model rows (the current-model mark, the name with its effort squares,
//! the right-aligned columns), right-edge scrollbar, selection detail, key hint, one blank line
//! below the hint (the operator's 2026-09-24 spacing directive). The search field and rows are
//! the shared inline menu panel's.

use pa_types::ai::{Model, ModelThinkingLevel};
use std::fmt::Write;

use super::filters::{BillingFilter, CapabilityFilter, ContextFilter, SortKey};
use super::rows::{ListRow, RowColumns, SignIn};
use super::{EffortLayout, ModelPicker};
use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{empty_state_row, EscVerb, Footer, HintRank, CHROME_INDENT};
use crate::menu_panel::{finish_menu_row, menu_row, search_field_lines, MenuSegment};
use crate::theme::{Theme, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};

const SEARCH_PLACEHOLDER: &str = "Search models";

const PRICE_UNIT_TEXT: &str = "$ / 1M tokens";

const PRICE_COLUMN_MIN_WIDTH: usize = 13;

/// Render the picker's frame: the inline menu panel plus the model/effort hint row.
pub(super) fn render(
    picker: &mut ModelPicker,
    theme: &Theme,
    width: usize,
    kb: &KeybindingsManager,
) -> Vec<Line> {
    picker.set_render_width(width);
    picker.set_visible_items(picker.list_layout());
    if let Some(lines) = picker.render_overlay(theme, width, kb) {
        return lines;
    }

    // The scope row renders ABOVE the search field (TS v0.9.7's headerHelpContainer mounts before
    // the search input): one leading space, the active side accented, then the dim toggle key and
    // muted description, truncated with "...".
    let mut lines = vec![crate::menu_chrome::title_row(theme, width, "Model")];
    if picker.has_scoped_models() {
        let muted = |text: &str| theme.fg_span(ThemeColor::Muted, text.to_string());
        let (all, scoped) = if picker.scoped_side() {
            (muted("all"), theme.fg_span(ThemeColor::Accent, "scoped"))
        } else {
            (theme.fg_span(ThemeColor::Accent, "all"), muted("scoped"))
        };
        let mut line = vec![
            muted(" Scope: "),
            all,
            muted(" | "),
            scoped,
            muted(" \u{b7} "),
        ];
        if let Some(key) = kb
            .first_key("app.model.toggleScope")
            .as_deref()
            .map(crate::keybindings::format_key_text)
        {
            line.push(theme.fg_span(ThemeColor::Dim, key));
            line.push(muted(" scope (all/scoped)"));
        }
        lines.push(crate::width::truncate_line(&line, width, "..."));
    }
    lines.extend(search_field_lines(
        theme,
        width,
        picker.query(),
        picker.search_cursor(),
        true,
        SEARCH_PLACEHOLDER,
    ));

    lines.push(chip_row(picker, theme, width, kb));

    // While searching, the list keeps the height of the browse list (the height it had when the
    // query started), so filtering never moves the rows around it. The overflow scrollbar rides
    // the rows' right edge, so the held height is the rows alone.
    let picker: &ModelPicker = picker;
    let list_start = lines.len();
    let held = picker.list_layout_over(picker.browse_row_count());
    let (start, end) = picker.display_window();
    let rows = &picker.display_rows()[start..end];
    let total_rows = picker.display_rows().len();
    let filtered_len = picker.filtered_len();
    let window_models: Vec<&Model> = rows
        .iter()
        .filter_map(|row| match row {
            ListRow::Model(position) => picker
                .filtered_index(*position)
                .and_then(|index| picker.model_at(index)),
            ListRow::Header { .. } => None,
        })
        .collect();
    // An overflowing window carries the slash menu's one-cell scrollbar on its right edge instead
    // of an `(n/m)` row; the rows give up that column plus a one-space gap, so the flush-right
    // columns never touch the bar.
    let scrollbar = (start > 0 || end < total_rows)
        .then(|| crate::menu_panel::scrollbar_cells(theme, end - start, total_rows, start));
    let row_width = width.saturating_sub(2 * usize::from(scrollbar.is_some()));
    let columns = RowColumns::fit(picker, &window_models, row_width);
    let effort_layout = ModelPicker::effort_layout(&window_models, row_width, columns.width());
    let selected_position = picker.selected_index();
    for (offset, display_row) in rows.iter().enumerate() {
        let mut row = match display_row {
            ListRow::Header { label, sign_in } => {
                let mut header: Line = vec![
                    Span::raw(CHROME_INDENT),
                    theme.fg_span(ThemeColor::Muted, label.clone()),
                ];
                match sign_in {
                    SignIn::Ready => {}
                    SignIn::Required => {
                        header.push(theme.fg_span(
                            ThemeColor::Dim,
                            format!(" \u{b7} {}", super::rows::SIGN_IN_TEXT),
                        ));
                    }
                }
                finish_menu_row(theme, &header, row_width, false)
            }
            ListRow::Model(position) => {
                let Some(model) = picker
                    .filtered_index(*position)
                    .and_then(|index| picker.model_at(index))
                else {
                    continue;
                };
                let selected = *position == selected_position;
                let mut primary: Line = if picker.is_current(model) {
                    vec![
                        theme.fg_span(ThemeColor::Accent, "\u{25cf}"),
                        Span::raw(" "),
                    ]
                } else if picker.is_favorite(model) {
                    vec![
                        theme.fg_span(ThemeColor::Accent, "\u{2605}"),
                        Span::raw(" "),
                    ]
                } else {
                    vec![Span::raw("  ")]
                };
                primary.extend(row_primary(picker, theme, model, selected, effort_layout));
                let cell = columns.cell(picker, model);
                menu_row(
                    theme,
                    row_width,
                    primary,
                    &[MenuSegment::muted(&cell)],
                    selected,
                )
            }
        };
        if let Some(cell) = scrollbar.as_ref().and_then(|cells| cells.get(offset)) {
            row.push(Span::raw(" "));
            row.push(cell.clone());
        }
        lines.push(row);
    }

    if filtered_len == 0 {
        lines.push(empty_state_row(
            theme,
            width,
            kb,
            picker.query(),
            "No models available",
        ));
    }
    crate::menu_chrome::hold_list_height(&mut lines, list_start, held);
    if let Some(model) = picker.selected_model().cloned() {
        if picker.detail_rows() > 0 {
            lines.extend(detail_lines(theme, width, &model));
        }
    }

    let enter = picker
        .selected_model()
        .map(|model| format!("switches to {}", model.name));
    lines.extend(
        Footer::new()
            .enter(kb, enter.as_deref())
            .bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "model",
                HintRank::Useful,
            )
            .bound(
                kb,
                &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
                "effort",
                HintRank::Useful,
            )
            .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
            .bound(kb, &["app.model.actions"], "actions", HintRank::Useful)
            .bound(kb, &["app.model.sort"], "sort", HintRank::Extra)
            .esc(kb, EscVerb::Close)
            .rows(theme, width),
    );

    lines.push(Vec::new());
    lines
}

/// The chip row under the search field: the sort and each filter chip, a non-default value in
/// the accent. The chips carry their keys when the whole row fits, else the bare chips.
fn chip_row(picker: &ModelPicker, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Line {
    let filters = picker.filters();
    let provider = filters.provider.as_deref().unwrap_or("all");
    let chips: [(&str, &str, &str, bool); 5] = [
        (
            "app.model.sort",
            "sort",
            picker.sort_key().label(),
            picker.sort_key() != SortKey::Recent,
        ),
        (
            "app.model.filterProvider",
            "provider",
            provider,
            filters.provider.is_some(),
        ),
        (
            "app.model.filterBilling",
            "billing",
            filters.billing.label(),
            filters.billing != BillingFilter::Any,
        ),
        (
            "app.model.filterCapability",
            "caps",
            filters.capability.label(),
            filters.capability != CapabilityFilter::Any,
        ),
        (
            "app.model.filterContext",
            "ctx",
            filters.context.label(),
            filters.context != ContextFilter::Any,
        ),
    ];
    let build = |keyed: bool| -> Line {
        let mut line: Line = vec![Span::raw(CHROME_INDENT)];
        for (index, (binding, name, value, active)) in chips.iter().enumerate() {
            if index > 0 {
                line.push(Span::raw("  "));
            }
            if keyed {
                if let Some(key) = kb.first_key(binding) {
                    line.push(theme.fg_span(
                        ThemeColor::Dim,
                        format!("{} ", crate::keybindings::format_key_text(&key)),
                    ));
                }
            }
            line.push(theme.fg_span(ThemeColor::Muted, format!("{name}: ")));
            let color = if *active {
                ThemeColor::Accent
            } else {
                ThemeColor::Muted
            };
            line.push(theme.fg_span(color, (*value).to_string()));
        }
        line
    };
    let keyed = build(true);
    if crate::width::spans_width(&keyed) <= width {
        return keyed;
    }
    crate::width::truncate_line(&build(false), width, "\u{2026}")
}

/// The primary cell of one row: the model name, or the name with the inline
/// effort cluster when the window fits it.
fn row_primary(
    picker: &ModelPicker,
    theme: &Theme,
    model: &Model,
    selected: bool,
    layout: EffortLayout,
) -> Line {
    let name = model.name.clone();
    if !layout.show_cluster {
        return vec![Span::raw(name)];
    }
    let levels = ModelPicker::selectable_levels(model);
    let Some(effort) = picker.effort_of(model) else {
        return vec![Span::raw(name)];
    };
    if levels.is_empty() {
        return vec![Span::raw(name)];
    }
    let mut primary: Line = vec![Span::raw(truncate_pad(
        &name,
        layout.name_column,
        "\u{2026}",
    ))];
    primary.push(Span::raw(" ".repeat(layout.gap)));
    if selected {
        primary.push(theme.fg_span(ThemeColor::Dim, "\u{2190}"));
    } else {
        primary.push(Span::raw(" "));
    }
    primary.push(Span::raw(" "));
    primary.extend(effort_square_spans(
        theme,
        &levels,
        effort,
        layout.square_slots,
        selected,
    ));
    primary.push(Span::raw(" "));
    if selected {
        primary.push(theme.fg_span(ThemeColor::Dim, "\u{2192}"));
    } else {
        primary.push(Span::raw(" "));
    }
    primary.push(Span::raw(" "));
    if layout.show_label {
        let mut label = effort.wire_name().to_string();
        let pad = layout.label_width.saturating_sub(label.chars().count());
        label.push_str(&" ".repeat(pad));
        primary.push(theme.fg_span(ThemeColor::Muted, label));
    }
    primary
}

/// The effort squares: one slot per on-level, filled up to the current effort,
/// padded to the row's slot count.
fn effort_square_spans(
    theme: &Theme,
    levels: &[ModelThinkingLevel],
    effort: ModelThinkingLevel,
    square_slots: usize,
    selected: bool,
) -> Line {
    let on_levels: Vec<ModelThinkingLevel> = levels
        .iter()
        .copied()
        .filter(|level| *level != ModelThinkingLevel::Off)
        .collect();
    if on_levels.is_empty() {
        return Vec::new();
    }
    let filled = if effort == ModelThinkingLevel::Off {
        0
    } else {
        on_levels
            .iter()
            .position(|level| *level == effort)
            .map_or(0, |position| position + 1)
    };
    let mut spans: Line = Vec::with_capacity(on_levels.len() + 1);
    for (index, _) in on_levels.iter().enumerate() {
        if index < filled {
            // Filled squares: the effort pastel on the selected row, muted elsewhere.
            if selected {
                let style = theme.effort_square_style();
                spans.push(Span::styled("\u{25a0}".to_string(), style));
            } else {
                spans.push(theme.fg_span(ThemeColor::Muted, "\u{25a0}"));
            }
        } else {
            spans.push(theme.fg_span(ThemeColor::Dim, "\u{25a1}"));
        }
    }
    let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    if used < square_slots {
        spans.push(Span::raw(" ".repeat(square_slots - used)));
    }
    spans
}

/// The selected model's price detail block (TS `renderInlineModelDetails`).
fn detail_lines(theme: &Theme, width: usize, model: &Model) -> Vec<Line> {
    let price = |value: Option<f64>| -> String {
        let Some(value) = value else {
            return "\u{2014}".to_string();
        };
        if !value.is_finite() || value < 0.0 {
            return "\u{2014}".to_string();
        }
        if value == 0.0 {
            return "$0".to_string();
        }
        let rounded = (value * 1000.0).round() / 1000.0;
        if rounded == 0.0 {
            return "<$0.001".to_string();
        }
        format!("${rounded}")
    };
    let entries = [
        ("Input", price(Some(model.cost.input.as_f64()))),
        ("Cached input", price(Some(model.cost.cache_read.as_f64()))),
        ("Output", price(Some(model.cost.output.as_f64()))),
    ];
    let unit = theme.fg_span(ThemeColor::Muted, PRICE_UNIT_TEXT);
    let mut lines: Vec<Line> = Vec::new();
    if width >= 58 {
        let column_width = PRICE_COLUMN_MIN_WIDTH
            .max((width.saturating_sub(2 + str_width(PRICE_UNIT_TEXT) + 1)) / 3);
        let row = |index: usize| -> String {
            entries
                .iter()
                .fold(String::new(), |mut output, (label, value)| {
                    let cell = if index == 0 {
                        (*label).to_string()
                    } else {
                        value.clone()
                    };
                    let pad = column_width.saturating_sub(cell.chars().count());
                    let _ = write!(output, "{cell}{}", " ".repeat(pad));
                    output
                })
        };
        // The block shares the title's, the rows' and the footer's text column.
        let mut labels = vec![
            Span::raw(CHROME_INDENT),
            theme.fg_span(ThemeColor::Muted, row(0)),
        ];
        labels.push(Span::raw(" "));
        labels.push(unit);
        lines.push(vec![Span::raw(" ")]);
        lines.push(labels);
        lines.push(vec![Span::raw(CHROME_INDENT), Span::raw(row(1))]);
        lines.push(vec![Span::raw(" ")]);
    } else {
        for (position, (label, value)) in entries.iter().enumerate() {
            let mut line = vec![
                Span::raw(CHROME_INDENT),
                theme.fg_span(ThemeColor::Muted, format!("{label}:")),
                Span::raw(format!(" {value}")),
            ];
            // The unit reads as a note on the last price, not as a second price.
            if position == entries.len() - 1 {
                line.push(theme.fg_span(ThemeColor::Muted, format!("  ({PRICE_UNIT_TEXT})")));
            }
            lines.push(line);
        }
    }
    lines
        .into_iter()
        .map(|line| {
            let mut line = line;
            let used: usize = line.iter().map(|span| str_width(&span.content)).sum();
            if used < width {
                line.push(Span::raw(" ".repeat(width - used)));
            }
            crate::width::truncate_line(&line, width, "\u{2026}")
        })
        .collect()
}

/// Truncate to a width and pad to it.
fn truncate_pad(text: &str, width: usize, ellipsis: &str) -> String {
    if width == 0 {
        return String::new();
    }
    let ellipsis_width = str_width(ellipsis);
    if str_width(text) <= width {
        let mut out = text.to_string();
        let pad = width.saturating_sub(str_width(text));
        out.push_str(&" ".repeat(pad));
        return out;
    }
    if ellipsis_width >= width {
        return " ".repeat(width);
    }
    let mut out = String::new();
    let mut used = 0;
    let target = width - ellipsis_width;
    for character in text.chars() {
        let character_width = crate::width::char_width(character);
        if used + character_width > target {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push_str(ellipsis);
    out
}
