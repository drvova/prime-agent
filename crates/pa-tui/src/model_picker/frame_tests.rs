//! The frame's own contract: what the Enter line and the footer offer for the state the picker
//! is in, the columns a narrow terminal keeps, grapheme-safe name cells, the action menu's
//! footer, clicks and height, and that no row is ever wider than its frame.

use proptest::prelude::*;

use super::tests::{frame_at, kb, rich_options, theme};
use super::*;
use crate::width::str_width;

/// The Enter line at 100 columns.
fn enter_line(picker: &mut ModelPicker) -> String {
    frame_at(picker, 100)
        .into_iter()
        .find(|row| row.starts_with("  enter "))
        .unwrap_or_default()
}

/// The footer row (the frame's last row before the trailing blank) at 100 columns.
fn footer(picker: &mut ModelPicker) -> String {
    let rows = frame_at(picker, 100);
    rows[rows.len() - 2].clone()
}

/// The rich catalog served again by a second signed-in provider: every name lists twice.
fn twice_served() -> ModelPickerOptions {
    let mut options = rich_options();
    let routes: Vec<Model> = options
        .models
        .iter()
        .map(|model| {
            let mut route = model.clone();
            route.provider = "groq".to_string();
            route
        })
        .collect();
    options.models.extend(routes);
    options.configured_providers.insert("groq".to_string());
    options
}

/// Enter on the current model keeps it, or sets its edited effort; on another model it switches,
/// at the edited effort once the arrows moved it.
#[test]
fn the_enter_line_says_what_enter_does_to_the_session() {
    let mut picker = ModelPicker::new(rich_options());
    let mut lines = vec![enter_line(&mut picker)];
    picker.handle_key("right", &kb());
    lines.push(enter_line(&mut picker));
    picker.handle_key("down", &kb());
    lines.push(enter_line(&mut picker));
    picker.handle_key("right", &kb());
    lines.push(enter_line(&mut picker));
    assert_eq!(
        lines,
        vec![
            "  enter keeps Claude Fable 5 (current)",
            "  enter sets Claude Fable 5 to high effort",
            "  enter switches to GLM 5.3",
            "  enter switches to GLM 5.3 at high effort",
        ]
    );
}

/// A model without effort levels is not offered the effort keys.
#[test]
fn the_effort_keys_show_only_on_a_model_with_effort() {
    let mut picker = ModelPicker::new(rich_options());
    // The recent Fable and GLM rows, the anthropic Haiku row, then the local Llama row.
    for _ in 0..3 {
        picker.handle_key("down", &kb());
    }
    assert_eq!(
        picker.selected_model().map(|model| model.id.as_str()),
        Some("llama-4")
    );
    assert_eq!(
        footer(&mut picker),
        "  \u{2191}/\u{2193} model \u{b7} Enter select \u{b7} Tab actions \u{b7} Alt+O sort \u{b7} Esc close"
    );
}

/// Chips that hide every model say so, and the footer offers only what still works.
#[test]
fn a_filter_that_hides_every_model_says_so() {
    let mut picker = ModelPicker::new(rich_options());
    // Billing on `free` (any, plan, pay-as-you-go, free), capability on `reasoning`.
    for key in ["alt+2", "alt+2", "alt+2", "alt+3", "alt+3"] {
        picker.handle_key(key, &kb());
    }
    let rows = frame_at(&mut picker, 100);
    assert_eq!(
        (
            rows[5].as_str(),
            footer(&mut picker).as_str(),
            enter_line(&mut picker).as_str()
        ),
        (
            "  No model matches the filters",
            "  Alt+O sort \u{b7} Esc close",
            ""
        )
    );
}

/// A search over models two providers serve keeps each row's provider at 40 columns, where the
/// context gives way.
#[test]
fn a_narrow_search_names_each_rows_provider() {
    let mut picker = ModelPicker::new(twice_served());
    picker.set_query("claude");
    let rows = frame_at(&mut picker, 40);
    assert_eq!(
        rows[5..9],
        [
            "\u{203a} \u{25cf} Claude Fable 5     plan    anthropic",
            "    Claude Haiku 5     plan    anthropic",
            "    Claude Fable 5     $5/$25  groq",
            "    Claude Haiku 5     $1/$5   groq",
        ]
    );
}

/// A name with a joined emoji or a combining mark is cut and padded by grapheme: the effort
/// squares start in the same column on every row.
#[test]
fn a_joined_emoji_name_keeps_the_effort_column() {
    let mut options = rich_options();
    options.models[1].name =
        "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} Haiku e\u{301}dition, long enough to be cut"
            .to_string();
    options.models[2].name = "\u{901a}\u{4e49} GLM \u{1f680}".to_string();
    let mut picker = ModelPicker::new(options);
    let columns: Vec<usize> = frame_at(&mut picker, 100)
        .iter()
        .filter_map(|row| {
            let at = row.find(['\u{25a0}', '\u{25a1}'])?;
            Some(str_width(&row[..at]))
        })
        .collect();
    assert_eq!(columns.len(), 3, "the three reasoning rows: {columns:?}");
    assert!(
        columns.iter().all(|column| *column == columns[0]),
        "{columns:?}"
    );
}

/// The action menu names the arrows on the rows they step (effort, account), and only there.
#[test]
fn the_action_menu_names_the_arrows_on_its_stepping_rows() {
    let mut picker = ModelPicker::new(rich_options());
    picker.handle_key("tab", &kb());
    let switch = footer(&mut picker);
    picker.handle_key("down", &kb());
    picker.handle_key("down", &kb());
    assert_eq!(
        [switch, footer(&mut picker)],
        [
            "  \u{2191}/\u{2193} action \u{b7} Enter select \u{b7} Esc back".to_string(),
            "  \u{2191}/\u{2193} action \u{b7} \u{2190}/\u{2192} change \u{b7} Enter select \u{b7} Esc back"
                .to_string(),
        ]
    );
}

/// A click under the action menu picks no list row: Esc returns to the model the menu was for.
#[test]
fn a_click_under_the_action_menu_keeps_the_list_selection() {
    let mut picker = ModelPicker::new(rich_options());
    picker.handle_key("tab", &kb());
    picker.select_display_row(4);
    picker.handle_key("escape", &kb());
    assert_eq!(
        picker.selected_model().map(|model| model.id.as_str()),
        Some("claude-fable-5")
    );
}

/// On a short terminal the action menu gives up its spacer rows (then its subtitle), so its title
/// and footer fit the list frame's height.
#[test]
fn a_short_terminal_keeps_the_action_menus_title() {
    let mut options = rich_options();
    options.viewport_rows = 8;
    let mut picker = ModelPicker::new(options);
    picker.handle_key("tab", &kb());
    assert_eq!(
        frame_at(&mut picker, 80),
        vec![
            "  Model \u{b7} Claude Fable 5",
            "  anthropic/claude-fable-5 \u{b7} plan \u{b7} 1M context",
            "\u{203a} Switch this session to it (current)",
            "  Set as default for new sessions",
            "  Effort \u{2190} medium \u{2192}",
            "  Add to favorites",
            "  Details",
            "  \u{2191}/\u{2193} action \u{b7} Enter select \u{b7} Esc back",
            "",
        ]
    );
}

/// The recent group is where the highlighted model sits until the arrows leave it.
#[test]
fn the_selection_knows_when_it_sits_in_the_recent_group() {
    let mut picker = ModelPicker::new(rich_options());
    picker.render(&theme(), 100, &kb());
    let in_recent = picker.selected_in_recent_group();
    // GLM 5.3 (recent), then Claude Haiku 5 (anthropic).
    picker.handle_key("down", &kb());
    let still = picker.selected_in_recent_group();
    picker.handle_key("down", &kb());
    assert_eq!(
        (in_recent, still, picker.selected_in_recent_group()),
        (true, true, false)
    );
}

/// Names a terminal draws wide, joined, or combined.
fn model_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("\u{901a}\u{4e49}\u{5343}\u{95ee} \u{8d85}\u{957f}\u{6a21}\u{578b}\u{540d}\u{79f0} Turbo".to_string()),
        Just("\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} Family".to_string()),
        Just("e\u{301}dition \u{2728}\u{1f680} preview".to_string()),
        "[a-zA-Z0-9 ./-]{1,48}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// No picker row is wider than its frame, for any width from 20 to 200 and any names, in the
    /// grouped list, a flat sort, a search, and the action menu.
    #[test]
    fn picker_rows_never_exceed_the_frame(
        names in proptest::collection::vec(model_name(), 6),
        width in 20usize..=200,
        surface in 0usize..4,
    ) {
        let mut options = twice_served();
        for (model, name) in options.models.iter_mut().zip(&names) {
            model.name.clone_from(name);
        }
        let mut picker = ModelPicker::new(options);
        match surface {
            0 => {}
            1 => {
                picker.handle_key("alt+o", &kb());
            }
            2 => picker.set_query("a"),
            _ => {
                picker.handle_key("tab", &kb());
            }
        }
        for line in picker.render(&theme(), width, &kb()) {
            let row: String = line.iter().map(|span| span.content.as_str()).collect();
            prop_assert!(str_width(&row) <= width, "{} > {width}: {row:?}", str_width(&row));
        }
    }
}
