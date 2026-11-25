//! The `/usage` page's content rows: the provider quota cards, the request pacing, the spend
//! tables and spark. The overview and the details view render through the same builders, so a
//! card, a gauge, a number column, and a state note look the same in both: the details view only
//! adds each card's age, the per-model and daily tables. Every row fits its width: a quota row
//! drops, in order, the reset's local clock, then its gauge, then the `resets` wording, and only
//! then truncates the label; a spend row drops its note, then its calls, then its tokens.

use pa_types::billing::BillingMode;
use pa_types::control::AdjustReason;
use pa_types::platform::LocalCivil;
use pa_types::provider_usage::{
    ProviderPacing, ProviderQuota, QuotaReport, QuotaSource, QuotaState, QuotaUnit, QuotaWindow,
    SpendHistory, SpendTotals,
};

use super::format::{countdown, money, reset_label, spark, tokens, ResetDetail};
use super::{Section, UsageView};
use crate::menu_chrome::CHROME_INDENT;
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

/// The content column under a heading: window, pacing, spend, and state rows all start here.
const ROW_INDENT: &str = "    ";
const MIN_BAR: usize = 8;
const MAX_BAR: usize = 24;
const MAX_LABEL: usize = 18;
const MAX_TABLE_LABEL: usize = 28;
const SEPARATOR: &str = " \u{b7} ";

/// The local-time lookup the rows format resets with (the platform's zone in production).
pub(crate) type CivilClock<'a> = &'a dyn Fn(i64) -> Option<LocalCivil>;

/// The display name of a provider id.
fn provider_name(provider: &str) -> &str {
    match provider {
        "anthropic" => "Anthropic",
        "openai-codex" => "ChatGPT (Codex)",
        "github-copilot" => "GitHub Copilot",
        "synthetic" => "Synthetic",
        "prime-inference" => "Prime Inference",
        "openai" => "OpenAI",
        "google" => "Google",
        "xai" => "xAI",
        other => other,
    }
}

/// How full a window is, which tones its dot and gauge.
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

/// The gauge in the TUI's one gauge grammar (the tray's and `/context`'s): `━` for the used
/// share in the pressure tone, `─` for the rest in the subtle border tone.
fn gauge(theme: &Theme, window: &QuotaWindow, width: usize) -> Vec<Span> {
    let fraction = window.used_fraction().unwrap_or(0.0).clamp(0.0, 1.0);
    let filled = ((fraction * width as f64).round() as usize).min(width);
    vec![
        theme.fg_span(Pressure::of(window).color(), "\u{2501}".repeat(filled)),
        theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(width - filled)),
    ]
}

/// The card's source label.
fn source_label(source: QuotaSource) -> &'static str {
    match source {
        QuotaSource::Provider => "from provider",
        QuotaSource::ObservedResponse => "observed on last response",
    }
}

/// The card's title row: the pressure dot, the name, the plan, the source, and in the details
/// view how old the card is. Too narrow for every segment, the source drops first, then the
/// plan; the credential label and the age stay, and only then the row truncates.
fn card_title(
    theme: &Theme,
    width: usize,
    card: &ProviderQuota,
    view: UsageView,
    now_ms: i64,
) -> Line {
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
    let age = match view {
        UsageView::Overview => None,
        UsageView::Details => Some(format!(
            "{} {} ago",
            match card.source {
                QuotaSource::Provider => "fetched",
                QuotaSource::ObservedResponse => "observed",
            },
            countdown(now_ms - card.fetched_at_ms)
        )),
    };
    // A provider holding several credentials shows one card per credential: `Provider · label ·
    // plan`. The label names the card, so it never drops.
    let (account, plan, source, age) = (
        card.account.as_deref(),
        card.plan.as_deref(),
        Some(source_label(card.source)),
        age.as_deref(),
    );
    let lead =
        str_width(CHROME_INDENT) + str_width("● ") + str_width(provider_name(&card.provider));
    let variants = [
        [account, plan, source, age],
        [account, plan, None, age],
        [account, None, None, age],
    ];
    let meta = variants
        .iter()
        .map(|segments| segments.iter().flatten().copied().collect::<Vec<&str>>())
        .filter(|segments| !segments.is_empty())
        .reduce(|wide, narrow| {
            let fits = lead + str_width(&format!("{SEPARATOR}{}", wide.join(SEPARATOR))) <= width;
            if fits {
                wide
            } else {
                narrow
            }
        })
        .unwrap_or_default();
    truncate_line(
        &vec![
            Span::raw(CHROME_INDENT),
            theme.fg_span(tone, "● "),
            theme.bold(theme.fg_span(ThemeColor::Text, provider_name(&card.provider))),
            theme.fg_span(
                ThemeColor::Muted,
                format!("{SEPARATOR}{}", meta.join(SEPARATOR)),
            ),
        ],
        width,
        "…",
    )
}

/// The window rows' columns, shared by every card on the page so the gauges and amounts line
/// up from card to card: the label and amount widths and the widest shape that fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowColumns {
    label: usize,
    amount: usize,
    bar: Option<usize>,
    reset: ResetDetail,
}

impl WindowColumns {
    fn of(windows: &[&QuotaWindow], width: usize, now_ms: i64, civil: CivilClock<'_>) -> Self {
        let label = windows
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
        let fixed = str_width(ROW_INDENT) + label + 1 + amount_width;
        [
            (ResetDetail::ClockAndCountdown, true),
            (ResetDetail::Countdown, true),
            (ResetDetail::Countdown, false),
            (ResetDetail::Bare, false),
        ]
        .iter()
        .find_map(|(reset, with_bar)| {
            let base = fixed + reset_width(*reset);
            let columns = |bar| WindowColumns {
                label,
                amount: amount_width,
                bar,
                reset: *reset,
            };
            match with_bar {
                true => (base + MIN_BAR < width)
                    .then(|| columns(Some((width - base - 1).clamp(MIN_BAR, MAX_BAR)))),
                false => (base <= width).then(|| columns(None)),
            }
        })
        .unwrap_or(WindowColumns {
            label,
            amount: amount_width,
            bar: None,
            reset: ResetDetail::Bare,
        })
    }

    fn row(
        &self,
        theme: &Theme,
        width: usize,
        window: &QuotaWindow,
        now_ms: i64,
        civil: CivilClock<'_>,
    ) -> Line {
        let label = truncate_to_width(&window.label, self.label, "…");
        let pad = " ".repeat(self.label.saturating_sub(str_width(&label)) + 1);
        let mut row: Line = vec![
            Span::raw(ROW_INDENT),
            theme.fg_span(ThemeColor::Text, label),
            Span::raw(pad),
        ];
        if let Some(bar_width) = self.bar {
            row.extend(gauge(theme, window, bar_width));
            row.push(Span::raw(" "));
        }
        let amount = amount(window);
        let amount_pad = " ".repeat(self.amount.saturating_sub(str_width(&amount)));
        row.push(theme.fg_span(Pressure::of(window).color(), amount));
        if let Some(at) = window.resets_at_ms {
            row.push(Span::raw(amount_pad));
            row.push(theme.fg_span(
                ThemeColor::Muted,
                format!("  {}", reset_label(now_ms, at, self.reset, civil)),
            ));
        }
        truncate_line(&row, width, "…")
    }
}

/// A prose row at the content column, word-wrapped to the width (a word wider than the row
/// truncates): state notes, raw errors, and caveats read whole at every width.
fn note(theme: &Theme, width: usize, color: ThemeColor, text: &str) -> Vec<Line> {
    let budget = width.saturating_sub(str_width(ROW_INDENT)).max(1);
    let mut rows: Vec<String> = Vec::new();
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        match rows.last_mut() {
            Some(row) if str_width(row) + 1 + str_width(word) <= budget => {
                row.push(' ');
                row.push_str(word);
            }
            Some(_) | None => rows.push(word.to_string()),
        }
    }
    rows.iter()
        .map(|row| {
            truncate_line(
                &vec![Span::raw(ROW_INDENT), theme.fg_span(color, row.clone())],
                width,
                "…",
            )
        })
        .collect()
}

/// One provider card: the title and its windows, the rate-limit line, or the error.
fn card_rows(
    theme: &Theme,
    width: usize,
    card: &ProviderQuota,
    columns: &WindowColumns,
    view: UsageView,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let mut rows = vec![card_title(theme, width, card, view, now_ms)];
    match &card.state {
        QuotaState::Windows { windows } if windows.is_empty() => {
            rows.extend(note(theme, width, ThemeColor::Muted, "no limits reported"));
        }
        QuotaState::Windows { windows } => rows.extend(
            windows
                .iter()
                .map(|window| columns.row(theme, width, window, now_ms, civil)),
        ),
        QuotaState::RateLimited { resets_at_ms } => {
            // The narrow row drops the reset's clock first, like the window rows.
            let text = match resets_at_ms {
                Some(at) => [ResetDetail::ClockAndCountdown, ResetDetail::Countdown]
                    .iter()
                    .map(|detail| {
                        format!(
                            "rate limited (HTTP 429){SEPARATOR}{}",
                            reset_label(now_ms, *at, *detail, civil)
                        )
                    })
                    .reduce(|wide, narrow| {
                        if str_width(ROW_INDENT) + str_width(&wide) <= width {
                            wide
                        } else {
                            narrow
                        }
                    })
                    .unwrap_or_default(),
                None => format!("rate limited (HTTP 429){SEPARATOR}no reset time reported"),
            };
            rows.push(truncate_line(
                &vec![
                    Span::raw(ROW_INDENT),
                    theme.fg_span(ThemeColor::Warning, text),
                ],
                width,
                "…",
            ));
        }
        QuotaState::Failed { error } => {
            rows.extend(note(theme, width, ThemeColor::Error, error));
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
    let mut cards: Vec<ProviderQuota> = quotas.map_or_default(|report| report.providers.clone());
    for observed in spend.map_or_default(|history| history.rate_limits.as_slice()) {
        if cards.iter().any(|card| card.provider == observed.provider) {
            continue;
        }
        cards.push(ProviderQuota {
            provider: observed.provider.clone(),
            plan: None,
            account: None,
            source: QuotaSource::ObservedResponse,
            fetched_at_ms: observed.observed_at_ms,
            state: QuotaState::RateLimited {
                resets_at_ms: observed.resets_at_ms,
            },
        });
    }
    cards
}

/// The live pacing table: what the adaptive admission controller allows each provider/model
/// now, and why it last changed. Read-only; nothing here is configured.
fn pacing_rows(theme: &Theme, width: usize, pacing: &[ProviderPacing]) -> Vec<Line> {
    let labels: Vec<String> = pacing
        .iter()
        .map(|entry| format!("{}/{}", entry.provider, entry.model))
        .collect();
    let label_width = labels
        .iter()
        .map(|label| str_width(label))
        .max()
        .unwrap_or(0)
        .min(MAX_TABLE_LABEL);
    pacing
        .iter()
        .zip(&labels)
        .map(|(entry, label)| {
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
            let mut detail = vec![format!(
                "{}/{} in flight",
                entry.in_flight, entry.concurrency
            )];
            if let Some(ceiling) = entry.concurrency_ceiling {
                detail.push(format!("ceiling {ceiling}"));
            }
            detail.push(format!("{}/min", entry.rate_per_min));
            detail.push(reason.to_string());
            let label = truncate_to_width(label, label_width, "…");
            truncate_line(
                &vec![
                    Span::raw(ROW_INDENT),
                    theme.fg_span(ThemeColor::Text, label.clone()),
                    Span::raw(" ".repeat(label_width.saturating_sub(str_width(&label)) + 2)),
                    theme.fg_span(ThemeColor::Muted, detail.join(SEPARATOR)),
                ],
                width,
                "…",
            )
        })
        .collect()
}

/// The quota half: the heading, the half's loading, error, or empty state, the cards, and the
/// live request pacing. The overview and the details view share it whole.
fn quota_rows(
    theme: &Theme,
    width: usize,
    quotas: &Section<QuotaReport>,
    spend: &Section<SpendHistory>,
    view: UsageView,
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
        rows.extend(note(theme, width, color, &text));
        if !cards.is_empty() {
            rows.push(Vec::new());
        }
    }
    let windows: Vec<&QuotaWindow> = cards
        .iter()
        .flat_map(|card| match &card.state {
            QuotaState::Windows { windows } => windows.iter().collect(),
            QuotaState::RateLimited { .. } | QuotaState::Failed { .. } => Vec::new(),
        })
        .collect();
    let columns = WindowColumns::of(&windows, width, now_ms, civil);
    for (index, card) in cards.iter().enumerate() {
        if index > 0 {
            rows.push(Vec::new());
        }
        rows.extend(card_rows(theme, width, card, &columns, view, now_ms, civil));
    }
    let pacing = quotas
        .data
        .as_ref()
        .map_or_default(|report| report.pacing.as_slice());
    if !pacing.is_empty() {
        rows.push(Vec::new());
        rows.push(section_title(theme, width, "Request pacing"));
        rows.extend(pacing_rows(theme, width, pacing));
    }
    rows
}

fn section_title(theme: &Theme, width: usize, title: &str) -> Line {
    truncate_line(
        &vec![
            Span::raw(CHROME_INDENT),
            theme.bold(theme.fg_span(ThemeColor::Accent, title.to_string())),
        ],
        width,
        "…",
    )
}

/// `1 call`, `3 calls`.
fn calls(count: u64) -> String {
    format!("{count} call{}", if count == 1 { "" } else { "s" })
}

/// How loud a spend row reads: a billed or recorded row, the plan's unbilled value, or a free or
/// empty row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowTone {
    Normal,
    Muted,
    Dim,
}

impl RowTone {
    /// The tone of the row's figures.
    fn value(self) -> ThemeColor {
        match self {
            RowTone::Normal => ThemeColor::Text,
            RowTone::Muted => ThemeColor::Muted,
            RowTone::Dim => ThemeColor::Dim,
        }
    }

    /// The tone of the row's label: one step quieter than a normal row's figures.
    fn label(self) -> ThemeColor {
        match self {
            RowTone::Normal | RowTone::Muted => ThemeColor::Muted,
            RowTone::Dim => ThemeColor::Dim,
        }
    }
}

/// One spend-table row's cells. A row without a cost or a token count holds the blank column, so
/// the columns after it stay aligned.
struct SpendCells {
    label: String,
    cost: Option<f64>,
    tokens: Option<u64>,
    calls: u64,
    note: String,
    tone: RowTone,
}

impl SpendCells {
    fn totals(label: &str, totals: &SpendTotals, note: String) -> Self {
        Self {
            label: label.to_string(),
            cost: Some(totals.cost),
            tokens: Some(totals.total_tokens()),
            calls: totals.requests,
            note,
            tone: if totals.requests == 0 {
                RowTone::Dim
            } else {
                RowTone::Normal
            },
        }
    }
}

/// A spend table's columns: every row of the table shares them, so the numbers right-align.
struct SpendColumns {
    label: usize,
    cost: usize,
    tokens: usize,
    calls: usize,
}

impl SpendColumns {
    fn of<'a>(cells: impl Iterator<Item = &'a SpendCells>) -> Self {
        cells.fold(
            Self {
                label: 0,
                cost: 0,
                tokens: 0,
                calls: 0,
            },
            |columns, cell| Self {
                label: columns
                    .label
                    .max(str_width(&cell.label).min(MAX_TABLE_LABEL)),
                cost: columns
                    .cost
                    .max(cell.cost.map_or(0, |cost| str_width(&money(cost)))),
                tokens: columns
                    .tokens
                    .max(cell.tokens.map_or(0, |count| str_width(&tokens(count)))),
                calls: columns.calls.max(str_width(&calls(cell.calls))),
            },
        )
    }

    /// The label cell at the content column: the label padded to the column and the gap.
    fn label_cell(&self, theme: &Theme, width: usize, label: &str, tone: ThemeColor) -> Line {
        let fixed = str_width(ROW_INDENT) + 2 + self.cost;
        let label_width = self.label.min(width.saturating_sub(fixed));
        let label = truncate_to_width(label, label_width, "…");
        vec![
            Span::raw(ROW_INDENT),
            theme.fg_span(tone, label.clone()),
            Span::raw(" ".repeat(label_width.saturating_sub(str_width(&label)) + 2)),
        ]
    }

    /// One row: the label and cost, then tokens, calls, and the note while they fit. The
    /// columns drop for the whole table (calls, then tokens), so every row keeps the same cells;
    /// only a note too long for its row drops alone.
    fn row(&self, theme: &Theme, width: usize, cells: &SpendCells) -> Line {
        let token_cols = match self.tokens {
            0 => 0,
            tokens => 2 + tokens + str_width(" tokens"),
        };
        let fixed = str_width(ROW_INDENT) + self.label + 2 + self.cost;
        let show_tokens = fixed + token_cols <= width;
        let show_calls = show_tokens && fixed + token_cols + 2 + self.calls <= width;
        let mut row = self.label_cell(theme, width, &cells.label, cells.tone.label());
        let cost = cells.cost.map_or_default(money);
        row.push(theme.fg_span(
            cells.tone.value(),
            format!("{cost:>width$}", width = self.cost),
        ));
        if show_tokens && token_cols > 0 {
            row.push(theme.fg_span(
                ThemeColor::Muted,
                match cells.tokens {
                    Some(count) => {
                        format!("  {:>width$} tokens", tokens(count), width = self.tokens)
                    }
                    None => " ".repeat(token_cols),
                },
            ));
        }
        if show_calls {
            row.push(theme.fg_span(
                ThemeColor::Dim,
                format!("  {:<width$}", calls(cells.calls), width = self.calls),
            ));
            let note = format!("  {}", cells.note);
            if !cells.note.is_empty()
                && fixed + token_cols + 2 + self.calls + str_width(&note) <= width
            {
                row.push(theme.fg_span(ThemeColor::Dim, note));
            }
        }
        truncate_line(&row, width, "…")
    }
}

/// A spend half's state when it has no table to show: loading, failed, or nothing recorded.
fn spend_state(spend: &Section<SpendHistory>) -> Result<&SpendHistory, (ThemeColor, String)> {
    let Some(history) = &spend.data else {
        return Err(match (&spend.error, spend.loading) {
            (_, true) => (ThemeColor::Muted, "Reading local sessions…".to_string()),
            (Some(error), false) => (
                ThemeColor::Error,
                format!("Spend history unavailable ({error})."),
            ),
            (None, false) => (ThemeColor::Muted, "No spend history.".to_string()),
        });
    };
    if history.all_time.requests == 0 {
        return Err((
            ThemeColor::Muted,
            "No billed model calls in local sessions yet.".to_string(),
        ));
    }
    Ok(history)
}

/// The overview's spend half: the four spans, the 30 days by billing mode, and the 30-day spark,
/// or the half's state.
fn spend_rows(theme: &Theme, width: usize, spend: &Section<SpendHistory>) -> Vec<Line> {
    let mut rows = vec![section_title(theme, width, "Spend · local sessions")];
    let history = match spend_state(spend) {
        Ok(history) => history,
        Err((color, text)) => {
            rows.extend(note(theme, width, color, &text));
            return rows;
        }
    };
    let spans: Vec<SpendCells> = [
        ("Today", &history.today),
        ("7 days", &history.last_7_days),
        ("30 days", &history.last_30_days),
        ("All time", &history.all_time),
    ]
    .iter()
    .map(|(name, totals)| SpendCells::totals(name, totals, String::new()))
    .collect();
    // The 30 days by billing mode, once any call there carries a mode: a plan's calls are its
    // API-equivalent value, never part of the billed figure.
    let split = history.last_30_days.billing;
    let classified = split
        != pa_types::billing::BillingSplit {
            unclassified: split.unclassified,
            ..Default::default()
        };
    let mut modes: Vec<SpendCells> = Vec::new();
    if classified {
        for (label, cost, count, note, tone) in [
            (
                "Billed",
                Some(split.pay_as_you_go.cost + split.unclassified.cost),
                split.pay_as_you_go.calls + split.unclassified.calls,
                "pay-as-you-go",
                RowTone::Normal,
            ),
            (
                "Plan (not billed)",
                Some(split.plan.cost + split.flat_rate.cost),
                split.plan.calls + split.flat_rate.calls,
                "API-equivalent value",
                RowTone::Muted,
            ),
            (
                "Free / unpriced",
                None,
                split.free.calls + split.unknown.calls,
                "",
                RowTone::Dim,
            ),
        ] {
            if count > 0 {
                modes.push(SpendCells {
                    label: label.to_string(),
                    cost,
                    tokens: None,
                    calls: count,
                    note: note.to_string(),
                    tone,
                });
            }
        }
    }
    let columns = SpendColumns::of(spans.iter().chain(&modes));
    rows.extend(spans.iter().map(|cells| columns.row(theme, width, cells)));
    if !modes.is_empty() {
        rows.push(Vec::new());
        rows.push(truncate_line(
            &vec![
                Span::raw(ROW_INDENT),
                theme.fg_span(ThemeColor::Muted, "30 days by billing"),
            ],
            width,
            "…",
        ));
        rows.extend(modes.iter().map(|cells| columns.row(theme, width, cells)));
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
    rows.push(Vec::new());
    let mut spark_row = columns.label_cell(theme, width, "30 days", ThemeColor::Muted);
    let room = width.saturating_sub(crate::width::spans_width(&spark_row));
    let shown = &values[values.len().saturating_sub(room)..];
    spark_row.push(theme.fg_span(ThemeColor::Accent, spark(shown)));
    rows.push(truncate_line(&spark_row, width, ""));
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
    let mut rows = quota_rows(
        theme,
        width,
        quotas,
        spend,
        UsageView::Overview,
        now_ms,
        civil,
    );
    rows.push(Vec::new());
    rows.extend(spend_rows(theme, width, spend));
    rows
}

/// The details view: the same quota half with each card's age, then the per-model and daily
/// spend tables in the overview's columns and number formats.
pub(crate) fn detail_rows(
    theme: &Theme,
    width: usize,
    quotas: &Section<QuotaReport>,
    spend: &Section<SpendHistory>,
    now_ms: i64,
    civil: CivilClock<'_>,
) -> Vec<Line> {
    let mut rows = quota_rows(
        theme,
        width,
        quotas,
        spend,
        UsageView::Details,
        now_ms,
        civil,
    );
    rows.push(Vec::new());
    rows.push(section_title(theme, width, "Spend by model · last 30 days"));
    let history = match spend_state(spend) {
        Ok(history) => history,
        Err((color, text)) => {
            rows.extend(note(theme, width, color, &text));
            return rows;
        }
    };
    rows.extend(note(
        theme,
        width,
        ThemeColor::Dim,
        "Estimates: cached-token pricing varies; long-context and service tiers are not \
         modelled; plan percentages lag up to 5 minutes.",
    ));
    let models: Vec<SpendCells> = history
        .models
        .iter()
        .map(|model| {
            // The latest call's billing mode; a plan's cost is its API-equivalent value.
            let mode = match model.mode {
                Some(BillingMode::PayAsYouGo { .. }) => Some("billed"),
                Some(BillingMode::Plan { .. }) => Some("plan, not billed"),
                Some(BillingMode::FlatRate) => Some("flat rate"),
                Some(BillingMode::Free) => Some("free"),
                Some(BillingMode::Unknown) => Some("unpriced"),
                None => None,
            };
            let mut note = vec![format!("all time {}", money(model.all_time.cost))];
            note.extend(mode.map(str::to_string));
            SpendCells::totals(
                &format!("{}/{}", model.provider, model.model),
                &model.last_30_days,
                note.join(SEPARATOR),
            )
        })
        .collect();
    let columns = SpendColumns::of(models.iter());
    rows.extend(models.iter().map(|cells| columns.row(theme, width, cells)));
    rows.push(Vec::new());
    rows.push(section_title(theme, width, "Daily spend · last 30 days"));
    let days: Vec<SpendCells> = history
        .daily
        .iter()
        .rev()
        .map(|day| SpendCells::totals(&day.date, &day.totals, String::new()))
        .collect();
    let columns = SpendColumns::of(days.iter());
    rows.extend(days.iter().map(|cells| columns.row(theme, width, cells)));
    rows
}
