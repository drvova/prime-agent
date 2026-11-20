use super::*;
use crate::keybindings::KeybindingsManager;
use crate::theme::{ColorMode, Theme};
use pa_types::billing::BillingMode;

pub(super) fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

pub(super) fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

/// One catalog model with explicit cost and an optional thinking map.
fn model(provider: &str, id: &str, name: &str, reasoning: bool, map: Option<&str>) -> Model {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": name, "api": "openai-completions", "provider": provider,
        "baseUrl": "https://example.invalid/v1", "reasoning": reasoning,
        "thinkingLevelMap": map.map(|m| serde_json::from_str::<serde_json::Value>(m).unwrap()),
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 4096,
    }))
    .expect("mock model deserializes")
}

/// The fable map: low..max on, off/minimal off.
fn fable_map() -> &'static str {
    r#"{"off": null, "minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": "xhigh", "max": "max"}"#
}

/// A reasoning model whose only on-level is high.
fn high_only_map() -> &'static str {
    r#"{"minimal": null, "low": null, "medium": null, "high": "high", "xhigh": null, "max": null}"#
}

/// The 4.6 map: low..max on except xhigh.
fn opus_map() -> &'static str {
    r#"{"minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": null, "max": "max"}"#
}

/// A battery-shaped catalog: the mock-1 current model plus the featured Claude ladder.
fn battery_catalog() -> Vec<Model> {
    vec![
        model("prime-inference", "mock-1", "Mock 1", false, None),
        model(
            "prime-inference",
            "anthropic/claude-fable-5",
            "Claude Fable 5",
            true,
            Some(fable_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-haiku-4.5",
            "Claude Haiku 4.5",
            true,
            Some(high_only_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4.6",
            "Claude Opus 4.6",
            true,
            Some(opus_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4.7",
            "Claude Opus 4.7",
            true,
            Some(opus_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4.8",
            "Claude Opus 4.8",
            true,
            Some(opus_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-sonnet-4.5",
            "Claude Sonnet 4.5",
            true,
            Some(high_only_map()),
        ),
        model(
            "prime-inference",
            "anthropic/claude-sonnet-4.6",
            "Claude Sonnet 4.6",
            true,
            Some(opus_map()),
        ),
        model(
            "prime-inference",
            "deepseek/deepseek-v4",
            "Deepseek V4",
            false,
            None,
        ),
    ]
}

fn current() -> CurrentModel {
    CurrentModel {
        provider: "prime-inference".to_string(),
        model_id: "mock-1".to_string(),
    }
}

fn configured() -> HashSet<String> {
    ["prime-inference".to_string()].into_iter().collect()
}

fn picker_options(catalog: Vec<Model>) -> ModelPickerOptions {
    ModelPickerOptions {
        models: catalog,
        current: Some(current()),
        configured_providers: configured(),
        accounts: std::collections::BTreeMap::new(),
        account_choices: std::collections::BTreeMap::new(),
        recent_models: Vec::new(),
        scoped_models: Vec::new(),
        thinking_level: Some(ModelThinkingLevel::Medium),
        viewport_rows: 19,
        billing: HashMap::new(),
        quota_labels: HashMap::new(),
        favorite_models: Vec::new(),
        usage: PickerUsage::default(),
    }
}

/// Rendered frame rows as trimmed plain text (tmux-capture shape).
fn frame_text(picker: &mut ModelPicker) -> Vec<String> {
    picker
        .render(&theme(), 120, &kb())
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The scope view: a session with scoped models opens on the scoped list, the scope row above it;
/// Alt+S swaps to the full catalog and back, and the macOS option-composed `ß` toggles too.
#[test]
fn scope_toggles_between_the_scoped_list_and_the_catalog() {
    let catalog = battery_catalog();
    let mut options = picker_options(catalog);
    options.scoped_models = [0, 3]
        .into_iter()
        .map(|index| {
            let model = &options.models[index];
            ModelPicker::model_key_provider(&model.provider, &model.id)
        })
        .collect();
    let mut picker = ModelPicker::new(options);
    assert_eq!(
        picker.filtered_len(),
        2,
        "the picker opens on the scoped list"
    );
    let rows = frame_text(&mut picker);
    // The scope row renders ABOVE the search field with one leading space (TS
    // v0.9.7, the parity run's b3 frame).
    let scope_row = rows
        .iter()
        .position(|row| row.starts_with(" Scope: all | scoped"));
    let search_row = rows
        .iter()
        .position(|row| row.contains("Search models"))
        .expect("the search field renders");
    let scope_hint = format!(
        "{}+S scope (all/scoped)",
        crate::keybindings::format_key_text("alt")
    );
    assert!(
        scope_row.is_some_and(|scope_row| {
            scope_row < search_row && rows[scope_row].contains(&scope_hint)
        }),
        "the scope row renders above the search field with its leading space:
{}",
        rows.join("\n")
    );
    assert_eq!(
        picker.handle_key("alt+s", &kb()),
        ModelPickerAction::ScopeToggled { scoped: false },
        "alt+s swaps to the catalog side"
    );
    assert_eq!(
        picker.filtered_len(),
        9,
        "the catalog side lists everything"
    );
    assert_eq!(
        picker.handle_key("alt+s", &kb()),
        ModelPickerAction::ScopeToggled { scoped: true },
        "alt+s swaps back to the scoped side"
    );
    assert_eq!(picker.filtered_len(), 2);
    assert_eq!(
        picker.handle_key("\u{df}", &kb()),
        ModelPickerAction::ScopeToggled { scoped: false },
        "the option-composed \u{df} toggles too"
    );
    assert_eq!(picker.filtered_len(), 9);
}

/// A scope the loaded catalog cannot resolve still scopes (the scope keys off the session's list,
/// never off what the catalog resolves): the picker opens with no rows, Alt+S still works, and a
/// refresh that brings the entries fills them.
#[test]
fn a_scope_the_catalog_cannot_resolve_still_scopes() {
    let catalog = battery_catalog();
    let mut options = picker_options(catalog);
    options.scoped_models = vec!["prime-inference/mock-9".to_string()];
    let mut picker = ModelPicker::new(options);
    assert!(picker.has_scoped_models(), "the session's scope counts");
    assert_eq!(picker.filtered_len(), 0, "the scoped side has no rows yet");
    assert_eq!(
        picker.handle_key("alt+s", &kb()),
        ModelPickerAction::ScopeToggled { scoped: false },
        "alt+s still offers the catalog side"
    );
    assert_eq!(
        picker.filtered_len(),
        9,
        "the catalog side lists everything"
    );
    assert_eq!(
        picker.handle_key("alt+s", &kb()),
        ModelPickerAction::ScopeToggled { scoped: true },
        "alt+s returns to the scoped side"
    );
    let mut refreshed = battery_catalog();
    refreshed.push(model("prime-inference", "mock-9", "Mock 9", false, None));
    picker.update_state(
        None,
        refreshed,
        configured(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
        HashMap::new(),
    );
    assert_eq!(
        picker.filtered_len(),
        1,
        "the refresh resolved the key: the scoped row appears"
    );
}

/// One priced catalog model: its capabilities, context window, and $/1M rates.
#[allow(clippy::too_many_arguments)] // A fixture row: every field is a column the frame shows.
fn rich_model(
    provider: &str,
    id: &str,
    name: &str,
    map: Option<&str>,
    vision: bool,
    context_window: u64,
    input: f64,
    output: f64,
) -> Model {
    let mut model = model(provider, id, name, map.is_some(), map);
    if vision {
        model.input.push(pa_types::ai::ModelInput::Image);
    }
    model.context_window = context_window;
    model.cost.input = input.into();
    model.cost.output = output.into();
    model
}

/// A catalog across billing classes: a Claude plan (with a cached 5-hour quota), metered
/// `OpenRouter` rows, a free local model, and an unconfigured provider with no price.
pub(super) fn rich_options() -> ModelPickerOptions {
    let models = vec![
        rich_model(
            "anthropic",
            "claude-fable-5",
            "Claude Fable 5",
            Some(fable_map()),
            true,
            1_000_000,
            5.0,
            25.0,
        ),
        rich_model(
            "anthropic",
            "claude-haiku-5",
            "Claude Haiku 5",
            Some(high_only_map()),
            true,
            200_000,
            1.0,
            5.0,
        ),
        rich_model(
            "openrouter",
            "z-ai/glm-5.3",
            "GLM 5.3",
            Some(opus_map()),
            false,
            200_000,
            0.6,
            2.2,
        ),
        rich_model(
            "openrouter",
            "qwen/qwen3-coder",
            "Qwen3 Coder",
            None,
            false,
            262_144,
            0.22,
            0.95,
        ),
        rich_model(
            "local-llm",
            "llama-4",
            "Llama 4 Local",
            None,
            true,
            128_000,
            0.0,
            0.0,
        ),
        rich_model(
            "other",
            "unknown-1",
            "Unknown One",
            None,
            false,
            32_000,
            0.0,
            0.0,
        ),
    ];
    let billing = [
        (
            "anthropic/claude-fable-5",
            BillingMode::Plan {
                provider_quota: true,
            },
        ),
        (
            "anthropic/claude-haiku-5",
            BillingMode::Plan {
                provider_quota: true,
            },
        ),
        (
            "openrouter/z-ai/glm-5.3",
            BillingMode::PayAsYouGo {
                source: pa_types::billing::CostSource::ProviderReported,
            },
        ),
        (
            "openrouter/qwen/qwen3-coder",
            BillingMode::PayAsYouGo {
                source: pa_types::billing::CostSource::ProviderReported,
            },
        ),
        ("local-llm/llama-4", BillingMode::Free),
    ]
    .into_iter()
    .map(|(key, mode)| (key.to_string(), mode))
    .collect();
    ModelPickerOptions {
        models,
        current: Some(CurrentModel {
            provider: "anthropic".to_string(),
            model_id: "claude-fable-5".to_string(),
        }),
        accounts: std::collections::BTreeMap::new(),
        account_choices: std::collections::BTreeMap::new(),
        configured_providers: ["anthropic", "openrouter", "local-llm"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        recent_models: vec![
            "openrouter/z-ai/glm-5.3".to_string(),
            "anthropic/claude-fable-5".to_string(),
        ],
        scoped_models: Vec::new(),
        thinking_level: Some(ModelThinkingLevel::Medium),
        viewport_rows: 19,
        billing,
        quota_labels: [("anthropic".to_string(), "5h 42%".to_string())]
            .into_iter()
            .collect(),
        favorite_models: Vec::new(),
        usage: PickerUsage::default(),
    }
}

/// The search field's rule row at `width`.
pub(super) fn rule(width: usize) -> String {
    "\u{2500}".repeat(width)
}

/// Rendered frame rows at `width` as trimmed plain text (tmux-capture shape).
pub(super) fn frame_at(picker: &mut ModelPicker, width: usize) -> Vec<String> {
    picker
        .render(&theme(), width, &kb())
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The grouped frame at a wide terminal: the chip row with its keys, the recent group (the
/// current model first, marked, then the newest use), one group per provider (the unconfigured
/// one marked `sign in`), and each row's capability glyphs, context, price or billing class, and
/// the provider's cached quota.
#[test]
fn the_grouped_frame_at_120_columns() {
    let mut picker = ModelPicker::new(rich_options());
    assert_eq!(frame_at(&mut picker, 120), vec![
        "  Model",
        &rule(120),
        " >  Search models",
        &rule(120),
        "  Alt+O sort: recent  Alt+1 provider: all  Alt+2 billing: any  Alt+3 caps: any  Alt+4 ctx: any",
        "  recent                                                                                                               \u{2588}",
        "\u{203a} \u{25cf} Claude Fable 5                                 \u{2190} \u{25a0}\u{25a0}\u{25a1}\u{25a1}\u{25a1} \u{2192} medium                       \u{25c9}\u{25c6}\u{192}    1M  plan       5h 42% \u{2588}",
        "    GLM 5.3                                          \u{25a0}\u{25a0}\u{25a1}\u{25a1}    medium                       \u{b7}\u{25c6}\u{192}  200k  $0.6/$2.2         \u{2588}",
        "  anthropic                                                                                                            \u{2588}",
        "    Claude Haiku 5                                   \u{25a0}       high                         \u{25c9}\u{25c6}\u{192}  200k  plan       5h 42% \u{2588}",
        "  local-llm                                                                                                            \u{2502}",
        "    Llama 4 Local                                                                         \u{25c9}\u{b7}\u{192}  128k  free              \u{2502}",
        "  openrouter                                                                                                           \u{2502}",
        "",
        " Input                             Cached input                      Output                             $ / 1M tokens",
        " $5                                $0                                $25",
        "",
        "  enter switches to Claude Fable 5",
        "  \u{2191}/\u{2193} model \u{b7} \u{2190}/\u{2192} effort \u{b7} Enter select \u{b7} Tab actions \u{b7} Alt+O sort \u{b7} Esc close",
        "",
    ]);
}

/// At 80 columns the chips drop their keys (the keyed row no longer fits); every column stays.
#[test]
fn the_grouped_frame_at_80_columns() {
    let mut picker = ModelPicker::new(rich_options());
    assert_eq!(frame_at(&mut picker, 80), vec![
        "  Model",
        &rule(80),
        " >  Search models",
        &rule(80),
        "  sort: recent  provider: all  billing: any  caps: any  ctx: any",
        "  recent                                                                       \u{2588}",
        "\u{203a} \u{25cf} Claude Fable 5             \u{2190} \u{25a0}\u{25a0}\u{25a1}\u{25a1}\u{25a1} \u{2192} medium   \u{25c9}\u{25c6}\u{192}    1M  plan       5h 42% \u{2588}",
        "    GLM 5.3                      \u{25a0}\u{25a0}\u{25a1}\u{25a1}    medium   \u{b7}\u{25c6}\u{192}  200k  $0.6/$2.2         \u{2588}",
        "  anthropic                                                                    \u{2588}",
        "    Claude Haiku 5               \u{25a0}       high     \u{25c9}\u{25c6}\u{192}  200k  plan       5h 42% \u{2588}",
        "  local-llm                                                                    \u{2502}",
        "    Llama 4 Local                                 \u{25c9}\u{b7}\u{192}  128k  free              \u{2502}",
        "  openrouter                                                                   \u{2502}",
        "",
        " Input                Cached input         Output                $ / 1M tokens",
        " $5                   $0                   $25",
        "",
        "  enter switches to Claude Fable 5",
        "  \u{2191}/\u{2193} model \u{b7} \u{2190}/\u{2192} effort \u{b7} Enter select \u{b7} Tab actions \u{b7} Alt+O sort \u{b7} Esc close",
        "",
    ]);
}

/// At 60 columns the effort label and the chips' tail give way; the names truncate.
#[test]
fn the_grouped_frame_at_60_columns() {
    let mut picker = ModelPicker::new(rich_options());
    assert_eq!(frame_at(&mut picker, 60), vec![
        "  Model",
        &rule(60),
        " >  Search models",
        &rule(60),
        "  sort: recent  provider: all  billing: any  caps: any  ctx\u{2026}",
        "  recent                                                   \u{2588}",
        "\u{203a} \u{25cf} Claude Fable\u{2026} \u{2190} \u{25a0}\u{25a0}\u{25a1}\u{25a1}\u{25a1} \u{2192}   \u{25c9}\u{25c6}\u{192}    1M  plan       5h 42% \u{2588}",
        "    GLM 5.3         \u{25a0}\u{25a0}\u{25a1}\u{25a1}      \u{b7}\u{25c6}\u{192}  200k  $0.6/$2.2         \u{2588}",
        "  anthropic                                                \u{2588}",
        "    Claude Haiku\u{2026}   \u{25a0}         \u{25c9}\u{25c6}\u{192}  200k  plan       5h 42% \u{2588}",
        "  local-llm                                                \u{2502}",
        "    Llama 4 Local             \u{25c9}\u{b7}\u{192}  128k  free              \u{2502}",
        "  openrouter                                               \u{2502}",
        "",
        " Input         Cached input  Output         $ / 1M tokens",
        " $5            $0            $25",
        "",
        "  enter switches to Claude Fable 5",
        "  \u{2191}/\u{2193} model \u{b7} \u{2190}/\u{2192} effort \u{b7} Enter select \u{b7} Esc close",
        "",
    ]);
}

/// The billing chip on `plan` and the price sort: one flat list across providers with the
/// provider column, the plan models only.
#[test]
fn the_plan_filter_and_price_sort_frame() {
    let mut picker = ModelPicker::new(rich_options());
    assert_eq!(
        picker.handle_key("alt+2", &kb()),
        ModelPickerAction::None,
        "alt+2 cycles billing to plan"
    );
    for _ in 0..2 {
        picker.handle_key("alt+o", &kb());
    }
    assert_eq!(frame_at(&mut picker, 100), vec![
        "  Model",
        &rule(100),
        " >  Search models",
        &rule(100),
        "  Alt+O sort: price  Alt+1 provider: all  Alt+2 billing: plan  Alt+3 caps: any  Alt+4 ctx: any",
        "\u{203a} \u{25cf} Claude Fable 5                        \u{2190} \u{25a0}\u{25a0}\u{25a1}\u{25a1}\u{25a1} \u{2192} medium        \u{25c9}\u{25c6}\u{192}    1M  plan  5h 42%  anthropic",
        "    Claude Haiku 5                          \u{25a0}       high          \u{25c9}\u{25c6}\u{192}  200k  plan  5h 42%  anthropic",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        " Input                       Cached input                Output                       $ / 1M tokens",
        " $5                          $0                          $25",
        "",
        "  enter switches to Claude Fable 5",
        "  \u{2191}/\u{2193} model \u{b7} \u{2190}/\u{2192} effort \u{b7} Enter select \u{b7} Tab actions \u{b7} Alt+O sort \u{b7} Esc close",
        "",
    ]);
}

/// The pay-as-you-go chip with the context sort and the vision chip: the chips compose.
#[test]
fn the_chips_compose_with_the_context_sort() {
    let mut picker = ModelPicker::new(rich_options());
    for key in ["alt+2", "alt+2", "alt+o", "alt+o", "alt+o"] {
        picker.handle_key(key, &kb());
    }
    let ids = |picker: &ModelPicker| filtered_ids(picker);
    assert_eq!(
        ids(&picker),
        vec!["qwen/qwen3-coder", "z-ai/glm-5.3"],
        "pay-as-you-go by context, widest first"
    );
    picker.handle_key("alt+3", &kb());
    assert_eq!(
        ids(&picker),
        Vec::<String>::new(),
        "no metered model reads images"
    );
    assert_eq!(
        picker.summary(ModelPickerOutcome::Closed),
        ModelPickerSummary {
            outcome: ModelPickerOutcome::Closed,
            chip_change_count: 3,
            sort_change_count: 3,
            filters_active: true,
            sort: "context",
            action_menu_count: 0,
            favorite_toggle_count: 0,
            default_set_count: 0,
            effort_set_count: 0,
            details_count: 0,
            account_set_count: 0,
        }
    );
}

/// The provider chip steps through the active list's providers in display order, then back to
/// every provider; the minimum-context chip keeps the wide windows.
#[test]
fn the_provider_and_context_chips_cycle() {
    let mut picker = ModelPicker::new(rich_options());
    let mut seen = Vec::new();
    for _ in 0..5 {
        picker.handle_key("alt+1", &kb());
        seen.push(picker.filters().provider.clone());
    }
    assert_eq!(
        seen,
        vec![
            Some("anthropic".to_string()),
            Some("openrouter".to_string()),
            Some("local-llm".to_string()),
            Some("other".to_string()),
            None,
        ]
    );
    for _ in 0..2 {
        picker.handle_key("alt+4", &kb());
    }
    assert_eq!(
        filtered_ids(&picker),
        vec![
            "claude-fable-5",
            "z-ai/glm-5.3",
            "claude-haiku-5",
            "qwen/qwen3-coder"
        ],
        "at least 200k: the recent group, then the provider groups"
    );
}

/// A provider holding several credentials names the session's account in its group header and,
/// in a flat sort, in its provider column.
#[test]
fn a_multi_credential_provider_names_its_account() {
    let mut options = rich_options();
    options.accounts = [("anthropic".to_string(), "work".to_string())]
        .into_iter()
        .collect();
    let mut picker = ModelPicker::new(options);
    let grouped = frame_at(&mut picker, 120);
    picker.handle_key("alt+o", &kb());
    let flat = frame_at(&mut picker, 120);
    let haiku = flat
        .iter()
        .find(|row| row.contains("Claude Haiku 5"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            grouped
                .iter()
                .any(|row| row == "  anthropic \u{b7} account: work"),
            haiku.ends_with("anthropic \u{b7} work"),
        ),
        (true, true),
        "{grouped:#?}\n{haiku}"
    );
}

/// A click on a group header selects nothing; a click on a model row selects it.
#[test]
fn a_header_click_keeps_the_selection_and_a_row_click_moves_it() {
    let mut picker = ModelPicker::new(rich_options());
    picker.render(&theme(), 120, &kb());
    let before = picker.selected_index();
    picker.select_display_row(0);
    assert_eq!(
        picker.selected_index(),
        before,
        "row 0 is the recent header"
    );
    picker.select_display_row(4);
    assert_eq!(
        picker.selected_model().map(|model| model.id.as_str()),
        Some("claude-haiku-5"),
        "row 4 is the anthropic group's first model"
    );
}

/// Pinning the paint path: the picker paints from the options it was opened with and nothing it
/// awaits, so a catalog or quota source that never answers cannot hold the first frame.
#[test]
fn the_picker_paints_without_a_catalog_or_quota_answer() {
    let mut options = rich_options();
    options.billing.clear();
    options.quota_labels.clear();
    let mut picker = ModelPicker::new(options);
    let rows = frame_at(&mut picker, 120);
    let fable = rows
        .iter()
        .find(|row| row.contains("Claude Fable 5"))
        .expect("the fable row");
    assert!(
        fable.contains("$5/$25"),
        "without a billing answer the row falls back to the catalog rates: {fable}"
    );
}

#[test]
fn the_current_model_leads_and_matches_only_by_provider_and_id() {
    let picker = ModelPicker::new(picker_options(battery_catalog()));
    let model = picker.selected_model().expect("selection");
    assert_eq!(model.id, "mock-1");
    assert_eq!(picker.selected_index(), 0);
}

#[test]
fn enter_applies_the_selection() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "mock-1".to_string(),
            effort: None,
        }))
    );
}

#[test]
fn typed_filter_selects_the_match_and_enter_applies_it() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.set_query("haiku");
    assert_eq!(picker.query(), "haiku");
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "anthropic/claude-haiku-4.5".to_string(),
            effort: None,
        }))
    );
}

/// The Tab-intercepted partial keeps the caret at its end, so typing extends the
/// filter instead of inserting before it.
#[test]
fn a_row_click_lands_the_arrows_on_the_clicked_rows_effort() {
    let kb = kb();
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    // A nonempty search keeps Left/Right on the search field until the user
    // enters the list (an arrow move) — a row click is the same entry.
    picker.set_query("fable");
    assert!(
        picker.search.value().contains("fable"),
        "the query prefilled"
    );
    picker.select_display_row(0);
    let before = picker.search.value().to_string();
    picker.handle_key("left", &kb);
    assert_eq!(
        picker.search.value(),
        before,
        "Left adjusted the clicked row's effort, not the search"
    );
}

#[test]
fn set_query_prefill_leaves_the_caret_at_the_end() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.set_query("gp");
    assert_eq!(picker.search.cursor(), 2, "the caret sits after gp");
    picker.handle_key("t", &kb());
    assert_eq!(picker.query(), "gpt");
    assert_eq!(picker.search.cursor(), 3);
}

#[test]
fn typing_into_the_picker_filters_and_resets_the_selection() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    for character in "mock".chars() {
        assert_eq!(
            picker.handle_key(&character.to_string(), &kb()),
            ModelPickerAction::None
        );
    }
    assert_eq!(picker.query(), "mock");
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "mock-1".to_string(),
            effort: None,
        }))
    );
}

#[test]
fn escape_and_ctrl_c_cancel() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    assert_eq!(
        picker.handle_key("escape", &kb()),
        ModelPickerAction::Cancel
    );
    assert_eq!(
        picker.handle_key("ctrl+c", &kb()),
        ModelPickerAction::Cancel
    );
}

#[test]
fn navigation_wraps_and_enter_applies_the_moved_selection() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    assert_eq!(picker.handle_key("up", &kb()), ModelPickerAction::None);
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "deepseek/deepseek-v4".to_string(),
            effort: None,
        }))
    );
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.handle_key("down", &kb());
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "anthropic/claude-fable-5".to_string(),
            effort: None,
        }))
    );
}

#[test]
fn left_right_adjust_the_selected_effort_and_enter_carries_it() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    // Move onto a reasoning model; its seeded effort is medium.
    picker.handle_key("down", &kb());
    // The empty filter keeps arrows on the effort cluster.
    assert_eq!(picker.handle_key("left", &kb()), ModelPickerAction::None);
    let model = picker.selected_model().expect("selection").clone();
    assert_eq!(picker.effort_of(&model), Some(ModelThinkingLevel::Low));
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "anthropic/claude-fable-5".to_string(),
            effort: Some("low".to_string()),
        }))
    );
}

#[test]
fn a_nonempty_filter_keeps_arrows_on_the_search_cursor() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    for character in "claude fable".chars() {
        picker.handle_key(&character.to_string(), &kb());
    }
    // The caret sits at the field's end: left moves the caret inside the text,
    // not the effort cluster or the picker.
    assert_eq!(picker.handle_key("left", &kb()), ModelPickerAction::None);
    assert_eq!(picker.query(), "claude fable");
    // Home walks the caret to the field's start; a further left acts like Esc (back
    // only at column 0).
    assert_eq!(picker.handle_key("home", &kb()), ModelPickerAction::None);
    assert_eq!(picker.handle_key("left", &kb()), ModelPickerAction::Cancel);
}

#[test]
fn unconfigured_providers_head_a_sign_in_group_and_sort_last() {
    let catalog = vec![
        model("prime-inference", "mock-1", "Mock 1", false, None),
        model(
            "other",
            "unconfigured-model",
            "Unconfigured Model",
            false,
            None,
        ),
    ];
    let mut options = picker_options(catalog);
    options.current = None;
    let mut picker = ModelPicker::new(options);
    let rows = frame_text(&mut picker);
    let headers: Vec<&str> = rows
        .iter()
        .map(String::as_str)
        .filter(|row| *row == "  prime-inference" || *row == "  other \u{b7} sign in")
        .collect();
    assert_eq!(headers, vec!["  prime-inference", "  other \u{b7} sign in"]);
}

#[test]
fn update_state_keeps_the_selection_on_the_surviving_model() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.handle_key("down", &kb());
    let mut catalog = battery_catalog();
    // The refresh drops one model and adds another.
    catalog.pop();
    catalog.push(model(
        "prime-inference",
        "new/model",
        "New Model",
        false,
        None,
    ));
    picker.update_state(
        Some(current()),
        catalog,
        configured(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
        HashMap::new(),
    );
    let selected = picker.selected_model().expect("selection").clone();
    assert_eq!(selected.id, "anthropic/claude-fable-5");
}

#[test]
fn the_effort_seed_clamps_to_the_model_levels() {
    let catalog = vec![
        model("prime-inference", "mock-1", "Mock 1", false, None),
        model(
            "prime-inference",
            "anthropic/claude-haiku-4.5",
            "Claude Haiku 4.5",
            true,
            Some(high_only_map()),
        ),
    ];
    let picker = ModelPicker::new(picker_options(catalog));
    let haiku = picker.model_at(1).expect("catalog").clone();
    // Medium clamps up to the only supported level.
    assert_eq!(picker.effort_of(&haiku), Some(ModelThinkingLevel::High));
}

#[test]
fn paging_moves_by_the_visible_window() {
    let mut catalog = battery_catalog();
    for extra in 0..20 {
        catalog.push(model(
            "prime-inference",
            &format!("extra/{extra}"),
            "Extra",
            false,
            None,
        ));
    }
    let mut options = picker_options(catalog);
    options.current = None;
    let mut picker = ModelPicker::new(options);
    assert_eq!(
        picker.handle_key("pageDown", &kb()),
        ModelPickerAction::None
    );
    assert_eq!(picker.selected_index(), picker.visible_items());
}

#[test]
fn the_sorted_order_matches_the_ts_chain() {
    // The full sort chain: configured, pinned, current, recent, provider, featured, id.
    let catalog = vec![
        model("other", "b-model", "B", false, None),
        model("prime-inference", "z-model-2", "Z2", false, None),
        model("prime-inference", "z-model-10", "Z10", false, None),
        model("prime-inference", "mock-1", "Mock 1", false, None),
    ];
    let mut options = picker_options(catalog);
    options.current = Some(current());
    let picker = ModelPicker::new(options);
    let ids: Vec<&str> = picker
        .all_models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(ids, vec!["mock-1", "z-model-2", "z-model-10", "b-model",]);
}

#[test]
fn an_empty_catalog_opens_the_empty_panel() {
    // TS `handleModelCommand` opens the menu regardless: an empty catalog
    // renders the bordered field and the no-match row.
    let ModelCommandOutcome::Open(mut picker) = ModelPicker::open(picker_options(Vec::new()), "");
    let rows = frame_text(&mut picker);
    assert_eq!(rows[2], " >  Search models");
    assert!(rows.iter().any(|row| row == "  No models available"));
}

#[test]
fn dispatch_opens_the_picker_with_the_prefilled_search() {
    let ModelCommandOutcome::Open(picker) =
        ModelPicker::open(picker_options(battery_catalog()), " haiku ");
    assert_eq!(picker.query(), "haiku");
}

#[test]
fn a_query_with_no_matches_renders_the_no_matching_row() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.set_query("zzz-no-match");
    let rows = frame_text(&mut picker);
    assert!(rows
        .iter()
        .any(|row| row == "  no match for \u{201c}zzz-no-match\u{201d} \u{b7} esc clears"));
}

#[test]
fn paste_edits_the_filter_like_the_ts_input() {
    let mut picker = ModelPicker::new(picker_options(battery_catalog()));
    picker.paste("mock-1");
    assert_eq!(picker.query(), "mock-1");
    assert_eq!(
        picker.handle_key("enter", &kb()),
        ModelPickerAction::Apply(Box::new(ModelSelectionApplied {
            provider: "prime-inference".to_string(),
            model_id: "mock-1".to_string(),
            effort: None,
        }))
    );
}

/// The ids of the filtered (search-ordered) rows.
fn filtered_ids(picker: &ModelPicker) -> Vec<String> {
    picker
        .filtered
        .iter()
        .map(|&index| picker.all_models[index].id.clone())
        .collect()
}

#[test]
fn version_key_parses_the_catalog_id_formats() {
    // Hyphen- and dot-joined releases, letter-glued versions, dated snapshots,
    // namespaced ids, and digit-free ids.
    assert_eq!(version_key("claude-opus-5-5"), vec!["5", "5"]);
    assert_eq!(version_key("claude-opus-5.5"), vec!["5", "5"]);
    assert_eq!(version_key("glm-5.3"), vec!["5", "3"]);
    assert_eq!(version_key("glm-5p2"), vec!["5", "2"]);
    assert_eq!(version_key("qwen3.5-plus"), vec!["3", "5"]);
    assert_eq!(version_key("deepseek-v4"), vec!["4"]);
    assert_eq!(version_key("minimax-m2.7"), vec!["2", "7"]);
    assert_eq!(version_key("mimo-v2.5"), vec!["2", "5"]);
    assert_eq!(version_key("kimi-k2.7-code"), vec!["2", "7"]);
    assert_eq!(version_key("o3-mini"), vec!["3"]);
    assert_eq!(version_key("x-ai/grok-4.20"), vec!["4", "20"]);
    assert_eq!(
        version_key("gpt-4o-2024-05-13"),
        vec!["4", "2024", "05", "13"]
    );
    assert_eq!(
        version_key("claude-opus-4-5-20251101"),
        vec!["4", "5", "20251101"]
    );
    assert_eq!(version_key("anthropic/claude-opus-4.7"), vec!["4", "7"]);
    assert_eq!(
        version_key("us.anthropic.claude-opus-4-6-v1:0"),
        vec!["4", "6", "1", "0"]
    );
    assert_eq!(version_key("claude-opus-latest"), Vec::<String>::new());
    assert_eq!(version_key("auto-beta"), Vec::<String>::new());
    // Overlong digit runs (beyond `u64`) are preserved, not dropped.
    assert_eq!(
        version_key("model-18446744073709551616"),
        vec!["18446744073709551616"]
    );
}

#[test]
fn version_desc_orders_runs_newest_first() {
    // Higher versions first; digit value, not text.
    assert_eq!(
        version_desc(&version_key("model-5.5"), &version_key("model-4.7")),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        version_desc(&version_key("model-2"), &version_key("model-10")),
        std::cmp::Ordering::Greater
    );
    // The dated snapshot of a release leads its undated alias.
    assert_eq!(
        version_desc(
            &version_key("model-4-5-20251101"),
            &version_key("model-4-5")
        ),
        std::cmp::Ordering::Less
    );
    // Ids without a version trail every versioned match.
    assert_eq!(
        version_desc(&version_key("model-latest"), &version_key("model-3-8")),
        std::cmp::Ordering::Greater
    );
    // Leading zeros compare by value.
    assert_eq!(
        version_desc(&version_key("model-05"), &version_key("model-5")),
        std::cmp::Ordering::Equal
    );
    // Digit runs beyond `u64` keep their numeric order.
    assert_eq!(
        version_desc(
            &version_key("model-18446744073709551616"),
            &version_key("model-2")
        ),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        version_desc(&version_key("model-4-7"), &version_key("model-4-7")),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn search_ranks_version_descending() {
    // Searching `opus` lists 5.5 before 4.7; both id spellings of 5.5 tie on
    // their version run, and the dated 4.5 snapshot stays below the newer 4.7.
    let catalog = vec![
        model(
            "prime-inference",
            "anthropic/claude-opus-4.7",
            "Claude Opus 4.7",
            false,
            None,
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-5.5",
            "Claude Opus 5.5",
            false,
            None,
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-5-5",
            "Claude Opus 5 5",
            false,
            None,
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4-5-20251101",
            "Claude Opus 4.5 Snapshot",
            false,
            None,
        ),
    ];
    let mut picker = ModelPicker::new(picker_options(catalog));
    picker.set_query("opus");
    assert_eq!(
        filtered_ids(&picker),
        vec![
            "anthropic/claude-opus-5-5",
            "anthropic/claude-opus-5.5",
            "anthropic/claude-opus-4.7",
            "anthropic/claude-opus-4-5-20251101",
        ]
    );
}

#[test]
fn search_keeps_logged_in_providers_above_newer_matches() {
    // The logged-in tier outranks text match and version: the configured provider's older model
    // leads the unconfigured provider's newer, better-scoring match.
    let catalog = vec![
        model(
            "prime-inference",
            "anthropic/claude-opus-4.7",
            "Claude Opus 4.7",
            false,
            None,
        ),
        model("other", "opus-9.9", "Opus 9.9", false, None),
    ];
    let mut options = picker_options(catalog);
    options.current = None;
    let mut picker = ModelPicker::new(options);
    picker.set_query("opus");
    assert_eq!(
        filtered_ids(&picker),
        vec!["anthropic/claude-opus-4.7", "opus-9.9"]
    );
}

#[test]
fn search_ranks_version_above_the_current_model() {
    // The version tier outranks the current-model marker: searching `opus` lists
    // 4.8 first even when 4.7 is the session's model.
    let catalog = vec![
        model(
            "prime-inference",
            "anthropic/claude-opus-4.7",
            "Claude Opus 4.7",
            false,
            None,
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4.8",
            "Claude Opus 4.8",
            false,
            None,
        ),
    ];
    let mut options = picker_options(catalog);
    options.current = Some(CurrentModel {
        provider: "prime-inference".to_string(),
        model_id: "anthropic/claude-opus-4.7".to_string(),
    });
    let mut picker = ModelPicker::new(options);
    picker.set_query("opus");
    assert_eq!(
        filtered_ids(&picker),
        vec!["anthropic/claude-opus-4.8", "anthropic/claude-opus-4.7"]
    );
}

#[test]
fn search_keeps_recent_use_within_an_equal_version() {
    // Equal version runs keep the sub-tier tiebreakers: the recent-use rank leads
    // `4.7` over the id-sorted-first `4-7` spelling.
    let catalog = vec![
        model(
            "prime-inference",
            "anthropic/claude-opus-4-7",
            "Claude Opus 4 7",
            false,
            None,
        ),
        model(
            "prime-inference",
            "anthropic/claude-opus-4.7",
            "Claude Opus 4.7",
            false,
            None,
        ),
    ];
    let mut options = picker_options(catalog);
    options.current = None;
    options.recent_models = vec!["prime-inference/anthropic/claude-opus-4.7".to_string()];
    let mut picker = ModelPicker::new(options);
    picker.set_query("opus");
    assert_eq!(
        filtered_ids(&picker),
        vec!["anthropic/claude-opus-4.7", "anthropic/claude-opus-4-7"]
    );
}

#[test]
fn search_compares_version_numbers_and_leaves_unversioned_ids_last() {
    // Digit runs compare by value (10 over 2) and ids without a version trail
    // every versioned match.
    let catalog = vec![
        model("prime-inference", "z-model-2", "Z Model 2", false, None),
        model("prime-inference", "z-model-10", "Z Model 10", false, None),
        model(
            "prime-inference",
            "z-model-next",
            "Z Model Next",
            false,
            None,
        ),
    ];
    let mut picker = ModelPicker::new(picker_options(catalog));
    picker.set_query("model");
    assert_eq!(
        filtered_ids(&picker),
        vec!["z-model-10", "z-model-2", "z-model-next"]
    );
}

#[test]
fn search_ranks_preview_suffixed_ids_by_their_version() {
    // A prerelease suffix never hides the version: `hy4-preview` outranks `hy3`.
    let catalog = vec![
        model("prime-inference", "tencent/hy3", "HY3", false, None),
        model(
            "prime-inference",
            "tencent/hy4-preview",
            "HY4 Preview",
            false,
            None,
        ),
    ];
    let mut picker = ModelPicker::new(picker_options(catalog));
    picker.set_query("hy");
    assert_eq!(
        filtered_ids(&picker),
        vec!["tencent/hy4-preview", "tencent/hy3"]
    );
}

#[test]
fn search_preserves_overlong_version_runs() {
    // A digit run beyond `u64` is a version like any other: the oversized newer
    // release leads, not trails, when text scores tie.
    let catalog = vec![
        model("prime-inference", "model-2", "Model Two", false, None),
        model(
            "prime-inference",
            "model-18446744073709551616",
            "Model Oversized",
            false,
            None,
        ),
    ];
    let mut picker = ModelPicker::new(picker_options(catalog));
    picker.set_query("model");
    assert_eq!(
        filtered_ids(&picker),
        vec!["model-18446744073709551616", "model-2"]
    );
}

/// Prime Inference routes group under their serving class (unlabeled ones keep the provider
/// id), and the non-Prime row renders byte-identical: the columns size against the window.
#[test]
fn prime_rows_name_the_serving_class_without_moving_other_rows() {
    let catalog = |served: bool| {
        let pi = |id: &str, name: &str, serving: ModelServing| {
            let mut model = model("prime-inference", id, name, true, Some(opus_map()));
            model.serving = served.then_some(serving);
            model
        };
        vec![
            pi("z-ai/glm-5.3", "GLM 5.3", ModelServing::Hosted),
            pi(
                "anthropic/claude-fable-5",
                "Claude Fable 5",
                ModelServing::Gateway,
            ),
            model("openrouter", "x/other", "Other", true, Some(opus_map())),
        ]
    };
    let rows = |served: bool| {
        let mut options = picker_options(catalog(served));
        options
            .configured_providers
            .insert("openrouter".to_string());
        frame_text(&mut ModelPicker::new(options))
    };
    let row = |rows: &[String], name: &str| {
        rows.iter()
            .find(|row| row.contains(name))
            .cloned()
            .expect(name)
    };
    let headers = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .filter(|row| row.starts_with("  prime") || *row == "  openrouter")
            .cloned()
            .collect()
    };
    let (served, unserved) = (rows(true), rows(false));
    assert_eq!(
        headers(&served),
        vec!["  prime-gateway", "  prime-hosted", "  openrouter"]
    );
    assert_eq!(
        headers(&unserved),
        vec!["  prime-inference", "  openrouter"]
    );
    assert_eq!(row(&served, "Other"), row(&unserved, "Other"));

    // Search matches the displayed serving label.
    let mut picker = ModelPicker::new(picker_options(catalog(true)));
    picker.set_query("gateway");
    assert_eq!(filtered_ids(&picker), vec!["anthropic/claude-fable-5"]);
    picker.set_query("hosted");
    assert_eq!(filtered_ids(&picker), vec!["z-ai/glm-5.3"]);
}
