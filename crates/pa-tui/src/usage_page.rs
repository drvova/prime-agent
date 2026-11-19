//! The `/usage` page: per-provider subscription quotas with when each limit resets, over the
//! local spend history. It mounts over the editor dock like the info panel and opens at once
//! over the last fetched data (or the loading rows); the daemon's `get_usage` halves fill it in
//! as they land, each half failing on its own. The countdowns repaint only when their text
//! changes: [`UsagePage::next_repaint_at_ms`] arms the run loop's frame deadline.

mod cards;
mod format;

#[cfg(test)]
mod tests;

use pa_types::provider_usage::{QuotaReport, QuotaState, SpendHistory};

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{title_row, EscVerb, Footer, HintRank};
use crate::menu_panel::{menu_list_layout, scroll_row};
use crate::theme::{Theme, ThemeColor};
use crate::Line;

pub(crate) use cards::CivilClock;
pub(crate) use format::{countdown, next_label_change};

/// The frame's rows outside the content window: the rule, the title, the status row, the blank
/// under it, the footer, and the trailing blank.
const FIXED_FRAME_ROWS: usize = 6;
const SCROLL_INDICATOR_ROWS: usize = 1;

/// The wall clock in epoch ms: the countdowns' `now`.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// One half of the page: the last data, whether a fetch is in flight, and the last failure.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Section<T> {
    pub data: Option<T>,
    pub loading: bool,
    pub error: Option<String>,
}

impl<T> Section<T> {
    pub(crate) fn loading(data: Option<T>) -> Self {
        Self {
            data,
            loading: true,
            error: None,
        }
    }

    /// Fold a landed fetch: success replaces the data, a failure keeps the last data.
    pub(crate) fn land(&mut self, result: Result<T, String>) {
        self.loading = false;
        match result {
            Ok(data) => {
                self.data = Some(data);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

/// Which view the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageView {
    Overview,
    Details,
}

/// The outcome of one key press on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePageAction {
    /// Consumed (scrolling, a view switch, an inert key).
    None,
    /// Re-fetch both halves, bypassing the quota cache.
    Refresh,
    /// Close the page.
    Close,
}

/// How one half of the page ended up, for the adoption event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSectionOutcome {
    /// Every card (or the history) loaded.
    Loaded,
    /// Some quota cards failed, others loaded.
    Partial,
    /// The half failed, or every card did.
    Failed,
    /// Loaded with nothing to show (no subscription sign-in).
    Empty,
    /// The page closed before the half landed.
    Loading,
}

impl UsageSectionOutcome {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            UsageSectionOutcome::Loaded => "loaded",
            UsageSectionOutcome::Partial => "partial",
            UsageSectionOutcome::Failed => "failed",
            UsageSectionOutcome::Empty => "empty",
            UsageSectionOutcome::Loading => "loading",
        }
    }
}

/// The adoption event's primitives (`tui usage page closed`): never an account, an amount, or a
/// URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsagePageSummary {
    pub provider_count: u64,
    pub quotas: UsageSectionOutcome,
    pub spend: UsageSectionOutcome,
    pub details_opened: bool,
    pub refresh_count: u64,
    /// Rate-limit reset chips the tray showed since the last page close.
    pub reset_chip_shown_count: u64,
    /// The shown 30 days' recorded calls by billing share: billed (pay-as-you-go and
    /// unclassified), plan (and flat rate), free or unpriced.
    pub billed_call_count: u64,
    pub plan_call_count: u64,
    pub free_call_count: u64,
}

/// The `/usage` page.
#[derive(Debug, Clone, PartialEq)]
pub struct UsagePage {
    view: UsageView,
    scroll: usize,
    /// The content window's height at the last paint (the scroll keys walk the painted window).
    visible_rows: usize,
    quotas: Section<QuotaReport>,
    spend: Section<SpendHistory>,
    /// When a countdown (or the `checked` age) next changes its text, as of the last paint.
    next_repaint_at_ms: Option<i64>,
    details_opened: bool,
    refresh_count: u64,
}

impl UsagePage {
    /// Open over the last fetched halves (stale-while-revalidate); both halves start loading.
    #[must_use]
    pub fn new(quotas: Option<QuotaReport>, spend: Option<SpendHistory>) -> Self {
        Self {
            view: UsageView::Overview,
            scroll: 0,
            visible_rows: 0,
            quotas: Section::loading(quotas),
            spend: Section::loading(spend),
            next_repaint_at_ms: None,
            details_opened: false,
            refresh_count: 0,
        }
    }

    /// A landed quota fetch.
    pub fn apply_quotas(&mut self, result: Result<QuotaReport, String>) {
        self.quotas.land(result);
    }

    /// A landed spend scan.
    pub fn apply_spend(&mut self, result: Result<SpendHistory, String>) {
        self.spend.land(result);
    }

    /// Whether a quota fetch is still in flight (the refresh key waits it out).
    #[must_use]
    pub fn quotas_loading(&self) -> bool {
        self.quotas.loading
    }

    /// One key press. The refresh key starts a refresh unless one is running.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> UsagePageAction {
        if key == "ctrl+c" || kb.matches(key, "app.clear") {
            return UsagePageAction::Close;
        }
        if kb.matches(key, "tui.select.cancel") || kb.matches(key, "app.modal.back") {
            return match self.view {
                UsageView::Overview => UsagePageAction::Close,
                UsageView::Details => {
                    self.switch_view(UsageView::Overview);
                    UsagePageAction::None
                }
            };
        }
        if kb.matches(key, "app.usage.refresh") {
            if self.quotas.loading || self.spend.loading {
                return UsagePageAction::None;
            }
            self.quotas.loading = true;
            self.spend.loading = true;
            self.refresh_count += 1;
            return UsagePageAction::Refresh;
        }
        if kb.matches(key, "tui.select.confirm") && self.view == UsageView::Overview {
            self.switch_view(UsageView::Details);
            return UsagePageAction::None;
        }
        if kb.matches(key, "app.usage.overview") {
            let next = match self.view {
                UsageView::Overview => UsageView::Details,
                UsageView::Details => UsageView::Overview,
            };
            self.switch_view(next);
            return UsagePageAction::None;
        }
        let page = self.visible_rows.max(1) as isize;
        let delta = if kb.matches(key, "tui.select.up") {
            -1
        } else if kb.matches(key, "tui.select.down") {
            1
        } else if kb.matches(key, "tui.select.pageUp") {
            -page
        } else if kb.matches(key, "tui.select.pageDown") {
            page
        } else if kb.matches(key, "tui.select.top") {
            isize::MIN / 2
        } else if kb.matches(key, "tui.select.bottom") {
            isize::MAX / 2
        } else {
            0
        };
        // The bottom clamp runs at paint time, where the content height is known.
        self.scroll = (self.scroll as isize).saturating_add(delta).max(0) as usize;
        UsagePageAction::None
    }

    fn switch_view(&mut self, view: UsageView) {
        if view == UsageView::Details {
            self.details_opened = true;
        }
        self.view = view;
        self.scroll = 0;
    }

    /// When the painted text next changes (a countdown's unit, the `checked` age): the run
    /// loop's frame deadline while the page is open (already passed means repaint now).
    #[must_use]
    pub fn repaint_deadline(&self) -> Option<std::time::Instant> {
        let wait = self.next_repaint_at_ms?.saturating_sub(now_ms()).max(0);
        Some(std::time::Instant::now() + std::time::Duration::from_millis(wait.unsigned_abs()))
    }

    /// The adoption summary for the close.
    #[must_use]
    pub fn summary(&self) -> UsagePageSummary {
        let cards = cards::cards(self.quotas.data.as_ref(), self.spend.data.as_ref());
        let failed = cards
            .iter()
            .filter(|card| matches!(card.state, QuotaState::Failed { .. }))
            .count();
        let quotas = match (&self.quotas.data, self.quotas.loading, &self.quotas.error) {
            (None, true, _) => UsageSectionOutcome::Loading,
            (None, false, Some(_)) => UsageSectionOutcome::Failed,
            (Some(_) | None, _, _) if cards.is_empty() => UsageSectionOutcome::Empty,
            (Some(_) | None, _, _) if failed == cards.len() => UsageSectionOutcome::Failed,
            (Some(_) | None, _, _) if failed > 0 => UsageSectionOutcome::Partial,
            (Some(_) | None, _, _) => UsageSectionOutcome::Loaded,
        };
        let spend = match (&self.spend.data, self.spend.loading) {
            (Some(history), _) if history.all_time.requests == 0 => UsageSectionOutcome::Empty,
            (Some(_), _) => UsageSectionOutcome::Loaded,
            (None, true) => UsageSectionOutcome::Loading,
            (None, false) => UsageSectionOutcome::Failed,
        };
        let split = self
            .spend
            .data
            .as_ref()
            .map(|history| history.last_30_days.billing)
            .unwrap_or_default();
        UsagePageSummary {
            provider_count: cards.len() as u64,
            quotas,
            spend,
            details_opened: self.details_opened,
            refresh_count: self.refresh_count,
            reset_chip_shown_count: 0,
            billed_call_count: split.pay_as_you_go.calls + split.unclassified.calls,
            plan_call_count: split.plan.calls + split.flat_rate.calls,
            free_call_count: split.free.calls + split.unknown.calls,
        }
    }

    /// The status row under the title: the refresh in flight, the last failure over kept data,
    /// or how old the quota numbers are.
    fn status_text(&self, now_ms: i64) -> (ThemeColor, String) {
        let checked = self
            .quotas
            .data
            .as_ref()
            .and_then(|report| report.providers.iter().map(|card| card.fetched_at_ms).max());
        match (self.quotas.loading, &self.quotas.error, checked) {
            (true, _, Some(_)) => (ThemeColor::Muted, "refreshing…".to_string()),
            (false, Some(error), Some(_)) => {
                (ThemeColor::Warning, format!("refresh failed: {error}"))
            }
            (false, None, Some(at)) => (
                ThemeColor::Dim,
                format!("checked {} ago", format::countdown(now_ms - at)),
            ),
            (true | false, _, None) => (ThemeColor::Dim, String::new()),
        }
    }

    /// Render the frame at the current row budget and record when its text next changes.
    pub fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        viewport_rows: usize,
        now_ms: i64,
        civil: CivilClock<'_>,
    ) -> Vec<Line> {
        let content = match self.view {
            UsageView::Overview => {
                cards::overview_rows(theme, width, &self.quotas, &self.spend, now_ms, civil)
            }
            UsageView::Details => {
                cards::detail_rows(theme, width, &self.quotas, &self.spend, now_ms, civil)
            }
        };
        let total = content.len();
        let visible = menu_list_layout(
            Some(viewport_rows),
            viewport_rows,
            total,
            FIXED_FRAME_ROWS,
            SCROLL_INDICATOR_ROWS,
        );
        self.visible_rows = visible;
        self.scroll = self.scroll.min(total.saturating_sub(visible.max(1)));
        let end = (self.scroll + visible).min(total);
        let title = match self.view {
            UsageView::Overview => "Usage",
            UsageView::Details => "Usage · Details",
        };
        let (status_color, status) = self.status_text(now_ms);
        let mut lines: Vec<Line> = Vec::with_capacity(FIXED_FRAME_ROWS + visible + 1);
        lines.push(vec![
            theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(width.max(1)))
        ]);
        lines.push(title_row(theme, width, title));
        lines.push(crate::width::truncate_line(
            &vec![
                crate::Span::raw(crate::menu_chrome::CHROME_INDENT),
                theme.fg_span(status_color, status),
            ],
            width,
            "…",
        ));
        lines.push(Vec::new());
        lines.extend(content[self.scroll..end].iter().cloned());
        if end < total || self.scroll > 0 {
            lines.push(scroll_row(theme, width, self.scroll + 1, total));
        }
        let scrollable = total > visible;
        let footer = Footer::new();
        let footer = if scrollable {
            footer.bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "scroll",
                HintRank::Extra,
            )
        } else {
            footer
        }
        .bound(kb, &["app.usage.refresh"], "refresh", HintRank::Useful);
        let footer = match self.view {
            UsageView::Overview => footer
                .bound(kb, &["tui.select.confirm"], "details", HintRank::Core)
                .esc(kb, EscVerb::Close),
            UsageView::Details => footer
                .bound(kb, &["app.usage.overview"], "overview", HintRank::Useful)
                .esc(kb, EscVerb::Back),
        };
        lines.push(footer.line(theme, width));
        lines.push(Vec::new());
        lines.truncate(viewport_rows.max(1));
        self.next_repaint_at_ms = self.next_change(now_ms);
        lines
    }

    /// The earliest instant a painted countdown or the `checked` age changes its text.
    fn next_change(&self, now_ms: i64) -> Option<i64> {
        let cards = cards::cards(self.quotas.data.as_ref(), self.spend.data.as_ref());
        let resets = cards.iter().flat_map(|card| match &card.state {
            QuotaState::Windows { windows } => windows
                .iter()
                .filter_map(|window| window.resets_at_ms)
                .collect(),
            QuotaState::RateLimited { resets_at_ms } => resets_at_ms.iter().copied().collect(),
            QuotaState::Failed { .. } => Vec::new(),
        });
        let ages = cards.iter().map(|card| card.fetched_at_ms);
        resets
            .chain(ages)
            .map(|at| format::next_label_change(now_ms, at))
            .min()
    }
}
