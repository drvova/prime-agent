use pa_types::provider_usage::{
    ModelSpend, ProviderQuota, QuotaSource, QuotaState, QuotaUnit, QuotaWindow, SpendTotals,
};

use super::super::tests::{frame_at, kb, rich_options, rule};
use super::*;

const NOW_MS: i64 = 1_800_000_000_000;
const HOUR_MS: i64 = 3_600_000;

fn spend(cost: f64, requests: u64) -> SpendTotals {
    SpendTotals {
        cost,
        requests,
        ..SpendTotals::default()
    }
}

/// The rich catalog with the cached usage: Fable's recorded spend and the anthropic card's
/// 5-hour and weekly windows.
fn usage_options() -> super::super::ModelPickerOptions {
    let mut options = rich_options();
    options.usage = PickerUsage {
        spend: vec![ModelSpend {
            provider: "anthropic".to_string(),
            model: "claude-fable-5".to_string(),
            mode: None,
            today: spend(1.234, 3),
            last_7_days: spend(3.4, 12),
            last_30_days: spend(4.5, 40),
            all_time: spend(4.5, 40),
        }],
        quotas: vec![ProviderQuota {
            provider: "anthropic".to_string(),
            plan: Some("max".to_string()),
            source: QuotaSource::Provider,
            fetched_at_ms: NOW_MS,
            state: QuotaState::Windows {
                windows: vec![
                    QuotaWindow {
                        label: "5-hour".to_string(),
                        used: 42.0,
                        limit: Some(100.0),
                        unit: QuotaUnit::Percent,
                        resets_at_ms: Some(NOW_MS + 2 * HOUR_MS + 13 * 60_000),
                    },
                    QuotaWindow {
                        label: "Weekly".to_string(),
                        used: 10.0,
                        limit: Some(100.0),
                        unit: QuotaUnit::Percent,
                        resets_at_ms: Some(NOW_MS + 96 * HOUR_MS),
                    },
                ],
            },
        }],
        now_ms: NOW_MS,
    };
    options
}

/// Press `keys` in order; the last key's action.
fn press(picker: &mut ModelPicker, keys: &[&str]) -> ModelPickerAction {
    let mut action = ModelPickerAction::None;
    for key in keys {
        action = picker.handle_key(key, &kb());
    }
    action
}

/// Tab opens the highlighted model's action menu; every row shows, effort with its level, and the
/// hint steps back with Esc. At 40 columns the subtitle truncates.
#[test]
fn the_action_menu_frame_at_100_and_40_columns() {
    let mut picker = ModelPicker::new(usage_options());
    press(&mut picker, &["tab"]);
    assert_eq!(
        (frame_at(&mut picker, 100), frame_at(&mut picker, 40)),
        (
            vec![
                "  Model \u{b7} Claude Fable 5".to_string(),
                "  anthropic/claude-fable-5 \u{b7} plan \u{b7} 1M context".to_string(),
                "".to_string(),
                "\u{203a} Switch this session to it (current)".to_string(),
                "  Set as default for new sessions".to_string(),
                "  Effort \u{2190} medium \u{2192}".to_string(),
                "  Add to favorites".to_string(),
                "  Details".to_string(),
                "".to_string(),
                "  \u{2191}/\u{2193} action \u{b7} Enter select \u{b7} Esc back".to_string(),
                "".to_string(),
            ],
            vec![
                "  Model \u{b7} Claude Fable 5".to_string(),
                "  anthropic/claude-fable-5 \u{b7} plan \u{b7} 1M \u{2026}".to_string(),
                "".to_string(),
                "\u{203a} Switch this session to it (current)".to_string(),
                "  Set as default for new sessions".to_string(),
                "  Effort \u{2190} medium \u{2192}".to_string(),
                "  Add to favorites".to_string(),
                "  Details".to_string(),
                "".to_string(),
                "  \u{2191}/\u{2193} action \u{b7} Enter select \u{b7} Esc back".to_string(),
                "".to_string(),
            ]
        )
    );
}

/// The details view: the full metadata, the cached spend for the model, and each quota window
/// with its reset.
#[test]
fn the_details_view_frame() {
    let mut picker = ModelPicker::new(usage_options());
    press(&mut picker, &["tab", "up", "enter"]);
    assert_eq!(
        frame_at(&mut picker, 120),
        vec![
            "  Model \u{b7} Claude Fable 5".to_string(),
            "  anthropic/claude-fable-5 \u{b7} plan \u{b7} 1M context".to_string(),
            "".to_string(),
            "  api       openai-completions".to_string(),
            "  context   1M \u{b7} max output 4k".to_string(),
            "  input     text, image".to_string(),
            "  effort    low, medium, high, xhigh, max".to_string(),
            "  price     $5 in \u{b7} $25 out \u{b7} $0 cache read \u{b7} $0 cache write per 1M"
                .to_string(),
            "  billing   plan".to_string(),
            "  spend     today $1.23 \u{b7} 7 days $3.40 \u{b7} all time $4.50 (40 requests)"
                .to_string(),
            "  quota     5h 42% \u{b7} resets in 2h 13m; 7d 10% \u{b7} resets in 4d".to_string(),
            "".to_string(),
            "  Esc back".to_string(),
            "".to_string(),
        ]
    );
}

/// Without a `/usage` load the details view says where the numbers come from instead of fetching.
#[test]
fn the_details_view_never_fetches_spend_or_quota() {
    let mut picker = ModelPicker::new(rich_options());
    press(&mut picker, &["down", "tab", "up", "enter"]);
    let rows = frame_at(&mut picker, 120);
    let field = |name: &str| {
        rows.iter()
            .find(|row| row.trim_start().starts_with(name))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(
        (field("spend"), field("quota")),
        (
            "  spend     not loaded (open /usage)".to_string(),
            "  quota     not loaded (open /usage)".to_string()
        )
    );
}

/// Each row's Enter: switch through the explicit apply, the default for new sessions, effort on
/// the current model (a level for the session), effort on another model (a switch at that level),
/// and Esc back to the list.
#[test]
fn the_action_rows_route_their_keys() {
    let applied = |provider: &str, model_id: &str, effort: Option<&str>| {
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            effort: effort.map(str::to_string),
        }))
    };
    let run = |keys: &[&str]| press(&mut ModelPicker::new(rich_options()), keys);
    assert_eq!(
        vec![
            run(&["tab", "enter"]),
            run(&["tab", "down", "enter"]),
            run(&["tab", "down", "down", "right", "enter"]),
            run(&["down", "tab", "down", "down", "left", "enter"]),
            run(&["tab", "escape", "enter"]),
        ],
        vec![
            applied("anthropic", "claude-fable-5", None),
            ModelPickerAction::SetDefault {
                provider: "anthropic".to_string(),
                model_id: "claude-fable-5".to_string(),
            },
            ModelPickerAction::SetEffort("high".to_string()),
            applied("openrouter", "z-ai/glm-5.3", Some("low")),
            applied("anthropic", "claude-fable-5", None),
        ]
    );
}

/// A model without a thinking surface offers no effort row; Esc from the details view returns to
/// the action menu, and a second Esc to the list.
#[test]
fn a_model_without_effort_skips_the_row_and_esc_steps_back() {
    let mut picker = ModelPicker::new(rich_options());
    // The grouped list: Fable, GLM (recent), Haiku, Llama.
    press(&mut picker, &["down", "down", "down", "tab"]);
    let PickerView::Actions { model, .. } = picker.view() else {
        panic!("the action menu is up");
    };
    let entry = picker.model_at(model).cloned().expect("the menu's model");
    assert_eq!(
        (
            entry.id.as_str(),
            ModelPicker::action_rows(&entry),
            press(&mut picker, &["up", "enter"]),
            picker.view(),
        ),
        (
            "llama-4",
            vec![
                ModelAction::Switch,
                ModelAction::SetDefault,
                ModelAction::Favorite,
                ModelAction::Details,
            ],
            ModelPickerAction::None,
            PickerView::Details { model },
        )
    );
    press(&mut picker, &["escape"]);
    assert_eq!(picker.view(), PickerView::Actions { model, selected: 0 });
    press(&mut picker, &["escape"]);
    assert_eq!(picker.view(), PickerView::List);
}

/// Favoriting a model reports the whole list, moves the model into the favorites group at the
/// top of the grouped frame with its star, and a second toggle removes it.
#[test]
fn favoriting_moves_a_model_to_the_top_group() {
    let mut picker = ModelPicker::new(rich_options());
    // Haiku is the anthropic group's first row; the favorite row is the fourth.
    let toggled = press(
        &mut picker,
        &["down", "down", "tab", "down", "down", "down", "enter"],
    );
    assert_eq!(
        toggled,
        ModelPickerAction::FavoritesChanged(vec!["anthropic/claude-haiku-5".to_string()])
    );
    press(&mut picker, &["escape"]);
    assert_eq!(frame_at(&mut picker, 120), vec![
        "  Model".to_string(),
        rule(120),
        " >  Search models".to_string(),
        rule(120),
        "  Alt+O sort: recent  Alt+1 provider: all  Alt+2 billing: any  Alt+3 caps: any  Alt+4 ctx: any".to_string(),
        "  favorites                                                                                                            \u{2588}".to_string(),
        "\u{203a} \u{2605} Claude Haiku 5                                 \u{2190} \u{25a0}     \u{2192} high                         \u{25c9}\u{25c6}\u{192}  200k  plan       5h 42% \u{2588}".to_string(),
        "  recent                                                                                                               \u{2588}".to_string(),
        "  \u{25cf} Claude Fable 5                                   \u{25a0}\u{25a0}\u{25a1}\u{25a1}\u{25a1}   medium                       \u{25c9}\u{25c6}\u{192}    1M  plan       5h 42% \u{2588}".to_string(),
        "    GLM 5.3                                          \u{25a0}\u{25a0}\u{25a1}\u{25a1}    medium                       \u{b7}\u{25c6}\u{192}  200k  $0.6/$2.2         \u{2588}".to_string(),
        "  local-llm                                                                                                            \u{2502}".to_string(),
        "    Llama 4 Local                                                                         \u{25c9}\u{b7}\u{192}  128k  free              \u{2502}".to_string(),
        "  openrouter                                                                                                           \u{2502}".to_string(),
        "".to_string(),
        " Input                             Cached input                      Output                             $ / 1M tokens".to_string(),
        " $1                                $0                                $5".to_string(),
        "".to_string(),
        "  enter switches to Claude Haiku 5".to_string(),
        "  \u{2191}/\u{2193} model \u{b7} \u{2190}/\u{2192} effort \u{b7} Enter select \u{b7} Tab actions \u{b7} Alt+O sort \u{b7} Esc close".to_string(),
        "".to_string(),
    ]);
    let removed = press(&mut picker, &["tab", "down", "down", "down", "enter"]);
    assert_eq!(
        (
            removed,
            picker.summary(super::super::ModelPickerOutcome::Closed)
        ),
        (
            ModelPickerAction::FavoritesChanged(Vec::new()),
            super::super::ModelPickerSummary {
                outcome: super::super::ModelPickerOutcome::Closed,
                chip_change_count: 0,
                sort_change_count: 0,
                filters_active: false,
                sort: "recent",
                action_menu_count: 2,
                favorite_toggle_count: 2,
                default_set_count: 0,
                effort_set_count: 0,
                details_count: 0,
            }
        )
    );
}
