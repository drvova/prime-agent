//! The `/usage` page's content rows: the provider quota cards, the spend summary and spark, and
//! the details view. Every row fits its width: a quota row drops, in order, the reset's local
//! clock, then its bar, then the `resets` wording, and only then truncates the label.

use std::fmt::Write as _;

use pa_types::control::AdjustReason;
use pa_types::platform::LocalCivil;
use pa_types::provider_usage::{
    ProviderQuota, QuotaReport, QuotaSource, QuotaState, QuotaUnit, QuotaWindow, SpendHistory,
    SpendTotals,
};

use super::format::{countdown, money, reset_label, spark, tokens, ResetDetail};
use super::Section;
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

const CARD_INDENT: &str = "  ";
const ROW_INDENT: &str = "    ";
const MIN_BAR: usize = 8;
const MAX_BAR: usize = 24;
const MAX_LABEL: usize = 18;

/// The local-time lookup the rows format resets with (the platform's zone in production).
pub(crate) type CivilClock<'a> = &'a dyn Fn(i64) -> Option<LocalCivil>;

/// The display name of a provider id.
fn provider_name(provider: &str) -> &str {
    match provider {
        "anthropic" => "Anthropic",
        "openai-codex" => "ChatGPT (Codex)",
        "github-copilot" => "GitHub Copilot",
        "prime-inference" => "Prime Inference",
        "openai" => "OpenAI",
        "google" => "Google",
        "xai" => "xAI",
        other => other,
    }
}

/// How full a window is, which tones its dot and bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Pressure {
    Ok,
    Warning,
    Exhausted,
}

impl Pressure {
    fn of(window: &QuotaWindow) -> Self {
        match window.used_fraction() {
            Some(fraction) if fraction >= 1.0 => Pressure::Exhausted,
            Some(fraction) if fraction >= 0.9 => Pressure::Warning,
            Some(_) | None => Pressure::Ok,
        }
    }

    fn color(self) -> ThemeColor {
        match self {
            Pressure::Ok => ThemeColor::Success,
            Pressure::Warning => ThemeColor::Warning,
            Pressure::Exhausted => ThemeColor::Error,
        }
    }
}

/// `37% used` (a percent window), `88 / 300 · 29%` (a counted one), `88 used` (no cap).
fn amount(window: &QuotaWindow) -> String {
    match (window.unit, window.limit, window.used_fraction()) {
        (QuotaUnit::Percent, _, _) => format!("{:.0}% used", window.used),
        (QuotaUnit::Requests, Some(limit), Some(fraction)) => {
            format!("{:.0} / {limit:.0} · {:.0}%", window.used, fraction * 100.0)
        }
        (QuotaUnit::Requests, Some(_) | None, _) => format!("{:.0} used", window.used),
    }
}

/// The bar: the used share filled in the pressure tone, the rest dim.
fn bar(theme: &Theme, window: &QuotaWindow, width: usize) -> Vec<Span> {
    let fraction = window.used_fraction().unwrap_or(0.0).clamp(0.0, 1.0);
    let filled = ((fraction * width as f64).round() as usize).min(width);
    vec![
        theme.fg_span(Pressure::of(window).color(), "█".repeat(filled)),
        theme.fg_span(ThemeColor::Dim, "░".repeat(width - filled)),
    ]
}

/// The card's source label.
fn source_label(source: QuotaSource) -> &'static str {
    match source {
        QuotaSource::Provider => "from provider",
        QuotaSource::ObservedResponse => "observed on last response",
    }
}

/// The card's title row: the pressure dot, the name, the plan, and the source.
fn card_title(theme: &Theme, width: usize, card: &ProviderQuota) -> Line {
    let tone = match &card.state {
        QuotaState::Windows { windows } => windows
            .iter()
            .map(Pressure::of)
            .max()
            .unwrap_or(Pressure::Ok)
            .color(),
        QuotaState::RateLimited { .. } => ThemeColor::Warning,
        QuotaState::Failed { .. } => ThemeColor::Error,
    };
    let mut meta = Vec::new();
    if let Some(plan) = &card.plan {
        meta.push(plan.clone());
    }
    meta.push(source_label(card.source).to_string());
    truncate_line(
        &vec![
            Span::raw(CARD_INDENT),
            theme.fg_span(tone, "● "),
            theme.bold(theme.fg_span(ThemeColor::Text, provider_name(&card.provider))),
            theme.fg_span(ThemeColor::Muted, format!(" · {}", meta.join(" · "))),
        ],
        width,
        "…",
    )
}

/// The row shape a card's windows share at one width, widest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowShape {
    bar: Option<usize>,
    reset: ResetDetail,
}

/// The card's window rows: one shape for the whole card (aligned columns), the widest that fits.
fn window_rows(
    theme: &Theme,
    width: usize,
    windows: &[QuotaWindow],
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let label_width = windows
        .iter()
        .map(|window| str_width(&window.label))
        .max()
        .unwrap_or(0)
        .min(MAX_LABEL);
    let amount_width = windows
        .iter()
        .map(|window| str_width(&amount(window)))
        .max()
        .unwrap_or(0);
    let reset_width = |detail: ResetDetail| {
        windows
            .iter()
            .filter_map(|window| window.resets_at_ms)
            .map(|at| str_width(&reset_label(now_ms, at, detail, civil)))
            .max()
            .map_or(0, |reset| reset + 2)
    };
    let fixed = str_width(ROW_INDENT) + label_width + 1 + amount_width;
    let shapes = [
        (ResetDetail::ClockAndCountdown, true),
        (ResetDetail::Countdown, true),
        (ResetDetail::Countdown, false),
        (ResetDetail::Bare, false),
    ];
    let shape = shapes
        .iter()
        .find_map(|(reset, with_bar)| {
            let reset_cols = reset_width(*reset);
            let base = fixed + reset_cols;
            match with_bar {
                true => (base + MIN_BAR < width).then(|| RowShape {
                    bar: Some((width - base - 1).clamp(MIN_BAR, MAX_BAR)),
                    reset: *reset,
                }),
                false => (base <= width).then_some(RowShape {
                    bar: None,
                    reset: *reset,
                }),
            }
        })
        .unwrap_or(RowShape {
            bar: None,
            reset: ResetDetail::Bare,
        });
    windows
        .iter()
        .map(|window| {
            let label = truncate_to_width(&window.label, label_width, "…");
            let pad = " ".repeat(label_width.saturating_sub(str_width(&label)) + 1);
            let mut row: Line = vec![
                Span::raw(ROW_INDENT),
                theme.fg_span(ThemeColor::Text, label),
                Span::raw(pad),
            ];
            if let Some(bar_width) = shape.bar {
                row.extend(bar(theme, window, bar_width));
                row.push(Span::raw(" "));
            }
            let amount = amount(window);
            let amount_pad = " ".repeat(amount_width.saturating_sub(str_width(&amount)));
            row.push(theme.fg_span(Pressure::of(window).color(), amount));
            if let Some(at) = window.resets_at_ms {
                row.push(Span::raw(amount_pad));
                row.push(theme.fg_span(
                    ThemeColor::Muted,
                    format!("  {}", reset_label(now_ms, at, shape.reset, civil)),
                ));
            }
            truncate_line(&row, width, "…")
        })
        .collect()
}

/// A muted note row at the card indent.
fn note_row(theme: &Theme, width: usize, color: ThemeColor, text: &str) -> Line {
    truncate_line(
        &vec![
            Span::raw(CARD_INDENT),
            theme.fg_span(color, text.to_string()),
        ],
        width,
        "…",
    )
}

/// One provider card: the title and its windows, the rate-limit line, or the error line.
fn card_rows(
    theme: &Theme,
    width: usize,
    card: &ProviderQuota,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let mut rows = vec![card_title(theme, width, card)];
    match &card.state {
        QuotaState::Windows { windows } if windows.is_empty() => rows.push(note_row(
            theme,
            width,
            ThemeColor::Muted,
            "  no limits reported",
        )),
        QuotaState::Windows { windows } => {
            rows.extend(window_rows(theme, width, windows, now_ms, civil));
        }
        QuotaState::RateLimited { resets_at_ms } => {
            // The narrow row drops the reset's clock first, like the window rows.
            let text = match resets_at_ms {
                Some(at) => [ResetDetail::ClockAndCountdown, ResetDetail::Countdown]
                    .iter()
                    .map(|detail| {
                        format!(
                            "  rate limited (HTTP 429) · {}",
                            reset_label(now_ms, *at, *detail, civil)
                        )
                    })
                    .reduce(|wide, narrow| {
                        if str_width(CARD_INDENT) + str_width(&wide) <= width {
                            wide
                        } else {
                            narrow
                        }
                    })
                    .unwrap_or_default(),
                None => "  rate limited (HTTP 429) · no reset time reported".to_string(),
            };
            rows.push(note_row(theme, width, ThemeColor::Warning, &text));
        }
        QuotaState::Failed { error } => {
            rows.push(note_row(
                theme,
                width,
                ThemeColor::Error,
                &format!("  {error}"),
            ));
        }
    }
    rows
}

/// The cards the page shows: the provider-reported ones, then an observed card for every
/// provider whose last response was rate limited and that has no provider card.
pub(crate) fn cards(
    quotas: Option<&QuotaReport>,
    spend: Option<&SpendHistory>,
) -> Vec<ProviderQuota> {
    let mut cards: Vec<ProviderQuota> = quotas
        .map(|report| report.providers.clone())
        .unwrap_or_default();
    for observed in spend
        .map(|history| history.rate_limits.as_slice())
        .unwrap_or_default()
    {
        if cards.iter().any(|card| card.provider == observed.provider) {
            continue;
        }
        cards.push(ProviderQuota {
            provider: observed.provider.clone(),
            plan: None,
            source: QuotaSource::ObservedResponse,
            fetched_at_ms: observed.observed_at_ms,
            state: QuotaState::RateLimited {
                resets_at_ms: observed.resets_at_ms,
            },
        });
    }
    cards
}

/// The quota half: the cards, or the half's loading, error, or empty state.
fn quota_rows(
    theme: &Theme,
    width: usize,
    quotas: &Section<QuotaReport>,
    spend: &Section<SpendHistory>,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let cards = cards(quotas.data.as_ref(), spend.data.as_ref());
    let mut rows = vec![section_title(theme, width, "Provider quotas")];
    // The half's own state leads the cards an observed rate limit may already show.
    let state = match (&quotas.data, &quotas.error, quotas.loading) {
        (None, _, true) => Some((ThemeColor::Muted, "Loading provider quotas…".to_string())),
        (None, Some(error), false) => Some((
            ThemeColor::Error,
            format!("Provider quotas unavailable ({error})."),
        )),
        (Some(_) | None, _, _) if cards.is_empty() => Some((
            ThemeColor::Muted,
            "No subscription sign-in reports quotas (/login adds one).".to_string(),
        )),
        (Some(_) | None, _, _) => None,
    };
    if let Some((color, text)) = state {
        rows.push(note_row(theme, width, color, &text));
        if !cards.is_empty() {
            rows.push(Vec::new());
        }
    }
    for (index, card) in cards.iter().enumerate() {
        if index > 0 {
            rows.push(Vec::new());
        }
        rows.extend(card_rows(theme, width, card, now_ms, civil));
    }
    // The daemon's live request pacing: what the adaptive admission controller allows each
    // provider/model now, and why it last changed. Read-only; nothing here is configured.
    let pacing = quotas
        .data
        .as_ref()
        .map(|report| report.pacing.as_slice())
        .unwrap_or_default();
    if !pacing.is_empty() {
        rows.push(Vec::new());
        rows.push(section_title(theme, width, "Request pacing"));
        for entry in pacing {
            let reason = match entry.reason {
                AdjustReason::Initial => "starting point",
                AdjustReason::Restored => "restored",
                AdjustReason::Headroom => "scaling up",
                AdjustReason::LimitReported => "provider limit",
                AdjustReason::Throttled => "backed off (429)",
                AdjustReason::Overloaded => "backed off (overloaded)",
                AdjustReason::LatencyRise => "backed off (latency)",
                AdjustReason::Decay => "re-probing",
            };
            let ceiling = entry
                .concurrency_ceiling
                .map(|ceiling| format!(" (ceiling {ceiling})"))
                .unwrap_or_default();
            rows.push(note_row(
                theme,
                width,
                ThemeColor::Muted,
                &format!(
                    "{}/{}  {}/{} in flight{ceiling} · {}/min · {reason}",
                    entry.provider,
                    entry.model,
                    entry.in_flight,
                    entry.concurrency,
                    entry.rate_per_min
                ),
            ));
        }
    }
    rows
}

fn section_title(theme: &Theme, width: usize, title: &str) -> Line {
    truncate_line(
        &vec![
            Span::raw(CARD_INDENT),
            theme.bold(theme.fg_span(ThemeColor::Accent, title.to_string())),
        ],
        width,
        "…",
    )
}

/// One spend row: the span name, the cost, then tokens and calls while they fit (calls drop
/// first, then tokens).
fn spend_row(theme: &Theme, width: usize, name: &str, totals: &SpendTotals) -> Line {
    let mut row: Line = vec![
        Span::raw(ROW_INDENT),
        theme.fg_span(ThemeColor::Muted, format!("{name:<9}")),
        theme.fg_span(ThemeColor::Text, format!("{:>9}", money(totals.cost))),
    ];
    let token_text = format!("  {:>7} tokens", tokens(totals.total_tokens()));
    let call_text = format!(
        "  {} call{}",
        totals.requests,
        if totals.requests == 1 { "" } else { "s" }
    );
    let used = str_width(ROW_INDENT) + 9 + 9;
    if used + str_width(&token_text) <= width {
        row.push(theme.fg_span(ThemeColor::Muted, token_text.clone()));
        if used + str_width(&token_text) + str_width(&call_text) <= width {
            row.push(theme.fg_span(ThemeColor::Dim, call_text));
        }
    }
    truncate_line(&row, width, "…")
}

/// The spend half: the four spans and the 30-day spark, or the half's state.
fn spend_rows(theme: &Theme, width: usize, spend: &Section<SpendHistory>) -> Vec<Line> {
    let mut rows = vec![section_title(theme, width, "Spend · local sessions")];
    let Some(history) = &spend.data else {
        let (color, text) = match (&spend.error, spend.loading) {
            (_, true) => (ThemeColor::Muted, "Reading local sessions…".to_string()),
            (Some(error), false) => (
                ThemeColor::Error,
                format!("Spend history unavailable ({error})."),
            ),
            (None, false) => (ThemeColor::Muted, "No spend history.".to_string()),
        };
        rows.push(note_row(theme, width, color, &text));
        return rows;
    };
    if history.all_time.requests == 0 {
        rows.push(note_row(
            theme,
            width,
            ThemeColor::Muted,
            "No billed model calls in local sessions yet.",
        ));
        return rows;
    }
    for (name, totals) in [
        ("Today", &history.today),
        ("7 days", &history.last_7_days),
        ("30 days", &history.last_30_days),
        ("All time", &history.all_time),
    ] {
        rows.push(spend_row(theme, width, name, totals));
    }
    // The 30 days by billing mode, once any call there carries a mode: a plan's calls are its
    // API-equivalent value, never part of the billed figure.
    let split = history.last_30_days.billing;
    let classified = split
        != pa_types::billing::BillingSplit {
            unclassified: split.unclassified,
            ..Default::default()
        };
    if classified {
        let calls = |calls: u64| format!("{calls} call{}", if calls == 1 { "" } else { "s" });
        let billed_calls = split.pay_as_you_go.calls + split.unclassified.calls;
        let plan_calls = split.plan.calls + split.flat_rate.calls;
        let free_calls = split.free.calls + split.unknown.calls;
        rows.push(note_row(
            theme,
            width,
            ThemeColor::Muted,
            "  30 days by billing",
        ));
        for (shown, color, text) in [
            (
                billed_calls > 0,
                ThemeColor::Text,
                format!(
                    "  Billed (pay-as-you-go)  {}  {}",
                    money(split.pay_as_you_go.cost + split.unclassified.cost),
                    calls(billed_calls)
                ),
            ),
            (
                plan_calls > 0,
                ThemeColor::Muted,
                format!(
                    "  Plan usage (API-equivalent value, not billed)  {}  {}",
                    money(split.plan.cost + split.flat_rate.cost),
                    calls(plan_calls)
                ),
            ),
            (
                free_calls > 0,
                ThemeColor::Dim,
                format!("  Free / unpriced  {}", calls(free_calls)),
            ),
        ] {
            if shown {
                rows.push(note_row(theme, width, color, &text));
            }
        }
    }
    // The spark weighs days by cost, or by tokens when nothing recorded a price.
    let priced = history.daily.iter().any(|day| day.totals.cost > 0.0);
    let values: Vec<f64> = history
        .daily
        .iter()
        .map(|day| {
            if priced {
                day.totals.cost
            } else {
                day.totals.total_tokens() as f64
            }
        })
        .collect();
    let prefix = format!("{ROW_INDENT}{:<9}", "30 days");
    let room = width.saturating_sub(str_width(&prefix));
    let shown = &values[values.len().saturating_sub(room)..];
    rows.push(truncate_line(
        &vec![
            theme.fg_span(ThemeColor::Muted, prefix),
            theme.fg_span(ThemeColor::Accent, spark(shown)),
        ],
        width,
        "",
    ));
    rows
}

/// The overview: the quota cards over the spend summary.
pub(crate) fn overview_rows(
    theme: &Theme,
    width: usize,
    quotas: &Section<QuotaReport>,
    spend: &Section<SpendHistory>,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let mut rows = quota_rows(theme, width, quotas, spend, now_ms, civil);
    rows.push(Vec::new());
    rows.extend(spend_rows(theme, width, spend));
    rows
}

/// The details view: every window with its full reset date and fetch time, the per-model spend,
/// and the daily spend list.
pub(crate) fn detail_rows(
    theme: &Theme,
    width: usize,
    quotas: &Section<QuotaReport>,
    spend: &Section<SpendHistory>,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let mut rows = vec![section_title(theme, width, "Provider quotas")];
    let cards = cards(quotas.data.as_ref(), spend.data.as_ref());
    if cards.is_empty() {
        rows.push(note_row(
            theme,
            width,
            ThemeColor::Muted,
            "No provider quotas.",
        ));
    }
    for card in &cards {
        rows.push(card_title(theme, width, card));
        rows.push(note_row(
            theme,
            width,
            ThemeColor::Dim,
            &format!(
                "  {} {} ago",
                match card.source {
                    QuotaSource::Provider => "fetched",
                    QuotaSource::ObservedResponse => "observed",
                },
                countdown(now_ms - card.fetched_at_ms)
            ),
        ));
        match &card.state {
            QuotaState::Windows { windows } => {
                for window in windows {
                    let mut text = format!("  {}: {}", window.label, amount(window));
                    if let Some(at) = window.resets_at_ms {
                        let _ = write!(
                            text,
                            " · {}",
                            reset_label(now_ms, at, ResetDetail::ClockAndCountdown, civil)
                        );
                    }
                    rows.push(note_row(theme, width, ThemeColor::Text, &text));
                }
            }
            QuotaState::RateLimited { .. } | QuotaState::Failed { .. } => {
                rows.extend(
                    card_rows(theme, width, card, now_ms, civil)
                        .into_iter()
                        .skip(1),
                );
            }
        }
    }
    rows.push(Vec::new());
    rows.push(section_title(
        theme,
        width,
        "Spend by model · 30 days / all time",
    ));
    let Some(history) = &spend.data else {
        rows.extend(spend_rows(theme, width, spend).into_iter().skip(1));
        return rows;
    };
    for caveat in [
        "  Estimates: cached-token pricing varies; long-context and service tiers",
        "  are not modelled; plan percentages lag up to 5 minutes.",
    ] {
        rows.push(note_row(theme, width, ThemeColor::Dim, caveat));
    }
    for model in &history.models {
        // The latest call's billing mode; a plan's cost is its API-equivalent value.
        let mode = match model.mode {
            Some(pa_types::billing::BillingMode::PayAsYouGo { .. }) => " · billed",
            Some(pa_types::billing::BillingMode::Plan { .. }) => " · plan, not billed",
            Some(pa_types::billing::BillingMode::FlatRate) => " · flat rate",
            Some(pa_types::billing::BillingMode::Free) => " · free",
            Some(pa_types::billing::BillingMode::Unknown) => " · unpriced",
            None => "",
        };
        rows.push(note_row(
            theme,
            width,
            ThemeColor::Text,
            &format!(
                "  {}/{}  {} · {} tok  /  {} · {} tok{mode}",
                model.provider,
                model.model,
                money(model.last_30_days.cost),
                tokens(model.last_30_days.total_tokens()),
                money(model.all_time.cost),
                tokens(model.all_time.total_tokens())
            ),
        ));
    }
    rows.push(Vec::new());
    rows.push(section_title(theme, width, "Daily spend · last 30 days"));
    for day in history.daily.iter().rev() {
        rows.push(note_row(
            theme,
            width,
            if day.totals.requests == 0 {
                ThemeColor::Dim
            } else {
                ThemeColor::Text
            },
            &format!(
                "  {}  {:>9}  {:>7} tok  {} call{}",
                day.date,
                money(day.totals.cost),
                tokens(day.totals.total_tokens()),
                day.totals.requests,
                if day.totals.requests == 1 { "" } else { "s" }
            ),
        ));
    }
    rows
}
