//! The `/usage` page: the reset-time formats (zones injected), and whole-frame renders at a wide
//! and a narrow width for the loading, filled, error, and empty states, plus the details view's
//! navigation.

use pa_types::platform::LocalCivil;
use pa_types::provider_usage::{
    DailySpend, ModelSpend, ObservedRateLimit, ProviderQuota, QuotaReport, QuotaSource, QuotaState,
    QuotaUnit, QuotaWindow, SpendHistory, SpendTotals,
};

use super::format::{countdown, money, next_label_change, reset_label, tokens, ResetDetail};
use super::*;

/// 2026-11-18 12:00:00 UTC, a Wednesday.
const NOW: i64 = 1_795_003_200_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

/// A fixed-offset zone: the civil time of `epoch_ms` at `offset_minutes` east of UTC.
fn zone(offset_minutes: i64) -> impl Fn(i64) -> Option<LocalCivil> {
    move |epoch_ms| {
        let local = epoch_ms + offset_minutes * MINUTE;
        let days = local.div_euclid(DAY);
        let of_day = local.rem_euclid(DAY);
        // Civil from days (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Some(LocalCivil {
            year: i32::try_from(year).ok()?,
            month: u8::try_from(month).ok()?,
            day: u8::try_from(day).ok()?,
            // 1970-01-01 was a Thursday.
            weekday: u8::try_from((days + 4).rem_euclid(7)).ok()?,
            hour: u8::try_from(of_day / HOUR).ok()?,
            minute: u8::try_from(of_day % HOUR / MINUTE).ok()?,
        })
    }
}

fn no_zone(_epoch_ms: i64) -> Option<LocalCivil> {
    None
}

#[test]
fn countdowns_floor_each_unit() {
    assert_eq!(
        [
            countdown(45_999),
            countdown(13 * MINUTE + 59_000),
            countdown(2 * HOUR + 13 * MINUTE),
            countdown(5 * HOUR),
            countdown(3 * DAY + 4 * HOUR + 59 * MINUTE),
            countdown(-5),
        ],
        ["45s", "13m", "2h 13m", "5h", "3d 4h", "0s"].map(str::to_string)
    );
}

/// The same reset reads in the viewer's zone: the clock alone today, the weekday this week, the
/// date further out; a passed reset reads as past; no zone binding keeps the countdown only.
#[test]
fn reset_labels_follow_the_zone_and_the_reset_side() {
    let utc = zone(0);
    let kolkata = zone(330);
    let in_7h = NOW + 7 * HOUR + 30 * MINUTE;
    assert_eq!(
        [
            reset_label(
                NOW,
                NOW + 2 * HOUR + 13 * MINUTE,
                ResetDetail::ClockAndCountdown,
                &utc
            ),
            // 19:30 UTC is 01:00 Thursday in Kolkata: past local midnight, so the weekday shows.
            reset_label(NOW, in_7h, ResetDetail::ClockAndCountdown, &kolkata),
            reset_label(NOW, NOW + 13 * DAY, ResetDetail::ClockAndCountdown, &utc),
            reset_label(NOW, NOW - 5 * MINUTE, ResetDetail::ClockAndCountdown, &utc),
            reset_label(
                NOW,
                NOW + 2 * HOUR,
                ResetDetail::ClockAndCountdown,
                &no_zone
            ),
            reset_label(NOW, NOW + 2 * HOUR, ResetDetail::Countdown, &utc),
            reset_label(NOW, NOW - 5 * MINUTE, ResetDetail::Bare, &utc),
        ],
        [
            "resets 14:13 (in 2h 13m)",
            "resets Thu 01:00 (in 7h 30m)",
            "resets Tue 1 Dec 12:00 (in 13d)",
            "reset 11:55 (5m ago)",
            "resets in 2h",
            "resets in 2h",
            "5m ago",
        ]
        .map(str::to_string)
    );
}

/// The next repaint lands exactly where the label's text changes.
#[test]
fn the_next_repaint_is_the_labels_next_change() {
    let reset = NOW + 2 * HOUR + 13 * MINUTE + 20_000;
    let next = next_label_change(NOW, reset);
    assert_eq!(next, NOW + 20_001);
    assert_eq!(countdown(reset - (next - 1)), "2h 13m");
    assert_eq!(countdown(reset - next), "2h 12m");
    // Past the reset the age counts up a minute at a time.
    assert_eq!(next_label_change(NOW, NOW - 90_000), NOW + 30_000);
}

#[test]
fn money_and_token_counts_stay_compact() {
    assert_eq!(
        [money(0.0), money(0.004), money(9.5), money(1234.4)],
        ["$0.00", "<$0.01", "$9.50", "$1,234"].map(str::to_string)
    );
    assert_eq!(
        [tokens(950), tokens(12_345), tokens(4_500_000)],
        ["950", "12.3k", "4.5M"].map(str::to_string)
    );
}

fn quota_report() -> QuotaReport {
    QuotaReport {
        providers: vec![
            ProviderQuota {
                provider: "anthropic".into(),
                plan: None,
                source: QuotaSource::Provider,
                fetched_at_ms: NOW - 2 * MINUTE,
                state: QuotaState::Windows {
                    windows: vec![
                        QuotaWindow {
                            label: "5-hour".into(),
                            used: 37.0,
                            limit: Some(100.0),
                            unit: QuotaUnit::Percent,
                            resets_at_ms: Some(NOW + 2 * HOUR + 13 * MINUTE),
                        },
                        QuotaWindow {
                            label: "Weekly".into(),
                            used: 95.0,
                            limit: Some(100.0),
                            unit: QuotaUnit::Percent,
                            resets_at_ms: Some(NOW + 2 * DAY + 19 * HOUR),
                        },
                    ],
                },
            },
            ProviderQuota {
                provider: "github-copilot".into(),
                plan: Some("individual".into()),
                source: QuotaSource::Provider,
                fetched_at_ms: NOW - 2 * MINUTE,
                state: QuotaState::Failed {
                    error: "the usage endpoint did not answer in time".into(),
                },
            },
        ],
        pacing: Vec::new(),
    }
}

fn spend_totals(cost: f64, tokens: u64, requests: u64) -> SpendTotals {
    SpendTotals {
        input_tokens: tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost,
        requests,
        billing: pa_types::billing::BillingSplit::default(),
    }
}

fn spend_history() -> SpendHistory {
    let daily = (0..30)
        .map(|index| DailySpend {
            date: format!("2026-11-{:02}", index % 28 + 1),
            totals: if index == 29 {
                spend_totals(9.5, 2_100, 2)
            } else if index == 20 {
                spend_totals(4.75, 900, 1)
            } else {
                SpendTotals::default()
            },
        })
        .collect();
    SpendHistory {
        today: spend_totals(9.5, 2_100, 2),
        last_7_days: spend_totals(9.5, 2_100, 2),
        last_30_days: spend_totals(14.25, 3_000, 3),
        all_time: spend_totals(20.0, 12_000, 7),
        daily,
        models: vec![ModelSpend {
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            mode: None,
            today: spend_totals(9.5, 2_100, 2),
            last_7_days: spend_totals(9.5, 2_100, 2),
            last_30_days: spend_totals(14.25, 3_000, 3),
            all_time: spend_totals(20.0, 12_000, 7),
        }],
        rate_limits: vec![ObservedRateLimit {
            provider: "openai-codex".into(),
            observed_at_ms: NOW - 30 * MINUTE,
            resets_at_ms: Some(NOW + HOUR),
        }],
        sessions: 4,
    }
}

fn plain(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| -> &str { span.content.as_ref() })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn render(page: &mut UsagePage, width: usize, rows: usize) -> Vec<String> {
    let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
    let kb = KeybindingsManager::new();
    plain(&page.render(&theme, width, &kb, rows, NOW, &zone(0)))
}

fn rule(width: usize) -> String {
    "\u{2500}".repeat(width)
}

#[test]
fn the_page_opens_loading_at_both_widths() {
    let mut page = UsagePage::new(None, None);
    for width in [80, 44] {
        assert_eq!(
            render(&mut page, width, 30),
            vec![
                rule(width),
                "  Usage".into(),
                String::new(),
                String::new(),
                "  Provider quotas".into(),
                "    Loading provider quotas…".into(),
                String::new(),
                "  Spend · local sessions".into(),
                "    Reading local sessions…".into(),
                "  R refresh · Enter details · Esc close".into(),
                String::new(),
            ]
        );
    }
}

/// The filled page: window rows in the TUI's one gauge grammar with their columns shared by
/// every card, the failed card's raw error wrapped whole, the observed rate limit, the spend
/// table and spark. The narrow width drops the reset clock and the gauges first, and a card
/// title drops its source before it truncates.
#[test]
fn the_filled_page_fits_both_widths() {
    let mut page = UsagePage::new(None, None);
    page.apply_quotas(Ok(quota_report()));
    page.apply_spend(Ok(spend_history()));
    assert_eq!(
        render(&mut page, 80, 40),
        vec![
            rule(80),
            "  Usage".into(),
            "  checked 2m ago".into(),
            String::new(),
            "  Provider quotas".into(),
            "  ● Anthropic · from provider".into(),
            "    5-hour ━━━━━━━━━─────────────── 37% used  resets 14:13 (in 2h 13m)".into(),
            "    Weekly ━━━━━━━━━━━━━━━━━━━━━━━─ 95% used  resets Sat 07:00 (in 2d 19h)".into(),
            String::new(),
            "  ● GitHub Copilot · individual · from provider".into(),
            "    the usage endpoint did not answer in time".into(),
            String::new(),
            "  ● ChatGPT (Codex) · observed on last response".into(),
            "    rate limited (HTTP 429) · resets 13:00 (in 1h)".into(),
            String::new(),
            "  Spend · local sessions".into(),
            "    Today      $9.50   2.1k tokens  2 calls".into(),
            "    7 days     $9.50   2.1k tokens  2 calls".into(),
            "    30 days   $14.25   3.0k tokens  3 calls".into(),
            "    All time  $20.00  12.0k tokens  7 calls".into(),
            String::new(),
            "    30 days   ····················▅········█".into(),
            "  R refresh · Enter details · Esc close".into(),
            String::new(),
        ]
    );
    assert_eq!(
        render(&mut page, 44, 40),
        vec![
            rule(44),
            "  Usage".into(),
            "  checked 2m ago".into(),
            String::new(),
            "  Provider quotas".into(),
            "  ● Anthropic · from provider".into(),
            "    5-hour 37% used  resets in 2h 13m".into(),
            "    Weekly 95% used  resets in 2d 19h".into(),
            String::new(),
            "  ● GitHub Copilot · individual".into(),
            "    the usage endpoint did not answer in".into(),
            "    time".into(),
            String::new(),
            "  ● ChatGPT (Codex) · observed on last resp…".into(),
            "    rate limited (HTTP 429) · resets in 1h".into(),
            String::new(),
            "  Spend · local sessions".into(),
            "    Today      $9.50   2.1k tokens  2 calls".into(),
            "    7 days     $9.50   2.1k tokens  2 calls".into(),
            "    30 days   $14.25   3.0k tokens  3 calls".into(),
            "    All time  $20.00  12.0k tokens  7 calls".into(),
            String::new(),
            "    30 days   ····················▅········█".into(),
            "  R refresh · Enter details · Esc close".into(),
            String::new(),
        ]
    );
}

/// The details view is the overview's quota half with each card's age, then the per-model and
/// daily spend in the overview's columns and number formats: the gauge rows match the overview's
/// cell for cell, so opening the details never re-flows the cards.
#[test]
fn the_details_view_reuses_the_overview_cards_at_both_widths() {
    let kb = KeybindingsManager::new();
    let mut page = UsagePage::new(None, None);
    page.apply_quotas(Ok(quota_report()));
    page.apply_spend(Ok(spend_history()));
    assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
    assert_eq!(
        render(&mut page, 80, 70),
        vec![
            rule(80),
            "  Usage › Details".into(),
            "  checked 2m ago".into(),
            String::new(),
            "  Provider quotas".into(),
            "  ● Anthropic · from provider · fetched 2m ago".into(),
            "    5-hour ━━━━━━━━━─────────────── 37% used  resets 14:13 (in 2h 13m)".into(),
            "    Weekly ━━━━━━━━━━━━━━━━━━━━━━━─ 95% used  resets Sat 07:00 (in 2d 19h)".into(),
            String::new(),
            "  ● GitHub Copilot · individual · from provider · fetched 2m ago".into(),
            "    the usage endpoint did not answer in time".into(),
            String::new(),
            "  ● ChatGPT (Codex) · observed on last response · observed 30m ago".into(),
            "    rate limited (HTTP 429) · resets 13:00 (in 1h)".into(),
            String::new(),
            "  Spend by model · last 30 days".into(),
            "    Estimates: cached-token pricing varies; long-context and service tiers are".into(),
            "    not modelled; plan percentages lag up to 5 minutes.".into(),
            "    anthropic/claude-opus  $14.25  3.0k tokens  3 calls  all time $20.00".into(),
            String::new(),
            "  Daily spend · last 30 days".into(),
            "    2026-11-02  $9.50  2.1k tokens  2 calls".into(),
            "    2026-11-01  $0.00     0 tokens  0 calls".into(),
            "    2026-11-28  $0.00     0 tokens  0 calls".into(),
            "    2026-11-27  $0.00     0 tokens  0 calls".into(),
            "    2026-11-26  $0.00     0 tokens  0 calls".into(),
            "    2026-11-25  $0.00     0 tokens  0 calls".into(),
            "    2026-11-24  $0.00     0 tokens  0 calls".into(),
            "    2026-11-23  $0.00     0 tokens  0 calls".into(),
            "    2026-11-22  $0.00     0 tokens  0 calls".into(),
            "    2026-11-21  $4.75   900 tokens  1 call".into(),
            "    2026-11-20  $0.00     0 tokens  0 calls".into(),
            "    2026-11-19  $0.00     0 tokens  0 calls".into(),
            "    2026-11-18  $0.00     0 tokens  0 calls".into(),
            "    2026-11-17  $0.00     0 tokens  0 calls".into(),
            "    2026-11-16  $0.00     0 tokens  0 calls".into(),
            "    2026-11-15  $0.00     0 tokens  0 calls".into(),
            "    2026-11-14  $0.00     0 tokens  0 calls".into(),
            "    2026-11-13  $0.00     0 tokens  0 calls".into(),
            "    2026-11-12  $0.00     0 tokens  0 calls".into(),
            "    2026-11-11  $0.00     0 tokens  0 calls".into(),
            "    2026-11-10  $0.00     0 tokens  0 calls".into(),
            "    2026-11-09  $0.00     0 tokens  0 calls".into(),
            "    2026-11-08  $0.00     0 tokens  0 calls".into(),
            "    2026-11-07  $0.00     0 tokens  0 calls".into(),
            "    2026-11-06  $0.00     0 tokens  0 calls".into(),
            "    2026-11-05  $0.00     0 tokens  0 calls".into(),
            "    2026-11-04  $0.00     0 tokens  0 calls".into(),
            "    2026-11-03  $0.00     0 tokens  0 calls".into(),
            "    2026-11-02  $0.00     0 tokens  0 calls".into(),
            "    2026-11-01  $0.00     0 tokens  0 calls".into(),
            "  R refresh · Tab overview · Esc back".into(),
            String::new(),
        ]
    );
    assert_eq!(
        render(&mut page, 60, 70),
        vec![
            rule(60),
            "  Usage › Details".into(),
            "  checked 2m ago".into(),
            String::new(),
            "  Provider quotas".into(),
            "  ● Anthropic · from provider · fetched 2m ago".into(),
            "    5-hour ━━━━────── 37% used  resets 14:13 (in 2h 13m)".into(),
            "    Weekly ━━━━━━━━━━ 95% used  resets Sat 07:00 (in 2d 19h)".into(),
            String::new(),
            "  ● GitHub Copilot · individual · fetched 2m ago".into(),
            "    the usage endpoint did not answer in time".into(),
            String::new(),
            "  ● ChatGPT (Codex) · observed 30m ago".into(),
            "    rate limited (HTTP 429) · resets 13:00 (in 1h)".into(),
            String::new(),
            "  Spend by model · last 30 days".into(),
            "    Estimates: cached-token pricing varies; long-context and".into(),
            "    service tiers are not modelled; plan percentages lag up".into(),
            "    to 5 minutes.".into(),
            "    anthropic/claude-opus  $14.25  3.0k tokens  3 calls".into(),
            String::new(),
            "  Daily spend · last 30 days".into(),
            "    2026-11-02  $9.50  2.1k tokens  2 calls".into(),
            "    2026-11-01  $0.00     0 tokens  0 calls".into(),
            "    2026-11-28  $0.00     0 tokens  0 calls".into(),
            "    2026-11-27  $0.00     0 tokens  0 calls".into(),
            "    2026-11-26  $0.00     0 tokens  0 calls".into(),
            "    2026-11-25  $0.00     0 tokens  0 calls".into(),
            "    2026-11-24  $0.00     0 tokens  0 calls".into(),
            "    2026-11-23  $0.00     0 tokens  0 calls".into(),
            "    2026-11-22  $0.00     0 tokens  0 calls".into(),
            "    2026-11-21  $4.75   900 tokens  1 call".into(),
            "    2026-11-20  $0.00     0 tokens  0 calls".into(),
            "    2026-11-19  $0.00     0 tokens  0 calls".into(),
            "    2026-11-18  $0.00     0 tokens  0 calls".into(),
            "    2026-11-17  $0.00     0 tokens  0 calls".into(),
            "    2026-11-16  $0.00     0 tokens  0 calls".into(),
            "    2026-11-15  $0.00     0 tokens  0 calls".into(),
            "    2026-11-14  $0.00     0 tokens  0 calls".into(),
            "    2026-11-13  $0.00     0 tokens  0 calls".into(),
            "    2026-11-12  $0.00     0 tokens  0 calls".into(),
            "    2026-11-11  $0.00     0 tokens  0 calls".into(),
            "    2026-11-10  $0.00     0 tokens  0 calls".into(),
            "    2026-11-09  $0.00     0 tokens  0 calls".into(),
            "    2026-11-08  $0.00     0 tokens  0 calls".into(),
            "    2026-11-07  $0.00     0 tokens  0 calls".into(),
            "    2026-11-06  $0.00     0 tokens  0 calls".into(),
            "    2026-11-05  $0.00     0 tokens  0 calls".into(),
            "    2026-11-04  $0.00     0 tokens  0 calls".into(),
            "    2026-11-03  $0.00     0 tokens  0 calls".into(),
            "    2026-11-02  $0.00     0 tokens  0 calls".into(),
            "    2026-11-01  $0.00     0 tokens  0 calls".into(),
            "  R refresh · Tab overview · Esc back".into(),
            String::new(),
        ]
    );
}

/// Spend the session files stamped with billing modes splits into the billed share, the plan's
/// API-equivalent value (never billed), and free or unpriced calls, in the span rows' columns;
/// the model rows name each model's mode, and the page close counts the calls per share.
#[test]
fn the_spend_half_splits_billed_plan_and_free_calls() {
    use pa_types::billing::{BillingMode, BillingSplit, CostSource, ModeSpend};
    let split = BillingSplit {
        pay_as_you_go: ModeSpend {
            calls: 1,
            cost: 0.4,
        },
        plan: ModeSpend {
            calls: 2,
            cost: 13.85,
        },
        free: ModeSpend {
            calls: 3,
            cost: 0.0,
        },
        ..BillingSplit::default()
    };
    let mut history = spend_history();
    history.last_30_days.billing = split;
    history.models[0].mode = Some(BillingMode::Plan {
        provider_quota: true,
    });
    history.models.push(ModelSpend {
        provider: "openrouter".into(),
        model: "glm".into(),
        mode: Some(BillingMode::PayAsYouGo {
            source: CostSource::ProviderReported,
        }),
        today: spend_totals(0.4, 100, 1),
        last_7_days: spend_totals(0.4, 100, 1),
        last_30_days: spend_totals(0.4, 100, 1),
        all_time: spend_totals(0.4, 100, 1),
    });
    let mut page = UsagePage::new(None, None);
    page.apply_quotas(Ok(quota_report()));
    page.apply_spend(Ok(history));
    let overview = render(&mut page, 80, 40);
    let spend_at = overview
        .iter()
        .position(|row| row == "  Spend · local sessions")
        .expect("the spend half");
    assert_eq!(
        overview[spend_at..overview.len() - 2].to_vec(),
        vec![
            "  Spend · local sessions".to_string(),
            "    Today               $9.50   2.1k tokens  2 calls".into(),
            "    7 days              $9.50   2.1k tokens  2 calls".into(),
            "    30 days            $14.25   3.0k tokens  3 calls".into(),
            "    All time           $20.00  12.0k tokens  7 calls".into(),
            String::new(),
            "    30 days by billing".into(),
            "    Billed              $0.40                1 call   pay-as-you-go".into(),
            "    Plan (not billed)  $13.85                2 calls  API-equivalent value".into(),
            "    Free / unpriced                          3 calls".into(),
            String::new(),
            "    30 days            ····················▅········█".into(),
        ]
    );
    let kb = KeybindingsManager::new();
    assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
    let details = render(&mut page, 100, 70);
    let models_at = details
        .iter()
        .position(|row| row == "  Spend by model · last 30 days")
        .expect("the model rows");
    assert_eq!(
        details[models_at + 3..models_at + 5].to_vec(),
        vec![
            "    anthropic/claude-opus  $14.25  3.0k tokens  3 calls  all time $20.00 · plan, not billed"
                .to_string(),
            "    openrouter/glm          $0.40   100 tokens  1 call   all time $0.40 · billed".into(),
        ]
    );
    assert_eq!(
        (
            page.summary().billed_call_count,
            page.summary().plan_call_count,
            page.summary().free_call_count,
        ),
        (1, 2, 3)
    );
}

/// A failed half keeps the other: the quota half's raw error, wrapped whole, beside an empty
/// history; the details view shows the same states instead of empty tables.
#[test]
fn failed_and_empty_halves_render_their_own_states() {
    let mut page = UsagePage::new(None, None);
    page.apply_quotas(Err("the daemon did not answer in time".into()));
    page.apply_spend(Ok(SpendHistory {
        today: SpendTotals::default(),
        last_7_days: SpendTotals::default(),
        last_30_days: SpendTotals::default(),
        all_time: SpendTotals::default(),
        daily: Vec::new(),
        models: Vec::new(),
        rate_limits: Vec::new(),
        sessions: 0,
    }));
    let frame =
        |rows: &[&str]| -> Vec<String> { rows.iter().map(|row| (*row).to_string()).collect() };
    assert_eq!(
        render(&mut page, 80, 30)[4..9].to_vec(),
        frame(&[
            "  Provider quotas",
            "    Provider quotas unavailable (the daemon did not answer in time).",
            "",
            "  Spend · local sessions",
            "    No billed model calls in local sessions yet.",
        ])
    );
    assert_eq!(
        render(&mut page, 44, 30)[4..11].to_vec(),
        frame(&[
            "  Provider quotas",
            "    Provider quotas unavailable (the daemon",
            "    did not answer in time).",
            "",
            "  Spend · local sessions",
            "    No billed model calls in local sessions",
            "    yet.",
        ])
    );
    let kb = KeybindingsManager::new();
    assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
    assert_eq!(
        render(&mut page, 80, 30)[4..9].to_vec(),
        frame(&[
            "  Provider quotas",
            "    Provider quotas unavailable (the daemon did not answer in time).",
            "",
            "  Spend by model · last 30 days",
            "    No billed model calls in local sessions yet.",
        ])
    );
    assert_eq!(page.handle_key("escape", &kb), UsagePageAction::None);
    page.apply_quotas(Ok(QuotaReport {
        providers: Vec::new(),
        pacing: Vec::new(),
    }));
    assert_eq!(
        render(&mut page, 80, 30)[5],
        "    No subscription sign-in reports quotas (/login adds one)."
    );
}

/// The daemon's live pacing rides the quota half as one read-only row per provider/model, at
/// the content column with the label column shared.
#[test]
fn the_quota_half_lists_the_live_request_pacing() {
    let mut page = UsagePage::new(None, Some(spend_history()));
    page.apply_quotas(Ok(QuotaReport {
        providers: Vec::new(),
        pacing: vec![pa_types::provider_usage::ProviderPacing {
            provider: "anthropic".into(),
            model: "claude-x".into(),
            concurrency: 3,
            rate_per_min: 50,
            concurrency_ceiling: Some(6),
            rate_ceiling: Some(50),
            in_flight: 2,
            reason: pa_types::control::AdjustReason::Throttled,
        }],
    }));
    let rendered = render(&mut page, 80, 30);
    let at = rendered
        .iter()
        .position(|row| row == "  Request pacing")
        .expect("the pacing section");
    assert_eq!(
        rendered[at + 1],
        "    anthropic/claude-x  2/3 in flight · ceiling 6 · 50/min · backed off (429)"
    );
}

/// Enter opens the details; Tab and Esc go back; Esc on the overview closes. Refresh waits out
/// a fetch in flight.
#[test]
fn details_navigation_and_refresh() {
    let kb = KeybindingsManager::new();
    let mut page = UsagePage::new(None, None);
    assert_eq!(page.handle_key("r", &kb), UsagePageAction::None);
    page.apply_quotas(Ok(quota_report()));
    page.apply_spend(Ok(spend_history()));
    assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
    let details = render(&mut page, 80, 70);
    assert_eq!(details[1], "  Usage \u{203a} Details");
    // A short frame windows the details with the scroll row and the scroll hint.
    let short = render(&mut page, 80, 20);
    assert_eq!(
        short[short.len() - 3..].to_vec(),
        vec![
            format!("  (1/{})", details.len() - 6),
            "  ↑/↓ scroll · R refresh · Tab overview · Esc back".to_string(),
            String::new(),
        ]
    );
    assert_eq!(page.handle_key("tab", &kb), UsagePageAction::None);
    assert_eq!(render(&mut page, 80, 40)[1], "  Usage");
    assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
    assert_eq!(page.handle_key("escape", &kb), UsagePageAction::None);
    assert_eq!(render(&mut page, 80, 40)[1], "  Usage");
    assert_eq!(page.handle_key("r", &kb), UsagePageAction::Refresh);
    assert_eq!(render(&mut page, 80, 40)[2], "  refreshing…");
    assert_eq!(page.handle_key("escape", &kb), UsagePageAction::Close);
    assert_eq!(
        page.summary(),
        UsagePageSummary {
            provider_count: 3,
            quotas: UsageSectionOutcome::Partial,
            spend: UsageSectionOutcome::Loaded,
            details_opened: true,
            refresh_count: 1,
            reset_chip_shown_count: 0,
            billed_call_count: 0,
            plan_call_count: 0,
            free_call_count: 0,
        }
    );
}

/// An observed rate limit that lands with the spend half never hides the quota half's loading row.
#[test]
fn a_loading_quota_half_shows_beside_an_observed_card() {
    let mut page = UsagePage::new(None, None);
    page.apply_spend(Ok(spend_history()));
    assert_eq!(
        render(&mut page, 80, 40)[4..9].to_vec(),
        vec![
            "  Provider quotas".to_string(),
            "    Loading provider quotas…".into(),
            String::new(),
            "  ● ChatGPT (Codex) · observed on last response".into(),
            "    rate limited (HTTP 429) · resets 13:00 (in 1h)".into(),
        ]
    );
}

/// Esc and Tab out of the details bring back the overview exactly as it was scrolled; the
/// details themselves open at their top.
#[test]
fn back_from_the_details_restores_the_overview_scroll() {
    let kb = KeybindingsManager::new();
    let mut page = UsagePage::new(Some(quota_report()), Some(spend_history()));
    page.apply_quotas(Ok(quota_report()));
    page.apply_spend(Ok(spend_history()));
    render(&mut page, 80, 16);
    for _ in 0..3 {
        assert_eq!(page.handle_key("down", &kb), UsagePageAction::None);
    }
    let scrolled = render(&mut page, 80, 16);
    for back in ["escape", "tab"] {
        assert_eq!(page.handle_key("enter", &kb), UsagePageAction::None);
        let details = render(&mut page, 80, 16);
        assert_eq!(
            details[4], "  Provider quotas",
            "the details open at their top"
        );
        assert_eq!(page.handle_key(back, &kb), UsagePageAction::None);
        assert_eq!(render(&mut page, 80, 16), scrolled, "back with {back}");
    }
}
